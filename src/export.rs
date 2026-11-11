//! Saving, copying, printing and sharing images.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use block2::RcBlock;
use objc2::rc::{Retained, Weak};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, MainThreadOnly};
use objc2_app_kit::*;
use objc2_core_foundation::{CFDictionary, CFMutableData, CFRetained, CFString};
use objc2_core_graphics::*;
use objc2_foundation::*;
use objc2_image_io::*;
use objc2_uniform_type_identifiers::UTType;

use crate::capture::{bitmap_context, image_size};
use crate::util::{cf_retain, file_url, hstack, label, mtm, ns, rect, temp_dir, unique_path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Heic,
    Tiff,
    Bmp,
}

impl ImageFormat {
    pub const ALL: [ImageFormat; 6] = [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::Heic,
        ImageFormat::Tiff,
        ImageFormat::Bmp,
    ];
    pub fn uti(self) -> &'static str {
        match self {
            ImageFormat::Png => "public.png",
            ImageFormat::Jpeg => "public.jpeg",
            ImageFormat::Gif => "com.compuserve.gif",
            ImageFormat::Heic => "public.heic",
            ImageFormat::Tiff => "public.tiff",
            ImageFormat::Bmp => "com.microsoft.bmp",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Gif => "gif",
            ImageFormat::Heic => "heic",
            ImageFormat::Tiff => "tiff",
            ImageFormat::Bmp => "bmp",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Gif => "GIF",
            ImageFormat::Heic => "HEIC",
            ImageFormat::Tiff => "TIFF",
            ImageFormat::Bmp => "BMP",
        }
    }
    fn opaque(self) -> bool {
        matches!(self, ImageFormat::Jpeg | ImageFormat::Bmp)
    }
    pub fn from_path(p: &Path) -> ImageFormat {
        match p
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .as_deref()
        {
            Some("jpg") | Some("jpeg") => ImageFormat::Jpeg,
            Some("gif") => ImageFormat::Gif,
            Some("heic") => ImageFormat::Heic,
            Some("tif") | Some("tiff") => ImageFormat::Tiff,
            Some("bmp") => ImageFormat::Bmp,
            _ => ImageFormat::Png,
        }
    }
}

thread_local! {
    static LAST_FORMAT: Cell<ImageFormat> = const { Cell::new(ImageFormat::Png) };
}

/// Composite onto white (for formats without alpha).
fn flatten_on_white(img: &CGImage) -> Option<CFRetained<CGImage>> {
    let (w, h) = image_size(img);
    let ctx = bitmap_context(w, h, CGImage::color_space(Some(img)).as_deref())?;
    let c = Some(&*ctx);
    CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(c, rect(0.0, 0.0, w as f64, h as f64));
    CGContext::draw_image(c, rect(0.0, 0.0, w as f64, h as f64), Some(img));
    CGBitmapContextCreateImage(c)
}

pub fn cfdict(keys: &[&NSString], vals: &[&AnyObject]) -> Retained<NSDictionary<NSString, AnyObject>> {
    NSDictionary::from_slices(keys, vals)
}

pub fn as_cf_dict(d: &NSDictionary<NSString, AnyObject>) -> &CFDictionary {
    unsafe { &*(d as *const NSDictionary<NSString, AnyObject> as *const CFDictionary) }
}

pub fn cfstr_as_ns(s: &CFString) -> &NSString {
    unsafe { &*(s as *const CFString as *const NSString) }
}

pub fn encode(img: &CGImage, fmt: ImageFormat) -> Option<Vec<u8>> {
    let image = if fmt.opaque() { flatten_on_white(img)? } else { cf_retain(img) };
    let data = CFMutableData::new(None, 0)?;
    let uti = CFString::from_str(fmt.uti());
    unsafe {
        let dest = CGImageDestination::with_data(&data, &uti, 1, None)?;
        let quality = NSNumber::new_f64(0.92);
        let props = cfdict(
            &[cfstr_as_ns(kCGImageDestinationLossyCompressionQuality)],
            &[&quality],
        );
        let lossy = matches!(fmt, ImageFormat::Jpeg | ImageFormat::Heic);
        dest.add_image(&image, if lossy { Some(as_cf_dict(&props)) } else { None });
        if !dest.finalize() {
            return None;
        }
    }
    Some(data.to_vec())
}

pub fn write_image(img: &CGImage, path: &Path) -> Result<(), String> {
    let fmt = ImageFormat::from_path(path);
    let bytes = encode(img, fmt).ok_or_else(|| format!("Could not encode {}", fmt.title()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

pub fn nsimage(img: &CGImage, scale: f64) -> Retained<NSImage> {
    let (w, h) = image_size(img);
    NSImage::initWithCGImage_size(
        NSImage::alloc(),
        img,
        NSSize::new(w as f64 / scale, h as f64 / scale),
    )
}

/// Copy an image to the clipboard as PNG + TIFF.
pub fn copy_image(img: &CGImage) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let item = NSPasteboardItem::new();
    unsafe {
        if let Some(png) = encode(img, ImageFormat::Png) {
            item.setData_forType(&NSData::with_bytes(&png), NSPasteboardTypePNG);
        }
        let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), img);
        if let Some(tiff) = rep.TIFFRepresentation() {
            item.setData_forType(&tiff, NSPasteboardTypeTIFF);
        }
    }
    let obj = ProtocolObject::from_retained(item);
    pb.writeObjects(&NSArray::from_retained_slice(&[obj]));
}

pub fn copy_text(text: &str) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    unsafe {
        pb.setString_forType(&ns(text), NSPasteboardTypeString);
    }
}

pub fn copy_file(path: &Path) {
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let url = file_url(path);
    let obj = ProtocolObject::from_retained(url);
    pb.writeObjects(&NSArray::from_retained_slice(&[obj]));
}

/// Write a temporary PNG (used for Share / Open with).
pub fn temp_png(img: &CGImage, name: &str) -> Option<PathBuf> {
    let path = unique_path(&temp_dir(), name, "png");
    write_image(img, &path).ok()?;
    Some(path)
}

/// Show a Save As sheet with a format picker.
pub fn save_dialog(
    window: Option<&NSWindow>,
    img: CFRetained<CGImage>,
    default_name: &str,
    directory: Option<&Path>,
    done: impl Fn(Option<PathBuf>) + 'static,
) {
    let m = mtm();
    let panel = NSSavePanel::savePanel(m);
    let fmt = LAST_FORMAT.with(|f| f.get());
    panel.setNameFieldStringValue(&ns(&format!("{default_name}.{}", fmt.ext())));
    panel.setCanCreateDirectories(true);
    panel.setExtensionHidden(false);
    if let Some(dir) = directory {
        let _ = std::fs::create_dir_all(dir);
        panel.setDirectoryURL(Some(&file_url(dir)));
    }
    let set_type = |panel: &NSSavePanel, f: ImageFormat| {
        if let Some(t) = UTType::typeWithIdentifier(&ns(f.uti())) {
            panel.setAllowedContentTypes(&NSArray::from_retained_slice(&[t]));
        }
    };
    set_type(&panel, fmt);

    // Accessory: "Format: [PNG ▾]"
    let popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(m), rect(0.0, 0.0, 120.0, 26.0), false);
    for f in ImageFormat::ALL {
        popup.addItemWithTitle(&ns(f.title()));
    }
    popup.selectItemAtIndex(ImageFormat::ALL.iter().position(|f| *f == fmt).unwrap_or(0) as isize);
    let weak_panel = Weak::from_retained(&panel);
    crate::util::on_action(&popup, move |sender| {
        let Some(panel) = weak_panel.load() else { return };
        let Some(popup) = sender.and_then(|s| s.downcast_ref::<NSPopUpButton>()) else { return };
        let idx = popup.indexOfSelectedItem().max(0) as usize;
        let f = ImageFormat::ALL[idx.min(ImageFormat::ALL.len() - 1)];
        LAST_FORMAT.with(|c| c.set(f));
        if let Some(t) = UTType::typeWithIdentifier(&ns(f.uti())) {
            panel.setAllowedContentTypes(&NSArray::from_retained_slice(&[t]));
        }
    });
    let acc = hstack(&[&label("Format:", 13.0, false), &popup], 8.0);
    acc.setEdgeInsets(NSEdgeInsets { top: 8.0, left: 16.0, bottom: 8.0, right: 16.0 });
    acc.setFrameSize(acc.fittingSize());
    panel.setAccessoryView(Some(&acc));

    let finish = {
        let panel = panel.clone();
        move |resp: NSModalResponse| {
            if resp != NSModalResponseOK {
                done(None);
                return;
            }
            let Some(url) = panel.URL() else {
                done(None);
                return;
            };
            let Some(path) = url.path().map(|p| PathBuf::from(p.to_string())) else {
                done(None);
                return;
            };
            LAST_FORMAT.with(|c| c.set(ImageFormat::from_path(&path)));
            match write_image(&img, &path) {
                Ok(()) => done(Some(path)),
                Err(e) => {
                    crate::util::alert("Couldn't save the snip", &e);
                    done(None);
                }
            }
        }
    };
    match window {
        Some(w) => {
            let block = RcBlock::new(finish);
            panel.beginSheetModalForWindow_completionHandler(w, &block);
        }
        None => {
            let resp = panel.runModal();
            finish(resp);
        }
    }
}

pub fn print_image(img: &CGImage, scale: f64, window: Option<&NSWindow>) {
    let m = mtm();
    let image = nsimage(img, scale);
    let size = image.size();
    let view = NSImageView::imageViewWithImage(&image, m);
    view.setFrame(rect(0.0, 0.0, size.width, size.height));
    view.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    let info = NSPrintInfo::sharedPrintInfo();
    info.setHorizontalPagination(NSPrintingPaginationMode::Fit);
    info.setVerticalPagination(NSPrintingPaginationMode::Fit);
    info.setHorizontallyCentered(true);
    info.setVerticallyCentered(true);
    if size.width > size.height {
        info.setOrientation(NSPaperOrientation::Landscape);
    } else {
        info.setOrientation(NSPaperOrientation::Portrait);
    }
    let op = NSPrintOperation::printOperationWithView_printInfo(&view, &info);
    op.setShowsPrintPanel(true);
    op.setShowsProgressPanel(true);
    match window {
        Some(w) => unsafe {
            op.runOperationModalForWindow_delegate_didRunSelector_contextInfo(
                w,
                None,
                None,
                std::ptr::null_mut(),
            );
        },
        None => {
            op.runOperation();
        }
    }
}

/// Show the macOS share sheet for a file.
pub fn share_file(path: &Path, anchor: &NSView) {
    let url = file_url(path);
    let items: Retained<NSArray> = NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(url))]);
    let picker = unsafe { NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items) };
    picker.showRelativeToRect_ofView_preferredEdge(anchor.bounds(), anchor, NSRectEdge::MinY);
}

pub struct AppChoice {
    pub name: String,
    pub icon: Retained<NSImage>,
    pub url: Retained<NSURL>,
}

/// Applications that can open the given file, default first.
pub fn apps_for(path: &Path) -> Vec<AppChoice> {
    let ws = NSWorkspace::sharedWorkspace();
    let url = file_url(path);
    let default = ws.URLForApplicationToOpenURL(&url);
    let mut out: Vec<AppChoice> = Vec::new();
    let mut push = |app: Retained<NSURL>| {
        let Some(p) = app.path() else { return };
        let name = Path::new(&p.to_string())
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if out.iter().any(|a| a.name == name) || name == "Snipping" {
            return;
        }
        let icon = ws.iconForFile(&p);
        icon.setSize(NSSize::new(16.0, 16.0));
        out.push(AppChoice { name, icon, url: app });
    };
    if let Some(d) = default {
        push(d);
    }
    for app in ws.URLsForApplicationsToOpenURL(&url).iter() {
        push(app);
    }
    out
}

pub fn open_with(path: &Path, app: Option<&NSURL>) {
    let ws = NSWorkspace::sharedWorkspace();
    let url = file_url(path);
    match app {
        Some(app) => {
            let cfg = NSWorkspaceOpenConfiguration::configuration();
            ws.openURLs_withApplicationAtURL_configuration_completionHandler(
                &NSArray::from_retained_slice(&[url]),
                app,
                &cfg,
                None,
            );
        }
        None => {
            ws.openURL(&url);
        }
    }
}

pub fn reveal_in_finder(path: &Path) {
    let ws = NSWorkspace::sharedWorkspace();
    ws.activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[file_url(path)]));
}

pub fn load_image_file(path: &Path) -> Option<(CFRetained<CGImage>, f64)> {
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &ns(&path.to_string_lossy()))?;
    let size = image.size();
    let mut rect_ = rect(0.0, 0.0, size.width, size.height);
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect_, None, None) }?;
    let (w, _) = image_size(&cg);
    let scale = if size.width > 0.0 { (w as f64 / size.width).max(1.0) } else { 1.0 };
    let _ = AnyObject::class;
    Some((cg.into(), scale))
}
