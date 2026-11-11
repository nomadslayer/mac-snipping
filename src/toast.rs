//! Windows-style toast: "Snip copied to clipboard — Select here to mark up
//! and share." Appears bottom-right; clicking it opens the editor.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadOnly};
use objc2_app_kit::*;
use objc2_core_graphics::CGImage;

use crate::capture::image_size;
use crate::util::{after, hud_container, label, mtm, rect, secondary_label};

struct Toast {
    window: Retained<NSPanel>,
    on_click: Option<Box<dyn Fn()>>,
}

thread_local! {
    static CURRENT: RefCell<Option<Toast>> = const { RefCell::new(None) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingToastView"]
    pub struct ToastView;

    impl ToastView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _e: &NSEvent) {
            let cb = CURRENT.with(|c| c.borrow_mut().as_mut().and_then(|t| t.on_click.take()));
            dismiss();
            if let Some(cb) = cb {
                cb();
            }
        }
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }
    }
);

pub fn dismiss() {
    if let Some(t) = CURRENT.with(|c| c.borrow_mut().take()) {
        let w = t.window;
        // Quick fade.
        for i in 1..=6 {
            let w = w.clone();
            after(0.025 * i as f64, move || {
                w.setAlphaValue(1.0 - i as f64 / 6.0);
                if i == 6 {
                    w.orderOut(None);
                }
            });
        }
    }
}

pub fn hide_now() {
    if let Some(t) = CURRENT.with(|c| c.borrow_mut().take()) {
        t.window.orderOut(None);
    }
}

pub fn show(thumb: Option<(&CGImage, f64)>, title: &str, subtitle: &str, on_click: Option<Box<dyn Fn()>>) {
    hide_now();
    let m = mtm();
    let width = 380.0;
    let height = if thumb.is_some() { 96.0 } else { 70.0 };

    let screen = NSScreen::mainScreen(m);
    let vis = screen.map(|s| s.visibleFrame()).unwrap_or(rect(0.0, 0.0, 1440.0, 900.0));
    let frame = rect(
        vis.origin.x + vis.size.width - width - 16.0,
        vis.origin.y + 16.0,
        width,
        height,
    );
    let panel = unsafe {
        NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(m),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        panel.setReleasedWhenClosed(false);
        panel.setLevel(NSStatusWindowLevel);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);
        panel.setHidesOnDeactivate(false);
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
    }

    let content = {
        let this = ToastView::alloc(m).set_ivars(());
        let v: Retained<ToastView> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, width, height)] };
        v
    };
    let bg = hud_container(rect(0.0, 0.0, width, height), 14.0);
    content.addSubview(&bg);

    let mut text_x = 16.0;
    if let Some((img, scale)) = thumb {
        let (w, h) = image_size(img);
        let ns_img = crate::export::nsimage(img, scale);
        let box_w = 104.0;
        let box_h = 64.0;
        let aspect = w as f64 / h.max(1) as f64;
        let (tw, th) = if aspect > box_w / box_h { (box_w, box_w / aspect) } else { (box_h * aspect, box_h) };
        let iv = NSImageView::imageViewWithImage(&ns_img, m);
        iv.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
        iv.setFrame(rect(16.0 + (box_w - tw) / 2.0, (height - th) / 2.0, tw, th));
        iv.setWantsLayer(true);
        if let Some(l) = iv.layer() {
            l.setCornerRadius(4.0);
            l.setMasksToBounds(true);
            l.setBorderWidth(0.5);
            l.setBorderColor(Some(&objc2_core_graphics::CGColor::new_srgb(0.5, 0.5, 0.5, 0.6)));
        }
        content.addSubview(&iv);
        text_x = 16.0 + box_w + 14.0;
    }
    let t = label(title, 13.0, true);
    let s = secondary_label(subtitle, 12.0);
    s.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    s.setMaximumNumberOfLines(2);
    let tw = width - text_x - 36.0;
    let title_h = 18.0;
    let sub_h = 32.0;
    let block = title_h + 2.0 + sub_h;
    let top = (height + block) / 2.0;
    t.setFrame(rect(text_x, top - title_h, tw, title_h));
    s.setFrame(rect(text_x, top - title_h - 2.0 - sub_h, tw, sub_h));
    content.addSubview(&t);
    content.addSubview(&s);

    let close = crate::util::icon_button("xmark", "Dismiss", dismiss);
    close.setBordered(false);
    close.setFrame(rect(width - 30.0, height - 28.0, 22.0, 22.0));
    content.addSubview(&close);

    panel.setContentView(Some(&content));
    panel.setAlphaValue(1.0);
    panel.orderFrontRegardless();

    let gen = GENERATION.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    CURRENT.with(|c| *c.borrow_mut() = Some(Toast { window: panel, on_click }));
    after(6.0, move || {
        if GENERATION.with(|g| g.get()) == gen {
            dismiss();
        }
    });
}

pub fn current_window() -> Option<Retained<NSPanel>> {
    CURRENT.with(|c| c.borrow().as_ref().map(|t| t.window.clone()))
}
