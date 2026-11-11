//! The full-screen snip overlay: frozen screen, dimming, capture bar, and the
//! rectangle / window / full-screen / freeform / color-picker interactions.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::*;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::*;
use objc2_foundation::*;
use objc2_quartz_core::*;

use crate::capture::{Frozen, ScreenInfo};
use crate::settings::{self, SnipMode};
use crate::util::{
    self, after, mtm, ns, on_click, rect, rect_contains, rect_from_points,
    rect_intersection, symbol, Rgba,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Snip,
    Record,
    TextExtract,
    ColorPick,
}

pub enum OverlayResult {
    Image { image: CFRetained<CGImage>, scale: f64 },
    Text { image: CFRetained<CGImage> },
    Color(Rgba),
    Region { screen: ScreenInfo, rect: CGRect },
    Cancelled,
}

struct Layers {
    image: Retained<CALayer>,
    dim: Retained<CAShapeLayer>,
    border: Retained<CAShapeLayer>,
}

enum Drag {
    Rect { display: usize, start: CGPoint, cur: CGPoint },
    Free { display: usize, pts: Vec<CGPoint> },
}

struct Session {
    frozen: Frozen,
    screens: Vec<ScreenInfo>,
    windows: Vec<Retained<OverlayWindow>>,
    views: Vec<Retained<OverlayView>>,
    layers: Vec<Layers>,
    bar: Option<Retained<NSView>>,
    bar_display: usize,
    mode_buttons: Vec<(SnipMode, Retained<NSButton>)>,
    text_button: Option<Retained<NSButton>>,
    color_button: Option<Retained<NSButton>>,
    purpose_buttons: Option<(Retained<NSButton>, Retained<NSButton>)>,
    size_label: Retained<NSTextField>,
    loupe: Retained<NSView>,
    loupe_image: Retained<CALayer>,
    loupe_label: Retained<NSTextField>,
    mode: SnipMode,
    purpose: Purpose,
    drag: Option<Drag>,
    hover_window: Option<usize>,
    /// A mouse press started on the overlay (not the bar) — required for
    /// click-to-snip in Window mode.
    pressed: bool,
    done: Option<Box<dyn FnOnce(OverlayResult)>>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    static TEST_HIDDEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn is_active() -> bool {
    SESSION.with(|s| s.borrow().is_some())
}

// ---------------------------------------------------------------------------
// Window + view classes
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super = NSPanel)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingOverlayWindow"]
    pub struct OverlayWindow;

    impl OverlayWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }
    }
);

pub struct ViewIvars {
    display: usize,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingOverlayView"]
    #[ivars = ViewIvars]
    pub struct OverlayView;

    impl OverlayView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let p = self.global_point(e);
            handle(self.ivars().display, Ev::Down(p));
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            let p = self.global_point(e);
            handle(self.ivars().display, Ev::Drag(p));
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            let p = self.global_point(e);
            handle(self.ivars().display, Ev::Up(p));
        }
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            let p = self.global_point(e);
            handle(self.ivars().display, Ev::Move(p));
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _e: &NSEvent) {
            handle(self.ivars().display, Ev::Cancel);
        }
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _e: &NSEvent) {
            NSCursor::crosshairCursor().set();
        }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            match e.keyCode() {
                53 => handle(self.ivars().display, Ev::Cancel),
                36 | 76 => handle(self.ivars().display, Ev::Enter),
                _ => {}
            }
        }
    }
);

impl OverlayView {
    fn new(frame: CGRect, display: usize) -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(ViewIvars { display });
        let v: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        v.setWantsLayer(true);
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::CursorUpdate
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&*v as &AnyObject),
                None,
            )
        };
        v.addTrackingArea(&area);
        v
    }

    fn global_point(&self, e: &NSEvent) -> CGPoint {
        let local = e.locationInWindow();
        let origin = self.window().map(|w| w.frame().origin).unwrap_or(NSPoint::ZERO);
        CGPoint::new(local.x + origin.x, local.y + origin.y)
    }
}

// The capture bar swallows clicks and shows an arrow cursor.
define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingCaptureBar"]
    pub struct CaptureBar;

    impl CaptureBar {
        // Swallow every click on the bar's background so it can't reach the
        // overlay (e.g. a mouse-up in Window mode would snap a window).
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _e: &NSEvent) {}
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _e: &NSEvent) {}
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, _e: &NSEvent) {}
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, _e: &NSEvent) {}
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _e: &NSEvent) {
            NSCursor::arrowCursor().set();
        }
    }
);

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

pub struct Request {
    pub purpose: Purpose,
    pub mode: SnipMode,
    pub frozen: Frozen,
    pub show_bar: bool,
    pub done: Box<dyn FnOnce(OverlayResult)>,
}

fn cgpath_rect(r: CGRect) -> CFRetained<CGMutablePath> {
    let p = CGMutablePath::new();
    unsafe { CGMutablePath::add_rect(Some(&p), std::ptr::null(), r) };
    p
}

pub fn show(req: Request) {
    if is_active() {
        (req.done)(OverlayResult::Cancelled);
        return;
    }
    let m = mtm();
    let screens: Vec<ScreenInfo> = req.frozen.displays.iter().map(|d| d.info).collect();
    let mouse = NSEvent::mouseLocation();
    let bar_display = screens
        .iter()
        .position(|s| rect_contains(s.frame, mouse))
        .unwrap_or(0);

    let mut windows = Vec::new();
    let mut views = Vec::new();
    let mut layers = Vec::new();
    for (i, shot) in req.frozen.displays.iter().enumerate() {
        let f = shot.info.frame;
        let this = OverlayWindow::alloc(m).set_ivars(());
        let win: Retained<OverlayWindow> = unsafe {
            msg_send![super(this),
                initWithContentRect: f,
                styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                backing: NSBackingStoreType::Buffered,
                defer: false]
        };
        unsafe {
            win.setReleasedWhenClosed(false);
            win.setLevel(NSScreenSaverWindowLevel);
            win.setOpaque(false);
            win.setBackgroundColor(Some(&NSColor::clearColor()));
            win.setHasShadow(false);
            win.setAcceptsMouseMovedEvents(true);
            // NSPanel hides on deactivate by default; the overlay must stay up
            // (otherwise the session lingers invisibly and blocks new snips).
            win.setHidesOnDeactivate(false);
            // No open/close animation: AppKit's default panel "pop" scales the
            // whole frozen screen in, which looks like the screen bouncing.
            win.setAnimationBehavior(NSWindowAnimationBehavior::None);
            win.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::IgnoresCycle,
            );
            win.setFrame_display(f, false);
        }
        let local = rect(0.0, 0.0, f.size.width, f.size.height);
        let view = OverlayView::new(local, i);
        let root = view.layer().expect("layer-backed");

        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let image = CALayer::layer();
        image.setFrame(local);
        unsafe {
            image.setContents(Some(&*(&*shot.image as *const CGImage as *const AnyObject)));
            image.setContentsGravity(kCAGravityResize);
        }
        image.setContentsScale(shot.info.scale);
        root.addSublayer(&image);

        let dim = CAShapeLayer::layer();
        dim.setFrame(local);
        unsafe { dim.setFillRule(kCAFillRuleEvenOdd) };
        dim.setFillColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 0.42)));
        dim.setPath(Some(&cgpath_rect(local)));
        root.addSublayer(&dim);

        let border = CAShapeLayer::layer();
        border.setFrame(local);
        border.setFillColor(None);
        border.setStrokeColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.95)));
        border.setLineWidth(1.0);
        root.addSublayer(&border);
        CATransaction::commit();

        win.setContentView(Some(&view));
        windows.push(win);
        views.push(view);
        layers.push(Layers { image, dim, border });
    }

    // Size label + color loupe live on the display the mouse is on; they get
    // re-parented as the pointer moves between displays.
    let size_label = NSTextField::labelWithString(&ns(""), m);
    size_label.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(11.0, unsafe { NSFontWeightMedium })));
    size_label.setTextColor(Some(&NSColor::whiteColor()));
    size_label.setDrawsBackground(true);
    size_label.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.7)));
    size_label.setHidden(true);

    let loupe = NSView::initWithFrame(NSView::alloc(m), rect(0.0, 0.0, 120.0, 142.0));
    loupe.setWantsLayer(true);
    let ll = loupe.layer().unwrap();
    ll.setBackgroundColor(Some(&CGColor::new_srgb(0.12, 0.12, 0.12, 0.92)));
    ll.setCornerRadius(10.0);
    ll.setBorderColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.8)));
    ll.setBorderWidth(1.0);
    let loupe_image = CALayer::layer();
    loupe_image.setFrame(rect(6.0, 28.0, 108.0, 108.0));
    unsafe { loupe_image.setMagnificationFilter(kCAFilterNearest) };
    loupe_image.setBorderColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.5)));
    loupe_image.setBorderWidth(0.5);
    ll.addSublayer(&loupe_image);
    let center = CALayer::layer();
    center.setFrame(rect(6.0 + 54.0 - 4.5, 28.0 + 54.0 - 4.5, 9.0, 9.0));
    center.setBorderColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 1.0)));
    center.setBorderWidth(1.5);
    ll.addSublayer(&center);
    let loupe_label = NSTextField::labelWithString(&ns("#000000"), m);
    loupe_label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(12.0, unsafe { NSFontWeightSemibold })));
    loupe_label.setTextColor(Some(&NSColor::whiteColor()));
    loupe_label.setAlignment(NSTextAlignment::Center);
    loupe_label.setFrame(rect(0.0, 5.0, 120.0, 18.0));
    loupe.addSubview(&loupe_label);
    loupe.setHidden(true);

    let session = Session {
        frozen: req.frozen,
        screens,
        windows,
        views,
        layers,
        bar: None,
        bar_display,
        mode_buttons: Vec::new(),
        text_button: None,
        color_button: None,
        purpose_buttons: None,
        size_label,
        loupe,
        loupe_image,
        loupe_label,
        mode: req.mode,
        purpose: req.purpose,
        drag: None,
        hover_window: None,
        pressed: false,
        done: Some(req.done),
    };
    SESSION.with(|s| *s.borrow_mut() = Some(session));

    if req.show_bar {
        build_bar();
    }
    SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let s = guard.as_mut().unwrap();
        apply_purpose_visuals(s);
        let hidden = TEST_HIDDEN.with(|t| t.get());
        for (i, w) in s.windows.iter().enumerate() {
            if hidden {
                continue;
            }
            if i == s.bar_display {
                w.makeKeyAndOrderFront(None);
                w.makeFirstResponder(Some(&s.views[i]));
            } else {
                w.orderFrontRegardless();
            }
        }
        let v = &s.views[s.bar_display];
        v.addSubview(&s.size_label);
        v.addSubview(&s.loupe);
    });
    NSCursor::crosshairCursor().set();
    // Prime hover state (window mode / color picker) at the current pointer.
    let p = NSEvent::mouseLocation();
    handle(bar_display, Ev::Move(p));
}

fn bar_button(sym: &str, tip: &str, f: impl Fn() + 'static) -> Retained<NSButton> {
    let m = mtm();
    let b = unsafe { NSButton::buttonWithImage_target_action(&symbol(sym, 17.0), None, None, m) };
    b.setBordered(false);
    b.setButtonType(NSButtonType::MomentaryChange);
    b.setContentTintColor(Some(&NSColor::whiteColor()));
    b.setToolTip(Some(&ns(tip)));
    b.setWantsLayer(true);
    if let Some(l) = b.layer() {
        l.setCornerRadius(8.0);
    }
    util::fixed_size(&b, 42.0, 36.0);
    on_click(&b, f);
    b
}

fn set_selected(b: &NSButton, on: bool) {
    if let Some(l) = b.layer() {
        l.setBackgroundColor(if on { Some(CGColor::new_srgb(1.0, 1.0, 1.0, 0.24)) } else { None }.as_deref());
    }
    let tint = if on { NSColor::whiteColor() } else { NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.82) };
    b.setContentTintColor(Some(&tint));
}

fn build_bar() {
    let m = mtm();
    let bar = {
        let this = CaptureBar::alloc(m).set_ivars(());
        let v: Retained<CaptureBar> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 10.0, 10.0)] };
        v
    };
    let snip_b = bar_button("camera", "Snip", || set_purpose(Purpose::Snip));
    let rec_b = bar_button("video", "Record", || set_purpose(Purpose::Record));
    let mut views: Vec<Retained<NSView>> = vec![
        Retained::into_super(Retained::into_super(snip_b.clone())),
        Retained::into_super(Retained::into_super(rec_b.clone())),
    ];
    let sep = || {
        let v = NSView::initWithFrame(NSView::alloc(mtm()), rect(0.0, 0.0, 1.0, 24.0));
        v.setWantsLayer(true);
        if let Some(l) = v.layer() {
            l.setBackgroundColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.25)));
        }
        util::fixed_size(&v, 1.0, 24.0);
        v
    };
    views.push(sep());
    let mut mode_buttons = Vec::new();
    for mode in SnipMode::ALL {
        let b = bar_button(mode.symbol(), &format!("{} snip", mode.title()), move || set_mode(mode));
        mode_buttons.push((mode, b.clone()));
        views.push(Retained::into_super(Retained::into_super(b)));
    }
    views.push(sep());
    let text_button = bar_button("text.viewfinder", "Text extractor", || set_purpose(Purpose::TextExtract));
    views.push(Retained::into_super(Retained::into_super(text_button.clone())));
    let color_button = bar_button("eyedropper", "Color picker", || set_purpose(Purpose::ColorPick));
    views.push(Retained::into_super(Retained::into_super(color_button.clone())));
    views.push(sep());
    let close = bar_button("xmark", "Close (Esc)", || cancel());
    views.push(Retained::into_super(Retained::into_super(close)));

    let refs: Vec<&NSView> = views.iter().map(|v| &**v).collect();
    let stack = util::hstack(&refs, 4.0);
    stack.setEdgeInsets(NSEdgeInsets { top: 6.0, left: 8.0, bottom: 6.0, right: 8.0 });
    let size = stack.fittingSize();
    let frame = rect(0.0, 0.0, size.width, size.height);
    bar.setFrame(frame);
    stack.setFrame(frame);
    bar.addSubview(&stack);
    bar.setWantsLayer(true);
    if let Some(l) = bar.layer() {
        l.setBackgroundColor(Some(&CGColor::new_srgb(0.13, 0.13, 0.14, 0.96)));
        l.setCornerRadius(12.0);
        l.setBorderColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.16)));
        l.setBorderWidth(1.0);
        l.setShadowColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
        l.setShadowOpacity(0.45);
        l.setShadowRadius(14.0);
        l.setShadowOffset(objc2_core_foundation::CGSize::new(0.0, -4.0));
    }
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::CursorUpdate
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(&*bar as &AnyObject),
            None,
        )
    };
    bar.addTrackingArea(&area);

    SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let s = guard.as_mut().unwrap();
        let disp = s.screens[s.bar_display].frame;
        bar.setFrameOrigin(NSPoint::new(
            ((disp.size.width - size.width) / 2.0).round(),
            disp.size.height - size.height - 14.0 - menu_bar_inset(disp),
        ));
        s.views[s.bar_display].addSubview(&bar);
        s.bar = Some(Retained::into_super(bar));
        s.mode_buttons = mode_buttons;
        s.text_button = Some(text_button);
        s.color_button = Some(color_button);
        s.purpose_buttons = Some((snip_b, rec_b));
    });
}

fn menu_bar_inset(display: CGRect) -> f64 {
    // Keep the bar clear of the menu bar / notch on the display that has it.
    for screen in NSScreen::screens(mtm()).iter() {
        if screen.frame() == display {
            let full = screen.frame();
            let vis = screen.visibleFrame();
            return (full.origin.y + full.size.height) - (vis.origin.y + vis.size.height);
        }
    }
    0.0
}

fn update_bar_state(s: &Session) {
    for (mode, b) in &s.mode_buttons {
        let on = (s.purpose == Purpose::Snip && *mode == s.mode)
            || (s.purpose == Purpose::Record && *mode == SnipMode::Rectangle);
        set_selected(b, on);
        b.setEnabled(s.purpose != Purpose::Record || *mode == SnipMode::Rectangle);
        b.setAlphaValue(if b.isEnabled() { 1.0 } else { 0.35 });
    }
    if let Some(b) = &s.text_button {
        set_selected(b, s.purpose == Purpose::TextExtract);
    }
    if let Some(b) = &s.color_button {
        set_selected(b, s.purpose == Purpose::ColorPick);
    }
    if let Some((snip, rec)) = &s.purpose_buttons {
        set_selected(snip, s.purpose != Purpose::Record);
        set_selected(rec, s.purpose == Purpose::Record);
    }
}

fn apply_purpose_visuals(s: &mut Session) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    for (i, l) in s.layers.iter().enumerate() {
        // Recording shows the live desktop through the overlay.
        l.image.setHidden(s.purpose == Purpose::Record);
        let local = rect(0.0, 0.0, s.screens[i].frame.size.width, s.screens[i].frame.size.height);
        l.dim.setPath(Some(&cgpath_rect(local)));
        l.dim.setFillColor(Some(&CGColor::new_srgb(
            0.0,
            0.0,
            0.0,
            match s.purpose {
                Purpose::ColorPick => 0.0,
                Purpose::Record => 0.3,
                _ => 0.42,
            },
        )));
        l.border.setPath(None);
    }
    CATransaction::commit();
    s.size_label.setHidden(true);
    s.loupe.setHidden(s.purpose != Purpose::ColorPick);
    update_bar_state(s);
}

fn set_mode(mode: SnipMode) {
    let full = SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(s) = guard.as_mut() else { return false };
        if s.purpose == Purpose::Record {
            // Recording only supports a region; the mode buttons don't
            // switch a recording into a screenshot.
            return false;
        }
        s.mode = mode;
        s.purpose = Purpose::Snip;
        s.drag = None;
        apply_purpose_visuals(s);
        mode == SnipMode::FullScreen
    });
    if SESSION.with(|s| s.borrow().as_ref().map(|s| s.purpose == Purpose::Record)).unwrap_or(false) {
        return;
    }
    settings::update(|st| st.snip_mode = mode);
    if full {
        // Windows captures the whole desktop immediately.
        let result = SESSION.with(|s| {
            s.borrow().as_ref().and_then(|s| s.frozen.full_desktop())
        });
        match result {
            Some((image, scale)) => finish(OverlayResult::Image { image, scale }),
            None => cancel(),
        }
    } else {
        let p = NSEvent::mouseLocation();
        let d = SESSION.with(|s| s.borrow().as_ref().map(|s| s.bar_display)).unwrap_or(0);
        handle(d, Ev::Move(p));
    }
}

fn set_purpose(p: Purpose) {
    SESSION.with(|s| {
        let mut guard = s.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        // Toggle the text extractor / color picker off when clicked again.
        s.purpose = if s.purpose == p && matches!(p, Purpose::TextExtract | Purpose::ColorPick) {
            Purpose::Snip
        } else {
            p
        };
        if s.purpose != Purpose::Snip {
            if s.mode == SnipMode::FullScreen || s.mode == SnipMode::Window && s.purpose == Purpose::Record {
                s.mode = SnipMode::Rectangle;
            }
        }
        s.drag = None;
        apply_purpose_visuals(s);
    });
    let p = NSEvent::mouseLocation();
    let d = SESSION.with(|s| s.borrow().as_ref().map(|s| s.bar_display)).unwrap_or(0);
    handle(d, Ev::Move(p));
}

pub fn cancel() {
    finish(OverlayResult::Cancelled);
}

fn finish(result: OverlayResult) {
    let Some(mut session) = SESSION.with(|s| s.borrow_mut().take()) else { return };
    for w in &session.windows {
        w.orderOut(None);
    }
    let done = session.done.take();
    // Release the windows on the next turn of the run loop (we may be inside
    // one of their event handlers right now).
    let windows = std::mem::take(&mut session.windows);
    after(0.05, move || {
        for w in windows {
            w.close();
        }
        drop(session);
    });
    NSCursor::arrowCursor().set();
    if let Some(done) = done {
        done(result);
    }
}

// ---------------------------------------------------------------------------
// Event handling
// ---------------------------------------------------------------------------

enum Ev {
    Down(CGPoint),
    Drag(CGPoint),
    Up(CGPoint),
    Move(CGPoint),
    Enter,
    Cancel,
}

fn local(s: &Session, display: usize, p: CGPoint) -> CGPoint {
    let f = s.screens[display].frame;
    CGPoint::new(p.x - f.origin.x, p.y - f.origin.y)
}

fn clamp_to_display(s: &Session, display: usize, p: CGPoint) -> CGPoint {
    let f = s.screens[display].frame;
    CGPoint::new(
        p.x.clamp(f.origin.x, f.origin.x + f.size.width),
        p.y.clamp(f.origin.y, f.origin.y + f.size.height),
    )
}

/// Highlight `hole` (display-local) on display `d`, dim elsewhere.
fn set_hole(s: &Session, d: usize, hole: Option<&CGPath>, outline: Option<&CGPath>) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    for (i, l) in s.layers.iter().enumerate() {
        let local = rect(0.0, 0.0, s.screens[i].frame.size.width, s.screens[i].frame.size.height);
        let path = cgpath_rect(local);
        if i == d {
            if let Some(h) = hole {
                unsafe { CGMutablePath::add_path(Some(&path), std::ptr::null(), Some(h)) };
            }
            l.border.setPath(outline);
        } else {
            l.border.setPath(None);
        }
        l.dim.setPath(Some(&path));
    }
    CATransaction::commit();
}

fn show_size_label(s: &Session, d: usize, r_local: CGRect) {
    let scale = s.frozen.displays[d].px_scale().0;
    let text = format!(
        " {} × {} ",
        (r_local.size.width * scale).round() as i64,
        (r_local.size.height * scale).round() as i64
    );
    s.size_label.setStringValue(&ns(&text));
    let sz = s.size_label.fittingSize();
    let disp = s.screens[d].frame.size;
    let mut x = r_local.origin.x + r_local.size.width - sz.width;
    let mut y = r_local.origin.y - sz.height - 6.0;
    if y < 4.0 {
        y = r_local.origin.y + 6.0;
    }
    x = x.clamp(4.0, disp.width - sz.width - 4.0);
    let view = &s.views[d];
    if unsafe { s.size_label.superview() }.map(|v| &*v as *const NSView) != Some(&***view as *const NSView) {
        s.size_label.removeFromSuperview();
        view.addSubview(&s.size_label);
    }
    s.size_label.setFrame(rect(x, y, sz.width, sz.height));
    s.size_label.setHidden(false);
}

fn update_loupe(s: &Session, d: usize, p: CGPoint) {
    let shot = &s.frozen.displays[d];
    let (sx, sy) = shot.px_scale();
    let l = local(s, d, p);
    let px = (l.x * sx).floor();
    let py = ((shot.info.frame.size.height - l.y) * sy).floor();
    let n: f64 = 11.0;
    if let Some(crop) = CGImage::with_image_in_rect(
        Some(&shot.image),
        rect(px - (n / 2.0).floor(), py - (n / 2.0).floor(), n, n),
    ) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        unsafe {
            s.loupe_image
                .setContents(Some(&*(&*crop as *const CGImage as *const AnyObject)));
        }
        CATransaction::commit();
    }
    if let Some(c) = s.frozen.color_at(p) {
        s.loupe_label.setStringValue(&ns(&c.to_hex()));
    }
    let view = &s.views[d];
    if unsafe { s.loupe.superview() }.map(|v| &*v as *const NSView) != Some(&***view as *const NSView) {
        s.loupe.removeFromSuperview();
        view.addSubview(&s.loupe);
    }
    let disp = s.screens[d].frame.size;
    let (w, h) = (120.0, 142.0);
    let mut x = l.x + 24.0;
    let mut y = l.y - h - 24.0;
    if x + w > disp.width {
        x = l.x - w - 24.0;
    }
    if y < 0.0 {
        y = l.y + 24.0;
    }
    s.loupe.setFrameOrigin(NSPoint::new(x, y));
    s.loupe.setHidden(false);
}

fn handle(display: usize, ev: Ev) {
    enum Outcome {
        None,
        Finish(OverlayResult),
        Cancel,
    }
    let outcome = SESSION.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(s) = guard.as_mut() else { return Outcome::None };
        match ev {
            Ev::Cancel => Outcome::Cancel,
            Ev::Enter => {
                if s.mode == SnipMode::Window && s.purpose == Purpose::Snip {
                    if let Some(i) = s.hover_window {
                        let f = s.frozen.windows[i].frame;
                        if let Some((image, scale)) = s.frozen.crop_global(f) {
                            return Outcome::Finish(OverlayResult::Image { image, scale });
                        }
                    }
                }
                Outcome::None
            }
            Ev::Move(p) => {
                // Move the bar-display association with the pointer for labels.
                if let Some(d) = s.frozen.display_index_at(p) {
                    match s.purpose {
                        Purpose::ColorPick => update_loupe(s, d, p),
                        Purpose::Snip if s.mode == SnipMode::Window => {
                            let hit = s.frozen.windows.iter().position(|w| rect_contains(w.frame, p));
                            if hit != s.hover_window || true {
                                s.hover_window = hit;
                                match hit {
                                    Some(i) => {
                                        let wf = s.frozen.windows[i].frame;
                                        // Highlight on every display the window touches.
                                        CATransaction::begin();
                                        CATransaction::setDisableActions(true);
                                        for (di, l) in s.layers.iter().enumerate() {
                                            let df = s.screens[di].frame;
                                            let local_r = rect(0.0, 0.0, df.size.width, df.size.height);
                                            let path = cgpath_rect(local_r);
                                            if let Some(inter) = rect_intersection(df, wf) {
                                                let lr = rect(inter.origin.x - df.origin.x, inter.origin.y - df.origin.y, inter.size.width, inter.size.height);
                                                let hole = cgpath_rect(lr);
                                                unsafe { CGMutablePath::add_path(Some(&path), std::ptr::null(), Some(&hole)) };
                                                l.border.setPath(Some(&cgpath_rect(crate::util::rect_inset(lr, 1.0))));
                                                l.border.setStrokeColor(Some(&Rgba::hex(0x0A84FF).cg()));
                                                l.border.setLineWidth(3.0);
                                            } else {
                                                l.border.setPath(None);
                                            }
                                            l.dim.setPath(Some(&path));
                                        }
                                        CATransaction::commit();
                                    }
                                    None => set_hole(s, d, None, None),
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let over_bar = s.bar.as_ref().map(|bar| {
                    let f = s.screens[s.bar_display].frame;
                    let local = CGPoint::new(p.x - f.origin.x, p.y - f.origin.y);
                    rect_contains(f, p) && !bar.isHidden() && rect_contains(bar.frame(), local)
                }).unwrap_or(false);
                if over_bar {
                    NSCursor::arrowCursor().set();
                } else {
                    NSCursor::crosshairCursor().set();
                }
                Outcome::None
            }
            Ev::Down(p) => {
                s.pressed = true;
                let p = clamp_to_display(s, display, p);
                match (s.purpose, s.mode) {
                    (Purpose::ColorPick, _) => {
                        return match s.frozen.color_at(p) {
                            Some(c) => Outcome::Finish(OverlayResult::Color(c)),
                            None => Outcome::None,
                        };
                    }
                    (Purpose::Snip, SnipMode::Window) => {}
                    (Purpose::Snip, SnipMode::Freeform) => {
                        s.drag = Some(Drag::Free { display, pts: vec![p] });
                    }
                    (Purpose::Snip, SnipMode::FullScreen) => {
                        if let Some((image, scale)) = s.frozen.full_desktop() {
                            return Outcome::Finish(OverlayResult::Image { image, scale });
                        }
                    }
                    _ => {
                        s.drag = Some(Drag::Rect { display, start: p, cur: p });
                    }
                }
                if let Some(bar) = &s.bar {
                    bar.setHidden(true);
                }
                for l in &s.layers {
                    l.border.setStrokeColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 0.95)));
                    l.border.setLineWidth(1.0);
                }
                Outcome::None
            }
            Ev::Drag(p) => {
                match s.drag.as_mut() {
                    Some(Drag::Rect { display: d, start, cur }) => {
                        let d = *d;
                        let f = s.screens[d].frame;
                        let p = CGPoint::new(
                            p.x.clamp(f.origin.x, f.origin.x + f.size.width),
                            p.y.clamp(f.origin.y, f.origin.y + f.size.height),
                        );
                        *cur = p;
                        let r = rect_from_points(*start, *cur);
                        let lr = rect(r.origin.x - f.origin.x, r.origin.y - f.origin.y, r.size.width, r.size.height);
                        let hole = cgpath_rect(lr);
                        set_hole(s, d, Some(&hole), Some(&hole));
                        show_size_label(s, d, lr);
                    }
                    Some(Drag::Free { display: d, pts }) => {
                        let d = *d;
                        let f = s.screens[d].frame;
                        let p = CGPoint::new(
                            p.x.clamp(f.origin.x, f.origin.x + f.size.width),
                            p.y.clamp(f.origin.y, f.origin.y + f.size.height),
                        );
                        let last = *pts.last().unwrap();
                        if (p.x - last.x).abs() + (p.y - last.y).abs() >= 1.0 {
                            pts.push(p);
                        }
                        let path = CGMutablePath::new();
                        for (i, q) in pts.iter().enumerate() {
                            let (x, y) = (q.x - f.origin.x, q.y - f.origin.y);
                            unsafe {
                                if i == 0 {
                                    CGMutablePath::move_to_point(Some(&path), std::ptr::null(), x, y);
                                } else {
                                    CGMutablePath::add_line_to_point(Some(&path), std::ptr::null(), x, y);
                                }
                            }
                        }
                        CGMutablePath::close_subpath(Some(&path));
                        set_hole(s, d, Some(&path), Some(&path));
                    }
                    None => {}
                }
                Outcome::None
            }
            Ev::Up(p) => {
                if let Some(bar) = &s.bar {
                    bar.setHidden(false);
                }
                s.size_label.setHidden(true);
                match s.drag.take() {
                    Some(Drag::Rect { display: d, start, cur }) => {
                        let r = rect_from_points(start, cur);
                        let tiny = r.size.width < 3.0 || r.size.height < 3.0;
                        if s.purpose == Purpose::Record {
                            // A click (no drag) records the whole display.
                            let screen = s.screens[d];
                            let r = if tiny { screen.frame } else { r };
                            return Outcome::Finish(OverlayResult::Region { screen, rect: r });
                        }
                        if tiny {
                            set_hole(s, d, None, None);
                            return Outcome::None;
                        }
                        match s.frozen.crop_global(r) {
                            Some((image, scale)) => {
                                if s.purpose == Purpose::TextExtract {
                                    Outcome::Finish(OverlayResult::Text { image })
                                } else {
                                    Outcome::Finish(OverlayResult::Image { image, scale })
                                }
                            }
                            None => Outcome::None,
                        }
                    }
                    Some(Drag::Free { display: d, pts }) => {
                        if pts.len() < 4 {
                            set_hole(s, d, None, None);
                            return Outcome::None;
                        }
                        match s.frozen.crop_freeform(&pts) {
                            Some((image, scale)) => Outcome::Finish(OverlayResult::Image { image, scale }),
                            None => Outcome::None,
                        }
                    }
                    None => {
                        let pressed = std::mem::replace(&mut s.pressed, false);
                        if pressed && s.purpose == Purpose::Snip && s.mode == SnipMode::Window {
                            let hit = s.frozen.windows.iter().position(|w| rect_contains(w.frame, p));
                            if let Some(i) = hit {
                                let f = s.frozen.windows[i].frame;
                                if let Some((image, scale)) = s.frozen.crop_global(f) {
                                    return Outcome::Finish(OverlayResult::Image { image, scale });
                                }
                            }
                        }
                        Outcome::None
                    }
                }
            }
        }
    });
    match outcome {
        Outcome::None => {}
        Outcome::Finish(r) => finish(r),
        Outcome::Cancel => cancel(),
    }
}

/// Used by the self-test: render the overlay of display 0 to an image.
pub fn render_first_display() -> Option<CFRetained<CGImage>> {
    SESSION.with(|cell| {
        let guard = cell.borrow();
        let s = guard.as_ref()?;
        let view = &s.views[0];
        let f = s.screens[0].frame;
        let scale = s.screens[0].scale;
        let w = (f.size.width * scale) as usize;
        let h = (f.size.height * scale) as usize;
        let ctx = crate::capture::bitmap_context(w, h, None)?;
        CGContext::scale_ctm(Some(&ctx), scale, scale);
        let layer = view.layer()?;
        layer.renderInContext(&ctx);
        CGBitmapContextCreateImage(Some(&ctx))
    })
}

#[allow(dead_code)]
pub fn simulate(display: usize, down: CGPoint, up: CGPoint) {
    handle(display, Ev::Down(down));
    handle(display, Ev::Drag(CGPoint::new((down.x + up.x) / 2.0, (down.y + up.y) / 2.0)));
    handle(display, Ev::Drag(up));
}

#[allow(dead_code)]
pub fn simulate_up(display: usize, up: CGPoint) {
    handle(display, Ev::Up(up));
}

#[allow(dead_code)]
pub fn simulate_move(display: usize, p: CGPoint) {
    handle(display, Ev::Move(p));
}

#[allow(dead_code)]
pub fn simulate_purpose(p: Purpose) {
    set_purpose(p);
}


/// Self-test: build overlays without putting them on screen.
pub fn set_test_hidden(on: bool) {
    TEST_HIDDEN.with(|t| t.set(on));
}

#[allow(dead_code)]
pub fn simulate_down(display: usize, p: CGPoint) {
    handle(display, Ev::Down(p));
}

#[allow(dead_code)]
pub fn simulate_drag(display: usize, p: CGPoint) {
    handle(display, Ev::Drag(p));
}

#[allow(dead_code)]
pub fn simulate_mode(mode: SnipMode) {
    set_mode(mode);
}

#[allow(dead_code)]
pub fn current_purpose() -> Option<Purpose> {
    SESSION.with(|s| s.borrow().as_ref().map(|s| s.purpose))
}
