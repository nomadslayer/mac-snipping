//! Screen capture: frozen per-display screenshots (ScreenCaptureKit), the
//! on-screen window list, and helpers for cropping / compositing.

use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2_app_kit::NSScreen;
use objc2_core_foundation::{CFArray, CFRetained, CGPoint, CGRect};
use objc2_core_graphics::*;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::*;

use crate::util::{mtm, ns, on_main, rect, rect_integral, rect_intersection, Rgba};

#[derive(Clone, Copy, Debug)]
pub struct ScreenInfo {
    pub display_id: u32,
    /// Cocoa global coordinates (points, bottom-left origin of the primary display).
    pub frame: CGRect,
    pub scale: f64,
}

pub struct DisplayShot {
    pub info: ScreenInfo,
    pub image: CFRetained<CGImage>,
}

impl DisplayShot {
    pub fn px_scale(&self) -> (f64, f64) {
        (
            CGImage::width(Some(&self.image)) as f64 / self.info.frame.size.width,
            CGImage::height(Some(&self.image)) as f64 / self.info.frame.size.height,
        )
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct WinInfo {
    pub id: u32,
    /// Cocoa global coordinates.
    pub frame: CGRect,
    pub owner: String,
    pub title: String,
}

pub struct Frozen {
    pub displays: Vec<DisplayShot>,
    pub windows: Vec<WinInfo>,
}

pub fn has_permission() -> bool {
    CGPreflightScreenCaptureAccess()
}

pub fn request_permission() -> bool {
    CGRequestScreenCaptureAccess()
}

pub fn screens() -> Vec<ScreenInfo> {
    let mut out = Vec::new();
    for screen in NSScreen::screens(mtm()).iter() {
        let desc = screen.deviceDescription();
        let id = desc
            .objectForKey(&*ns("NSScreenNumber"))
            .and_then(|o| o.downcast::<NSNumber>().ok())
            .map(|n| n.unsignedIntValue())
            .unwrap_or(0);
        out.push(ScreenInfo {
            display_id: id,
            frame: screen.frame(),
            scale: screen.backingScaleFactor(),
        });
    }
    out
}

/// Height of the primary display, used to flip between CG (top-left) and
/// Cocoa (bottom-left) global coordinates.
pub fn primary_height() -> f64 {
    NSScreen::screens(mtm())
        .iter()
        .next()
        .map(|s| s.frame().size.height)
        .unwrap_or(0.0)
}

pub fn cg_to_cocoa(r: CGRect, primary_h: f64) -> CGRect {
    rect(r.origin.x, primary_h - (r.origin.y + r.size.height), r.size.width, r.size.height)
}

#[derive(Clone, Debug)]
pub struct CaptureError {
    pub message: String,
    /// Screen Recording permission is missing or stale for this build.
    pub permission: bool,
}

/// SCStreamErrorUserDeclined: the user (or TCC) declined screen capture.
const SC_USER_DECLINED: isize = -3801;

fn capture_error(err: *mut NSError) -> CaptureError {
    if err.is_null() {
        return CaptureError { message: "unknown error".into(), permission: !has_permission() };
    }
    let (message, code) = unsafe { ((*err).localizedDescription().to_string(), (*err).code()) };
    crate::util::log(&format!("capture error {code}: {message}"));
    CaptureError { message, permission: code == SC_USER_DECLINED || !has_permission() }
}


/// Capture every display at native resolution. `done` runs on the main thread.
pub fn capture_displays(
    infos: Vec<ScreenInfo>,
    done: impl FnOnce(Result<Vec<DisplayShot>, CaptureError>) + 'static,
) {
    struct Pending {
        remaining: usize,
        shots: Vec<DisplayShot>,
        error: Option<CaptureError>,
    }
    type Done = Box<dyn FnOnce(Result<Vec<DisplayShot>, CaptureError>)>;
    let done: Arc<Mutex<Option<crate::util::SendBox<Done>>>> =
        Arc::new(Mutex::new(Some(crate::util::SendBox(Box::new(done)))));

    let finish = {
        let done = done.clone();
        move |res: Result<Vec<DisplayShot>, CaptureError>| {
            if let Some(cb) = done.lock().unwrap().take() {
                let res = crate::util::SendBox(res);
                on_main(move || (cb.into_inner())(res.into_inner()));
            }
        }
    };

    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let Some(content) = (unsafe { content.as_ref() }) else {
            finish(Err(capture_error(err)));
            return;
        };
        let displays = unsafe { content.displays() };
        let order: Vec<u32> = infos.iter().map(|i| i.display_id).collect();
        let pending = Arc::new(Mutex::new(Pending {
            remaining: infos.len(),
            shots: Vec::new(),
            error: None,
        }));
        if infos.is_empty() {
            finish(Err(CaptureError { message: "No displays found".into(), permission: false }));
            return;
        }
        for info in infos.iter().copied() {
            let display = displays
                .iter()
                .find(|d| unsafe { d.displayID() } == info.display_id);
            let Some(display) = display else {
                let mut p = pending.lock().unwrap();
                p.remaining -= 1;
                if p.remaining == 0 {
                    let shots = std::mem::take(&mut p.shots);
                    drop(p);
                    finish(if shots.is_empty() {
                        Err(CaptureError { message: "Display not shareable".into(), permission: false })
                    } else {
                        Ok(shots)
                    });
                }
                continue;
            };
            unsafe {
                let filter = SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::new(),
                );
                let cfg = SCStreamConfiguration::new();
                cfg.setWidth((info.frame.size.width * info.scale).round() as usize);
                cfg.setHeight((info.frame.size.height * info.scale).round() as usize);
                cfg.setShowsCursor(false);
                cfg.setCaptureResolution(SCCaptureResolutionType::Best);
                let pending = pending.clone();
                let finish = finish.clone();
                let order = order.clone();
                let handler = RcBlock::new(move |img: *mut CGImage, err: *mut NSError| {
                    let mut p = pending.lock().unwrap();
                    match NonNull::new(img) {
                        Some(img) => p.shots.push(DisplayShot {
                            info,
                            image: CFRetained::retain(img),
                        }),
                        None => p.error = Some(capture_error(err)),
                    }
                    p.remaining -= 1;
                    if p.remaining == 0 {
                        let mut shots = std::mem::take(&mut p.shots);
                        // Completion order is arbitrary; keep screen order.
                        shots.sort_by_key(|s| order.iter().position(|id| *id == s.info.display_id).unwrap_or(usize::MAX));
                        let error = p.error.take();
                        drop(p);
                        if shots.is_empty() {
                            finish(Err(error.unwrap_or_else(|| CaptureError { message: "Capture failed".into(), permission: false })));
                        } else {
                            finish(Ok(shots));
                        }
                    }
                });
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &cfg,
                    Some(&handler),
                );
            }
        }
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
}

/// All normal on-screen windows, front to back, excluding our own.
pub fn window_list() -> Vec<WinInfo> {
    let primary_h = primary_height();
    let my_pid = std::process::id() as i64;
    let Some(arr) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        0,
    ) else {
        return Vec::new();
    };
    // CFArray<CFDictionary> is toll-free bridged to NSArray<NSDictionary>.
    let arr: &NSArray<NSDictionary> =
        unsafe { &*(&*arr as *const CFArray as *const NSArray<NSDictionary>) };
    let num = |d: &NSDictionary, k: &str| -> Option<f64> {
        d.objectForKey(&*ns(k))
            .and_then(|o| o.downcast::<NSNumber>().ok())
            .map(|n| n.doubleValue())
    };
    let string = |d: &NSDictionary, k: &str| -> String {
        d.objectForKey(&*ns(k))
            .and_then(|o| o.downcast::<NSString>().ok())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    for d in arr.iter() {
        let layer = num(&d, "kCGWindowLayer").unwrap_or(0.0) as i64;
        let pid = num(&d, "kCGWindowOwnerPID").unwrap_or(0.0) as i64;
        let alpha = num(&d, "kCGWindowAlpha").unwrap_or(1.0);
        // Our own editor/settings windows are part of the frozen screen too, so
        // they're snippable (overlay windows don't exist yet at this point).
        let _ = (pid, my_pid);
        if !(0..20).contains(&layer) || alpha <= 0.01 {
            continue;
        }
        let Some(bounds) = d
            .objectForKey(&*ns("kCGWindowBounds"))
            .and_then(|o| o.downcast::<NSDictionary>().ok())
        else {
            continue;
        };
        let (x, y, w, h) = (
            num(&bounds, "X").unwrap_or(0.0),
            num(&bounds, "Y").unwrap_or(0.0),
            num(&bounds, "Width").unwrap_or(0.0),
            num(&bounds, "Height").unwrap_or(0.0),
        );
        if w < 24.0 || h < 24.0 {
            continue;
        }
        let owner = string(&d, "kCGWindowOwnerName");
        if owner == "Window Server" || owner == "Dock" {
            continue;
        }
        out.push(WinInfo {
            id: num(&d, "kCGWindowNumber").unwrap_or(0.0) as u32,
            frame: cg_to_cocoa(rect(x, y, w, h), primary_h),
            owner,
            title: string(&d, "kCGWindowName"),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Bitmap helpers
// ---------------------------------------------------------------------------

pub fn srgb() -> CFRetained<CGColorSpace> {
    CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })).expect("sRGB color space")
}

/// A premultiplied BGRA8 bitmap context (native byte order, fastest path).
pub fn bitmap_context(
    w: usize,
    h: usize,
    space: Option<&CGColorSpace>,
) -> Option<CFRetained<CGContext>> {
    let fallback = srgb();
    let space = match space {
        Some(s) if CGColorSpace::model(Some(s)) == CGColorSpaceModel::RGB => s,
        _ => &fallback,
    };
    let info = CGImageAlphaInfo::PremultipliedFirst.0 | CGBitmapInfo::ByteOrder32Little.bits();
    let ctx = unsafe {
        CGBitmapContextCreate(std::ptr::null_mut(), w.max(1), h.max(1), 8, 0, Some(space), info)
    };
    ctx.or_else(|| unsafe {
        CGBitmapContextCreate(std::ptr::null_mut(), w.max(1), h.max(1), 8, 0, Some(&fallback), info)
    })
}

pub fn image_size(img: &CGImage) -> (usize, usize) {
    (CGImage::width(Some(img)), CGImage::height(Some(img)))
}

impl Frozen {
    /// Display containing `p`. Half-open on the right/top so a point on a
    /// shared edge belongs to exactly one display.
    pub fn display_index_at(&self, p: CGPoint) -> Option<usize> {
        let inside = |f: CGRect| {
            p.x >= f.origin.x
                && p.x < f.origin.x + f.size.width
                && p.y >= f.origin.y
                && p.y < f.origin.y + f.size.height
        };
        self.displays
            .iter()
            .position(|d| inside(d.info.frame))
            .or_else(|| self.displays.iter().position(|d| crate::util::rect_contains(d.info.frame, p)))
    }

    /// Crop a rectangle given in Cocoa global coordinates. Returns the image
    /// and its pixel-per-point scale.
    pub fn crop_global(&self, r: CGRect) -> Option<(CFRetained<CGImage>, f64)> {
        let hits: Vec<&DisplayShot> = self
            .displays
            .iter()
            .filter(|d| rect_intersection(d.info.frame, r).is_some())
            .collect();
        if hits.is_empty() {
            return None;
        }
        // Fast path: entirely within one display -> zero-copy sub-image.
        if hits.len() == 1 {
            let d = hits[0];
            if let Some(clip) = rect_intersection(d.info.frame, r) {
                let (sx, sy) = d.px_scale();
                let local_x = clip.origin.x - d.info.frame.origin.x;
                let local_y = clip.origin.y - d.info.frame.origin.y;
                let px = rect_integral(rect(
                    local_x * sx,
                    (d.info.frame.size.height - local_y - clip.size.height) * sy,
                    clip.size.width * sx,
                    clip.size.height * sy,
                ));
                let img = CGImage::with_image_in_rect(Some(&d.image), px)?;
                return Some((img, sx));
            }
        }
        // Composite across displays at the highest scale involved.
        let scale = hits.iter().map(|d| d.px_scale().0).fold(1.0, f64::max);
        let w = (r.size.width * scale).round() as usize;
        let h = (r.size.height * scale).round() as usize;
        let space = CGImage::color_space(Some(&hits[0].image));
        let ctx = bitmap_context(w, h, space.as_deref())?;
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        for d in hits {
            let f = d.info.frame;
            let dst = rect(
                (f.origin.x - r.origin.x) * scale,
                (f.origin.y - r.origin.y) * scale,
                f.size.width * scale,
                f.size.height * scale,
            );
            CGContext::draw_image(Some(&ctx), dst, Some(&d.image));
        }
        let img = CGBitmapContextCreateImage(Some(&ctx))?;
        Some((img, scale))
    }

    /// Union of all displays as one image (Windows "Full screen" snip).
    pub fn full_desktop(&self) -> Option<(CFRetained<CGImage>, f64)> {
        if self.displays.len() == 1 {
            let d = &self.displays[0];
            return Some((d.image.clone(), d.px_scale().0));
        }
        let mut u = self.displays.first()?.info.frame;
        for d in &self.displays[1..] {
            u = crate::util::rect_union(u, d.info.frame);
        }
        self.crop_global(u)
    }

    /// Freeform snip: `pts` are Cocoa global points of a closed lasso.
    pub fn crop_freeform(&self, pts: &[CGPoint]) -> Option<(CFRetained<CGImage>, f64)> {
        if pts.len() < 3 {
            return None;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in pts {
            x0 = x0.min(p.x);
            y0 = y0.min(p.y);
            x1 = x1.max(p.x);
            y1 = y1.max(p.y);
        }
        let bb = rect_integral(rect(x0, y0, x1 - x0, y1 - y0));
        let (img, scale) = self.crop_global(bb)?;
        let (w, h) = image_size(&img);
        let ctx = bitmap_context(w, h, CGImage::color_space(Some(&img)).as_deref())?;
        CGContext::begin_path(Some(&ctx));
        for (i, p) in pts.iter().enumerate() {
            let x = (p.x - bb.origin.x) * scale;
            let y = (p.y - bb.origin.y) * scale;
            if i == 0 {
                CGContext::move_to_point(Some(&ctx), x, y);
            } else {
                CGContext::add_line_to_point(Some(&ctx), x, y);
            }
        }
        CGContext::close_path(Some(&ctx));
        CGContext::clip(Some(&ctx));
        CGContext::draw_image(Some(&ctx), rect(0.0, 0.0, w as f64, h as f64), Some(&img));
        let out = CGBitmapContextCreateImage(Some(&ctx))?;
        Some((out, scale))
    }

    /// sRGB color of the pixel under a Cocoa global point.
    pub fn color_at(&self, p: CGPoint) -> Option<Rgba> {
        let d = &self.displays[self.display_index_at(p)?];
        let (sx, sy) = d.px_scale();
        let (w, h) = image_size(&d.image);
        let lx = ((p.x - d.info.frame.origin.x) * sx).floor().clamp(0.0, w as f64 - 1.0);
        let ly = ((d.info.frame.size.height - (p.y - d.info.frame.origin.y)) * sy).floor().clamp(0.0, h as f64 - 1.0);
        let one = CGImage::with_image_in_rect(Some(&d.image), rect(lx, ly, 1.0, 1.0))?;
        let ctx = bitmap_context(1, 1, Some(&srgb()))?;
        CGContext::draw_image(Some(&ctx), rect(0.0, 0.0, 1.0, 1.0), Some(&one));
        let data = CGBitmapContextGetData(Some(&ctx)) as *const u8;
        if data.is_null() {
            return None;
        }
        let px = unsafe { std::slice::from_raw_parts(data, 4) };
        // BGRA premultiplied; screen pixels are opaque.
        Some(Rgba::new(
            px[2] as f64 / 255.0,
            px[1] as f64 / 255.0,
            px[0] as f64 / 255.0,
            1.0,
        ))
    }
}
