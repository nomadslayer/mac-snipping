//! Small shared helpers: main-thread dispatch, closure-backed Objective-C
//! targets, colors, SF Symbols and a few UI builders.
#![allow(dead_code)]

use std::ffi::c_void;
use std::time::Duration;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::*;
use objc2_core_foundation::{CFRetained, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::CGColor;
use objc2_foundation::*;
use serde::{Deserialize, Serialize};

pub fn mtm() -> MainThreadMarker {
    MainThreadMarker::new().expect("UI code must run on the main thread")
}

pub fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

/// Moves a value across threads. Used only for values that are created and
/// consumed on the main thread but have to ride through a GCD/completion
/// callback that requires `Send`.
pub struct SendBox<T>(pub T);
unsafe impl<T> Send for SendBox<T> {}
impl<T> SendBox<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

/// Run `f` on the main thread (asynchronously).
pub fn on_main<F: FnOnce() + 'static>(f: F) {
    let b = SendBox(f);
    DispatchQueue::main().exec_async(move || (b.into_inner())());
}

/// Run `f` on the main thread after `secs` seconds.
pub fn after<F: FnOnce() + 'static>(secs: f64, f: F) {
    let b = SendBox(f);
    let t = DispatchTime::try_from(Duration::from_secs_f64(secs.max(0.0)))
        .unwrap_or(DispatchTime::NOW);
    let _ = DispatchQueue::main().after(t, move || (b.into_inner())());
}

/// Run `f` on a background queue.
pub fn background<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .name("snipping-worker".into())
        .spawn(f)
        .expect("spawn worker thread");
}

// ---------------------------------------------------------------------------
// Closure-backed target/action
// ---------------------------------------------------------------------------

pub struct ActionIvars {
    f: Box<dyn Fn(Option<&AnyObject>)>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingActionTarget"]
    #[ivars = ActionIvars]
    pub struct ActionTarget;

    impl ActionTarget {
        #[unsafe(method(invoke:))]
        fn invoke(&self, sender: Option<&AnyObject>) {
            (self.ivars().f)(sender);
        }
    }
);

impl ActionTarget {
    pub fn new(f: impl Fn(Option<&AnyObject>) + 'static) -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(ActionIvars { f: Box::new(f) });
        unsafe { msg_send![super(this), init] }
    }
}

static TARGET_KEY: u8 = 0;

/// Keep `obj` alive for as long as `owner` lives (associated object).
pub fn retain_on(owner: &AnyObject, obj: &AnyObject, key: &'static u8) {
    unsafe {
        objc2::ffi::objc_setAssociatedObject(
            owner as *const AnyObject as *mut AnyObject,
            key as *const u8 as *const c_void,
            obj as *const AnyObject as *mut AnyObject,
            objc2::ffi::OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
    }
}

/// Attach a closure as the action of an NSControl (button, slider, popup...).
pub fn on_action(control: &NSControl, f: impl Fn(Option<&AnyObject>) + 'static) {
    let t = ActionTarget::new(f);
    let any: &AnyObject = &t;
    unsafe {
        control.setTarget(Some(any));
        control.setAction(Some(sel!(invoke:)));
    }
    retain_on(control, any, &TARGET_KEY);
}

pub fn on_click(control: &NSControl, f: impl Fn() + 'static) {
    on_action(control, move |_| f());
}

/// Build a menu item with a closure action.
pub fn menu_item(title: &str, key: &str, f: impl Fn() + 'static) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm()),
            &ns(title),
            Some(sel!(invoke:)),
            &ns(key),
        )
    };
    let t = ActionTarget::new(move |_| f());
    let any: &AnyObject = &t;
    unsafe { item.setTarget(Some(any)) };
    retain_on(&item, any, &TARGET_KEY);
    item
}

pub fn menu_item_mods(
    title: &str,
    key: &str,
    mods: NSEventModifierFlags,
    f: impl Fn() + 'static,
) -> Retained<NSMenuItem> {
    let item = menu_item(title, key, f);
    item.setKeyEquivalentModifierMask(mods);
    item
}

pub fn menu(title: &str) -> Retained<NSMenu> {
    NSMenu::initWithTitle(NSMenu::alloc(mtm()), &ns(title))
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rgba {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Rgba {
    pub const fn new(r: f64, g: f64, b: f64, a: f64) -> Self {
        Self { r, g, b, a }
    }
    pub const fn hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xff) as f64 / 255.0,
            g: ((v >> 8) & 0xff) as f64 / 255.0,
            b: (v & 0xff) as f64 / 255.0,
            a: 1.0,
        }
    }
    pub fn with_alpha(self, a: f64) -> Self {
        Self { a, ..self }
    }
    pub fn ns(&self) -> Retained<NSColor> {
        NSColor::colorWithSRGBRed_green_blue_alpha(self.r, self.g, self.b, self.a)
    }
    pub fn cg(&self) -> CFRetained<CGColor> {
        CGColor::new_srgb(self.r, self.g, self.b, self.a)
    }
    pub fn to_hex(&self) -> String {
        let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02X}{:02X}{:02X}", c(self.r), c(self.g), c(self.b))
    }
    pub fn from_ns(color: &NSColor) -> Self {
        let c = color
            .colorUsingColorSpace(&NSColorSpace::sRGBColorSpace())
            .unwrap_or_else(|| color.retain());
        Self {
            r: c.redComponent(),
            g: c.greenComponent(),
            b: c.blueComponent(),
            a: c.alphaComponent(),
        }
    }
    /// Perceived luminance, used to pick contrasting outlines.
    pub fn luma(&self) -> f64 {
        0.299 * self.r + 0.587 * self.g + 0.114 * self.b
    }
}

// ---------------------------------------------------------------------------
// Geometry helpers
// ---------------------------------------------------------------------------

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

pub fn rect_from_points(a: CGPoint, b: CGPoint) -> CGRect {
    rect(a.x.min(b.x), a.y.min(b.y), (a.x - b.x).abs(), (a.y - b.y).abs())
}

pub fn rect_contains(r: CGRect, p: CGPoint) -> bool {
    p.x >= r.origin.x
        && p.y >= r.origin.y
        && p.x <= r.origin.x + r.size.width
        && p.y <= r.origin.y + r.size.height
}

pub fn rect_intersection(a: CGRect, b: CGRect) -> Option<CGRect> {
    let x0 = a.origin.x.max(b.origin.x);
    let y0 = a.origin.y.max(b.origin.y);
    let x1 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    if x1 > x0 && y1 > y0 {
        Some(rect(x0, y0, x1 - x0, y1 - y0))
    } else {
        None
    }
}

pub fn rect_union(a: CGRect, b: CGRect) -> CGRect {
    let x0 = a.origin.x.min(b.origin.x);
    let y0 = a.origin.y.min(b.origin.y);
    let x1 = (a.origin.x + a.size.width).max(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).max(b.origin.y + b.size.height);
    rect(x0, y0, x1 - x0, y1 - y0)
}

pub fn rect_inset(r: CGRect, d: f64) -> CGRect {
    rect(
        r.origin.x + d,
        r.origin.y + d,
        (r.size.width - 2.0 * d).max(0.0),
        (r.size.height - 2.0 * d).max(0.0),
    )
}

pub fn rect_integral(r: CGRect) -> CGRect {
    let x0 = r.origin.x.floor();
    let y0 = r.origin.y.floor();
    let x1 = (r.origin.x + r.size.width).ceil();
    let y1 = (r.origin.y + r.size.height).ceil();
    rect(x0, y0, x1 - x0, y1 - y0)
}

// ---------------------------------------------------------------------------
// UI builders
// ---------------------------------------------------------------------------

pub fn symbol(name: &str, points: CGFloat) -> Retained<NSImage> {
    symbol_weighted(name, points, unsafe { NSFontWeightRegular })
}

pub fn symbol_weighted(name: &str, points: CGFloat, weight: CGFloat) -> Retained<NSImage> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), None)
        .or_else(|| {
            NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns("square"), None)
        })
        .expect("SF Symbols available");
    let cfg = NSImageSymbolConfiguration::configurationWithPointSize_weight(points, weight);
    img.imageWithSymbolConfiguration(&cfg).unwrap_or(img)
}

pub fn label(text: &str, size: CGFloat, bold: bool) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&ns(text), mtm());
    let font = if bold {
        NSFont::boldSystemFontOfSize(size)
    } else {
        NSFont::systemFontOfSize(size)
    };
    l.setFont(Some(&font));
    l
}

pub fn secondary_label(text: &str, size: CGFloat) -> Retained<NSTextField> {
    let l = label(text, size, false);
    l.setTextColor(Some(&NSColor::secondaryLabelColor()));
    l
}

/// A borderless icon button (toolbar look).
pub fn icon_button(symbol_name: &str, tooltip: &str, f: impl Fn() + 'static) -> Retained<NSButton> {
    let m = mtm();
    let b = unsafe {
        NSButton::buttonWithImage_target_action(&symbol(symbol_name, 15.0), None, None, m)
    };
    b.setBezelStyle(NSBezelStyle::Toolbar);
    b.setToolTip(Some(&ns(tooltip)));
    on_click(&b, f);
    b
}

/// A toggle-able icon button (stays highlighted while "on").
pub fn toggle_button(symbol_name: &str, tooltip: &str, f: impl Fn() + 'static) -> Retained<NSButton> {
    let b = icon_button(symbol_name, tooltip, f);
    b.setButtonType(NSButtonType::PushOnPushOff);
    b
}

pub fn text_button(title: &str, f: impl Fn() + 'static) -> Retained<NSButton> {
    let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), None, None, mtm()) };
    on_click(&b, f);
    b
}

pub fn text_icon_button(
    title: &str,
    symbol_name: &str,
    f: impl Fn() + 'static,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_image_target_action(
            &ns(title),
            &symbol(symbol_name, 13.0),
            None,
            None,
            mtm(),
        )
    };
    b.setImagePosition(NSCellImagePosition::ImageLeading);
    on_click(&b, f);
    b
}

pub fn set_on(button: &NSButton, on: bool) {
    button.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
}

pub fn is_on(button: &NSButton) -> bool {
    button.state() == NSControlStateValueOn
}

pub fn hstack(views: &[&NSView], spacing: CGFloat) -> Retained<NSStackView> {
    let s = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm());
    s.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    s.setSpacing(spacing);
    s.setAlignment(NSLayoutAttribute::CenterY);
    s
}

pub fn vstack(views: &[&NSView], spacing: CGFloat) -> Retained<NSStackView> {
    let s = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm());
    s.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    s.setSpacing(spacing);
    s.setAlignment(NSLayoutAttribute::Leading);
    s
}

/// A rounded "glass" container used for floating bars and the capture bar.
pub fn hud_container(frame: CGRect, radius: CGFloat) -> Retained<NSVisualEffectView> {
    let v = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm()), frame);
    v.setMaterial(NSVisualEffectMaterial::Popover);
    v.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    v.setState(NSVisualEffectState::Active);
    v.setWantsLayer(true);
    if let Some(layer) = v.layer() {
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(true);
    }
    v
}

pub fn fixed_size(view: &NSView, w: CGFloat, h: CGFloat) {
    view.setTranslatesAutoresizingMaskIntoConstraints(false);
    if w > 0.0 {
        view.widthAnchor().constraintEqualToConstant(w).setActive(true);
    }
    if h > 0.0 {
        view.heightAnchor().constraintEqualToConstant(h).setActive(true);
    }
}

pub fn alert(title: &str, message: &str) {
    let a = NSAlert::new(mtm());
    a.setMessageText(&ns(title));
    a.setInformativeText(&ns(message));
    a.runModal();
}

pub fn timestamp_name(prefix: &str) -> String {
    // Local time via libc to avoid extra dependencies.
    let now = unsafe { libc_time() };
    format!(
        "{prefix} {:04}-{:02}-{:02} {:02}{:02}{:02}",
        now.0, now.1, now.2, now.3, now.4, now.5
    )
}

unsafe fn libc_time() -> (i32, i32, i32, i32, i32, i32) {
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }
    extern "C" {
        fn time(t: *mut i64) -> i64;
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let t = time(std::ptr::null_mut());
    let mut tm: Tm = std::mem::zeroed();
    localtime_r(&t, &mut tm);
    (
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

pub fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

/// Pick a non-existing file path `dir/stem.ext`, adding " (n)" if needed.
pub fn unique_path(dir: &std::path::Path, stem: &str, ext: &str) -> std::path::PathBuf {
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    p
}

pub fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&ns(&path.to_string_lossy()))
}

pub fn temp_dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join("snipping");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn log(msg: &str) {
    eprintln!("[snipping] {msg}");
    // Bundled apps have no visible stderr; keep a small log for diagnosis.
    use std::io::Write;
    let dir = home_dir().join("Library/Logs/Snipping");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("snipping.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("snipping.old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {msg}", timestamp_name("").trim());
    }
}

/// Retain a borrowed CoreFoundation object.
pub fn cf_retain<T: objc2_core_foundation::Type>(obj: &T) -> CFRetained<T> {
    unsafe { CFRetained::retain(std::ptr::NonNull::from(obj)) }
}

// ---------------------------------------------------------------------------
// Generic window delegate (close callback)
// ---------------------------------------------------------------------------

pub struct WinDelegateIvars {
    on_close: std::cell::RefCell<Option<Box<dyn Fn()>>>,
    on_resign_key: std::cell::RefCell<Option<Box<dyn Fn()>>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingWindowDelegate"]
    #[ivars = WinDelegateIvars]
    pub struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            let f = self.ivars().on_close.borrow_mut().take();
            if let Some(f) = f {
                f();
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _n: &NSNotification) {
            if let Some(f) = self.ivars().on_resign_key.borrow().as_ref() {
                f();
            }
        }
    }
);

static DELEGATE_KEY: u8 = 0;

/// Run `f` once when the window closes (delegate retained by the window).
pub fn on_window_close(window: &NSWindow, f: impl Fn() + 'static) {
    on_window_events(window, f, None::<fn()>);
}

/// Close callback plus an optional "window lost focus" callback.
pub fn on_window_events(window: &NSWindow, on_close: impl Fn() + 'static, on_resign_key: Option<impl Fn() + 'static>) {
    let this = WindowDelegate::alloc(mtm()).set_ivars(WinDelegateIvars {
        on_close: std::cell::RefCell::new(Some(Box::new(on_close))),
        on_resign_key: std::cell::RefCell::new(on_resign_key.map(|f| Box::new(f) as Box<dyn Fn()>)),
    });
    let d: Retained<WindowDelegate> = unsafe { msg_send![super(this), init] };
    window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*d)));
    retain_on(window, &d, &DELEGATE_KEY);
}

/// SF Symbol tinted with a fixed color (works regardless of bezel style).
pub fn symbol_colored(name: &str, points: CGFloat, color: &NSColor) -> Retained<NSImage> {
    let img = symbol(name, points);
    let cfg = NSImageSymbolConfiguration::configurationWithHierarchicalColor(color);
    img.imageWithSymbolConfiguration(&cfg).unwrap_or(img)
}
