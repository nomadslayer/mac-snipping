//! Recording preview window: playback + Save as / Copy / Share / Show in
//! Finder / Export GIF (like the Windows Snipping Tool's video view).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::MainThreadOnly;
use objc2_app_kit::*;
use objc2_av_foundation::*;
use objc2_av_kit::*;
use objc2_core_foundation::{CFString, CGSize};
use objc2_core_media::{kCMTimeZero, CMTime};
use objc2_foundation::*;
use objc2_image_io::*;

use crate::app::{self, Origin};
use crate::editor::{flex_id, install_toolbar};
use crate::export::{as_cf_dict, cfdict, cfstr_as_ns};
use crate::util::{
    self, background, file_url, hstack, icon_button, mtm, ns, on_main, rect, text_icon_button,
    timestamp_name, SendBox,
};

pub struct VideoWindow {
    window: Retained<NSWindow>,
    _player: Retained<AVPlayer>,
    path: PathBuf,
    saved: RefCell<Option<PathBuf>>,
    share_anchor: RefCell<Option<Retained<NSButton>>>,
}

thread_local! {
    static VIDEOS: RefCell<Vec<Rc<VideoWindow>>> = const { RefCell::new(Vec::new()) };
}

/// The video window that has keyboard focus, if any.
pub fn focused() -> Option<Rc<VideoWindow>> {
    let app = NSApplication::sharedApplication(mtm());
    let key = app.keyWindow()?;
    VIDEOS.with(|v| v.borrow().iter().find(|w| *w.window == *key).cloned())
}

pub fn open(path: PathBuf, saved: Option<PathBuf>) {
    let m = mtm();
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(m),
            rect(0.0, 0.0, 960.0, 600.0),
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns(
        &saved
            .as_ref()
            .unwrap_or(&path)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Screen Recording".into()),
    ));
    window.setToolbarStyle(NSWindowToolbarStyle::Unified);
    window.setContentMinSize(NSSize::new(640.0, 360.0));
    window.setTabbingMode(NSWindowTabbingMode::Disallowed);

    let player = unsafe { AVPlayer::playerWithURL(&file_url(&path), m) };
    let view = unsafe { AVPlayerView::initWithFrame(AVPlayerView::alloc(m), rect(0.0, 0.0, 960.0, 600.0)) };
    unsafe {
        view.setPlayer(Some(&player));
        view.setControlsStyle(AVPlayerViewControlsStyle::Inline);
        view.setShowsFullScreenToggleButton(true);
    }
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    window.setContentView(Some(&view));

    let vw = Rc::new(VideoWindow {
        window: window.clone(),
        _player: player.clone(),
        path,
        saved: RefCell::new(saved),
        share_anchor: RefCell::new(None),
    });
    let w = Rc::downgrade(&vw);

    let new_rec = text_icon_button("New", "video.badge.plus", || app::start_record(Origin::Menu));
    new_rec.setBezelStyle(NSBezelStyle::Toolbar);
    new_rec.setToolTip(Some(&ns("New recording")));
    let mk = |sym: &str, tip: &str, f: fn(&VideoWindow)| {
        let w: Weak<VideoWindow> = w.clone();
        icon_button(sym, tip, move || {
            if let Some(v) = w.upgrade() {
                f(&v);
            }
        })
    };
    let save = mk("square.and.arrow.down", "Save as (⌘S)", VideoWindow::save_as);
    let copy = mk("doc.on.doc", "Copy", VideoWindow::copy);
    let share = mk("square.and.arrow.up", "Share", VideoWindow::share);
    let finder = mk("folder", "Show in Finder", VideoWindow::reveal);
    let gif = mk("photo.on.rectangle.angled", "Export as GIF", VideoWindow::export_gif);
    *vw.share_anchor.borrow_mut() = Some(share.clone());

    let left = hstack(&[&new_rec], 0.0);
    let right = hstack(&[&save, &copy, &share, &finder, &gif], 2.0);
    install_toolbar(
        &window,
        "SnippingVideoToolbar",
        vec![("new", Retained::into_super(left)), ("output", Retained::into_super(right))],
        vec!["new".into(), flex_id(), "output".into()],
    );

    let w2 = Rc::downgrade(&vw);
    util::on_window_close(&window, move || {
        if let Some(v) = w2.upgrade() {
            unsafe { v._player.pause() };
        }
        let w3 = w2.clone();
        util::after(0.0, move || {
            VIDEOS.with(|vs| vs.borrow_mut().retain(|x| !Weak::ptr_eq(&Rc::downgrade(x), &w3)));
            app::window_closed();
        });
    });

    VIDEOS.with(|vs| vs.borrow_mut().push(vw));
    app::ensure_regular();
    window.center();
    window.makeKeyAndOrderFront(None);
    app::activate();
    unsafe { player.play() };
}

impl VideoWindow {
    fn best_path(&self) -> PathBuf {
        self.saved.borrow().clone().unwrap_or_else(|| self.path.clone())
    }

    pub fn save_as(&self) {
        let m = mtm();
        let panel = NSSavePanel::savePanel(m);
        panel.setNameFieldStringValue(&ns(&format!("{}.mp4", timestamp_name("Screen Recording"))));
        panel.setCanCreateDirectories(true);
        if let Some(t) = objc2_uniform_type_identifiers::UTType::typeWithIdentifier(&ns("public.mpeg-4")) {
            panel.setAllowedContentTypes(&NSArray::from_retained_slice(&[t]));
        }
        let dir = crate::settings::recordings_dir();
        let _ = std::fs::create_dir_all(&dir);
        panel.setDirectoryURL(Some(&file_url(&dir)));
        let src = self.best_path();
        let p2 = panel.clone();
        let me = VIDEOS.with(|v| v.borrow().iter().find(|w| *w.window == *self.window).map(Rc::downgrade));
        let block = RcBlock::new(move |resp: NSModalResponse| {
            if resp != NSModalResponseOK {
                return;
            }
            let Some(dest) = p2.URL().and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string())) else { return };
            // Saving onto the source itself would truncate it.
            let same = match (std::fs::canonicalize(&src), std::fs::canonicalize(&dest)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
            if !same {
                // Copy to a temp name first, then move into place.
                let tmp = dest.with_extension("mp4.partial");
                let res = std::fs::copy(&src, &tmp).and_then(|_| std::fs::rename(&tmp, &dest));
                if let Err(e) = res {
                    let _ = std::fs::remove_file(&tmp);
                    util::alert("Couldn't save the recording", &e.to_string());
                    return;
                }
            }
            if let Some(v) = me.as_ref().and_then(|w| w.upgrade()) {
                v.window.setTitle(&ns(&dest.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()));
                *v.saved.borrow_mut() = Some(dest);
            }
        });
        panel.beginSheetModalForWindow_completionHandler(&self.window, &block);
    }

    pub fn copy(&self) {
        crate::export::copy_file(&self.best_path());
    }

    fn share(&self) {
        if let Some(anchor) = self.share_anchor.borrow().as_ref() {
            crate::export::share_file(&self.best_path(), anchor);
        }
    }

    fn reveal(&self) {
        crate::export::reveal_in_finder(&self.best_path());
    }

    fn export_gif(&self) {
        let m = mtm();
        let panel = NSSavePanel::savePanel(m);
        let stem = self
            .best_path()
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Screen Recording".into());
        panel.setNameFieldStringValue(&ns(&format!("{stem}.gif")));
        if let Some(t) = objc2_uniform_type_identifiers::UTType::typeWithIdentifier(&ns("com.compuserve.gif")) {
            panel.setAllowedContentTypes(&NSArray::from_retained_slice(&[t]));
        }
        let src = self.best_path();
        let p2 = panel.clone();
        let win = self.window.clone();
        let block = RcBlock::new(move |resp: NSModalResponse| {
            if resp != NSModalResponseOK {
                return;
            }
            let Some(dest) = p2.URL().and_then(|u| u.path()).map(|p| PathBuf::from(p.to_string())) else { return };
            win.setSubtitle(&ns("Exporting GIF…"));
            let src = src.clone();
            let win2 = SendBox(win.clone());
            background(move || {
                let res = make_gif(&src, &dest);
                let dest2 = dest.clone();
                on_main(move || {
                    let win = win2.into_inner();
                    win.setSubtitle(&ns(""));
                    match res {
                        Ok(()) => crate::toast::show(None, "GIF exported", &dest2.display().to_string(), Some(Box::new(move || crate::export::reveal_in_finder(&dest2)))),
                        Err(e) => util::alert("GIF export failed", &e),
                    }
                });
            });
        });
        panel.beginSheetModalForWindow_completionHandler(&self.window, &block);
    }
}

/// Render the video to an animated GIF (≤ 960 px wide, up to 15 fps).
pub fn make_gif(src: &Path, dest: &Path) -> Result<(), String> {
    unsafe {
        let asset = AVURLAsset::URLAssetWithURL_options(&file_url(src), None);
        let duration = asset.duration().seconds();
        if !duration.is_finite() || duration <= 0.0 {
            return Err("The recording has no frames.".into());
        }
        const MAX_FRAMES: f64 = 900.0;
        let mut fps: f64 = if duration <= 20.0 { 15.0 } else if duration <= 60.0 { 10.0 } else { 6.0 };
        // Long recordings: lower the frame rate instead of cutting the end.
        if duration * fps > MAX_FRAMES {
            fps = MAX_FRAMES / duration;
        }
        let frames = ((duration * fps).ceil() as usize).max(1);
        let gen = AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset);
        gen.setAppliesPreferredTrackTransform(true);
        gen.setMaximumSize(CGSize::new(960.0, 960.0));
        gen.setRequestedTimeToleranceBefore(kCMTimeZero);
        gen.setRequestedTimeToleranceAfter(kCMTimeZero);

        let uti = CFString::from_str("com.compuserve.gif");
        let url = file_url(dest);
        let cfurl: &objc2_core_foundation::CFURL = &*(&*url as *const NSURL as *const objc2_core_foundation::CFURL);
        let Some(dst) = CGImageDestination::with_url(cfurl, &uti, frames, None) else {
            return Err("Could not create the GIF file.".into());
        };
        let loop_inner = cfdict(&[cfstr_as_ns(kCGImagePropertyGIFLoopCount)], &[&NSNumber::new_i32(0)]);
        let file_props = cfdict(&[cfstr_as_ns(kCGImagePropertyGIFDictionary)], &[&loop_inner]);
        dst.set_properties(Some(as_cf_dict(&file_props)));
        let delay = NSNumber::new_f64(1.0 / fps);
        let frame_inner = cfdict(
            &[cfstr_as_ns(kCGImagePropertyGIFDelayTime), cfstr_as_ns(kCGImagePropertyGIFUnclampedDelayTime)],
            &[&delay, &delay],
        );
        let frame_props = cfdict(&[cfstr_as_ns(kCGImagePropertyGIFDictionary)], &[&frame_inner]);
        // The destination expects exactly `frames` images: if a frame can't be
        // decoded (e.g. audio runs past the last video frame) repeat the
        // previous one so the GIF still finalizes.
        let mut last: Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> = None;
        let mut missing_at_start = 0usize;
        let mut added = 0usize;
        for i in 0..frames {
            let t = CMTime::new((i as f64 / fps * 600.0) as i64, 600);
            #[allow(deprecated)]
            let img = gen.copyCGImageAtTime_actualTime_error(t, std::ptr::null_mut()).ok().map(Into::into);
            match img.or_else(|| last.clone()) {
                Some(img) => {
                    // Frames that failed before the first good one are added now.
                    for _ in 0..missing_at_start {
                        dst.add_image(&img, Some(as_cf_dict(&frame_props)));
                        added += 1;
                    }
                    missing_at_start = 0;
                    dst.add_image(&img, Some(as_cf_dict(&frame_props)));
                    added += 1;
                    last = Some(img);
                }
                None => missing_at_start += 1,
            }
        }
        if added == 0 || !dst.finalize() {
            return Err("Could not write the GIF.".into());
        }
    }
    Ok(())
}
