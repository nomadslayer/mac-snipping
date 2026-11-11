//! The Snipping Tool window: toolbar (New / mode / delay, markup tools,
//! undo/redo, zoom, save, copy, share, more) + zoomable canvas.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::*;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use objc2_foundation::*;

use crate::app::{self, Origin};
use crate::canvas::{CanvasKey, CanvasView, Tool};
use crate::export;
use crate::settings::{self, SnipMode};
use crate::toolopts;
use crate::util::{
    self, fixed_size, hstack, icon_button, is_on, label, menu, menu_item, mtm, ns, on_action,
    on_click, rect, secondary_label, set_on, symbol, text_icon_button, timestamp_name,
    toggle_button, vstack,
};

const MIN_W: f64 = 880.0;
const MIN_H: f64 = 420.0;

thread_local! {
    static EDITORS: RefCell<Vec<Rc<Editor>>> = const { RefCell::new(Vec::new()) };
    static LAST_KEY: RefCell<Weak<Editor>> = RefCell::new(Weak::new());
}

// ---------------------------------------------------------------------------
// Objective-C helper classes
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super = NSClipView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingCenteringClipView"]
    pub struct CenteringClipView;

    impl CenteringClipView {
        #[unsafe(method(constrainBoundsRect:))]
        fn constrain_bounds(&self, proposed: NSRect) -> NSRect {
            let mut r: NSRect = unsafe { msg_send![super(self), constrainBoundsRect: proposed] };
            if let Some(doc) = self.documentView() {
                let df = doc.frame();
                if r.size.width > df.size.width {
                    r.origin.x = df.origin.x + (df.size.width - r.size.width) / 2.0;
                }
                if r.size.height > df.size.height {
                    r.origin.y = df.origin.y + (df.size.height - r.size.height) / 2.0;
                }
            }
            r
        }
    }
);

pub struct TbIvars {
    items: RefCell<Vec<(String, Retained<NSToolbarItem>)>>,
    order: RefCell<Vec<String>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingToolbarDelegate"]
    #[ivars = TbIvars]
    pub struct ToolbarDelegate;

    unsafe impl NSObjectProtocol for ToolbarDelegate {}

    unsafe impl NSToolbarDelegate for ToolbarDelegate {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn item_for(
            &self,
            _toolbar: &NSToolbar,
            ident: &NSString,
            _insert: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            let id = ident.to_string();
            self.ivars().items.borrow().iter().find(|(k, _)| *k == id).map(|(_, v)| v.clone())
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_ids(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            let ids: Vec<Retained<NSString>> = self.ivars().order.borrow().iter().map(|s| ns(s)).collect();
            NSArray::from_retained_slice(&ids)
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_ids(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            let ids: Vec<Retained<NSString>> = self.ivars().order.borrow().iter().map(|s| ns(s)).collect();
            NSArray::from_retained_slice(&ids)
        }
    }
);

pub struct DelegateIvars {
    editor: RefCell<Weak<Editor>>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingEditorWindowDelegate"]
    #[ivars = DelegateIvars]
    pub struct EditorWindowDelegate;

    unsafe impl NSObjectProtocol for EditorWindowDelegate {}

    unsafe impl NSWindowDelegate for EditorWindowDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            match self.ivars().editor.borrow().upgrade() {
                Some(ed) => ed.should_close(),
                None => true,
            }
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            let ed = self.ivars().editor.borrow().upgrade();
            if let Some(ed) = ed {
                ed.closed();
            }
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            if let Some(ed) = self.ivars().editor.borrow().upgrade() {
                LAST_KEY.with(|k| *k.borrow_mut() = Rc::downgrade(&ed));
                ed.sync_capture_controls();
            }
        }
    }
);

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

pub struct Editor {
    pub window: Retained<NSWindow>,
    pub canvas: Retained<CanvasView>,
    scroll: Retained<NSScrollView>,
    empty: Retained<NSStackView>,
    bottom: Retained<NSVisualEffectView>,
    bottom_content: RefCell<Option<Retained<NSView>>>,
    _tb_delegate: Retained<ToolbarDelegate>,
    _win_delegate: Retained<EditorWindowDelegate>,
    tools_item: Retained<NSToolbarItem>,
    output_item: Retained<NSToolbarItem>,
    tool_buttons: Vec<(Tool, Retained<NSButton>)>,
    ruler_button: Retained<NSButton>,
    crop_button: Retained<NSButton>,
    text_button: Retained<NSButton>,
    undo_button: Retained<NSButton>,
    redo_button: Retained<NSButton>,
    zoom_button: Retained<NSButton>,
    share_button: Retained<NSButton>,
    more_button: Retained<NSButton>,
    segment: Retained<NSSegmentedControl>,
    mode_popup: Retained<NSPopUpButton>,
    delay_popup: Retained<NSPopUpButton>,
    pub saved_path: RefCell<Option<PathBuf>>,
    me: RefCell<Weak<Editor>>,
    closing: Cell<bool>,
    copy_gen: Cell<u64>,
    popover: RefCell<Option<Retained<NSPopover>>>,
    /// Bumped to cancel an in-flight text scan.
    ocr_gen: Cell<u64>,
    ocr_pending: Cell<bool>,
    observer: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>>,
}

fn as_view<T: objc2::Message>(v: &Retained<T>) -> &NSView
where
    T: std::ops::Deref,
{
    unsafe { &*(&**v as *const T as *const NSView) }
}

fn separator() -> Retained<NSBox> {
    let b = NSBox::new(mtm());
    b.setBoxType(NSBoxType::Separator);
    fixed_size(&b, 1.0, 22.0);
    b
}

impl Editor {
    pub fn new() -> Rc<Editor> {
        let m = mtm();
        let ed = Rc::new_cyclic(|weak: &Weak<Editor>| {
            let window = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(m),
                    rect(0.0, 0.0, 900.0, 520.0),
                    NSWindowStyleMask::Titled
                        | NSWindowStyleMask::Closable
                        | NSWindowStyleMask::Miniaturizable
                        | NSWindowStyleMask::Resizable,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { window.setReleasedWhenClosed(false) };
            window.setTitle(&ns("Snipping"));
            window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
            window.setToolbarStyle(NSWindowToolbarStyle::Unified);
            window.setContentMinSize(NSSize::new(MIN_W, MIN_H));
            window.setTabbingMode(NSWindowTabbingMode::Disallowed);

            // --- Canvas inside a magnifiable scroll view ---------------------
            let canvas = CanvasView::new();
            let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(m), rect(0.0, 0.0, 900.0, 520.0));
            let clip = {
                let this = CenteringClipView::alloc(m).set_ivars(());
                let c: Retained<CenteringClipView> =
                    unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 900.0, 520.0)] };
                c
            };
            scroll.setContentView(&clip);
            scroll.setHasVerticalScroller(true);
            scroll.setHasHorizontalScroller(true);
            scroll.setAutohidesScrollers(true);
            scroll.setAllowsMagnification(true);
            scroll.setMinMagnification(0.05);
            scroll.setMaxMagnification(16.0);
            scroll.setDrawsBackground(true);
            scroll.setBackgroundColor(&NSColor::underPageBackgroundColor());
            scroll.setDocumentView(Some(&canvas));
            scroll.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
            canvas.setWantsLayer(true);
            if let Some(l) = canvas.layer() {
                l.setShadowColor(Some(&objc2_core_graphics::CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
                l.setShadowOpacity(0.25);
                l.setShadowRadius(8.0);
                l.setShadowOffset(objc2_core_foundation::CGSize::new(0.0, -2.0));
            }

            // --- Empty state --------------------------------------------------
            let s = settings::get();
            let icon = NSImageView::imageViewWithImage(&symbol("scissors", 46.0), m);
            icon.setContentTintColor(Some(&NSColor::tertiaryLabelColor()));
            let headline = label(
                &format!("Press {} to start a snip.", s.hotkey_snip.display()),
                17.0,
                false,
            );
            let sub = secondary_label(
                &format!(
                    "Or select New (⌘N). Press {} to record your screen, or {} to grab text.",
                    s.hotkey_record.display(),
                    s.hotkey_text.display()
                ),
                12.5,
            );
            let empty = vstack(&[&icon, &headline, &sub], 10.0);
            empty.setAlignment(NSLayoutAttribute::CenterX);

            // --- Floating bottom bar (crop / text actions) ---------------------
            let bottom = util::hud_container(rect(0.0, 0.0, 10.0, 10.0), 12.0);
            bottom.setHidden(true);

            let content = NSView::initWithFrame(NSView::alloc(m), rect(0.0, 0.0, 900.0, 520.0));
            content.addSubview(&scroll);
            content.addSubview(&empty);
            content.addSubview(&bottom);
            empty.setTranslatesAutoresizingMaskIntoConstraints(false);
            empty.centerXAnchor().constraintEqualToAnchor(&content.centerXAnchor()).setActive(true);
            empty.centerYAnchor().constraintEqualToAnchor(&content.centerYAnchor()).setActive(true);
            window.setContentView(Some(&content));
            scroll.setFrame(content.bounds());

            // --- Toolbar controls -----------------------------------------------
            let w = weak.clone();
            let segment = unsafe {
                NSSegmentedControl::segmentedControlWithImages_trackingMode_target_action(
                    &NSArray::from_retained_slice(&[symbol("camera", 14.0), symbol("video", 14.0)]),
                    NSSegmentSwitchTracking::SelectOne,
                    None,
                    None,
                    m,
                )
            };
            segment.setToolTip_forSegment(Some(&ns("Snip")), 0);
            segment.setToolTip_forSegment(Some(&ns("Record")), 1);
            segment.setSelectedSegment(0);
            {
                let w = w.clone();
                on_click(&segment, move || {
                    if let Some(ed) = w.upgrade() {
                        ed.sync_capture_controls();
                    }
                });
            }

            let new_button = {
                let w = w.clone();
                text_icon_button("New", "plus", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.new_capture();
                    }
                })
            };
            new_button.setBezelStyle(NSBezelStyle::Toolbar);
            new_button.setToolTip(Some(&ns("New snip (⌘N)")));

            let mode_popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(m), rect(0.0, 0.0, 150.0, 26.0), false);
            for mode in SnipMode::ALL {
                mode_popup.addItemWithTitle(&ns(mode.title()));
                if let Some(item) = mode_popup.lastItem() {
                    item.setImage(Some(&symbol(mode.symbol(), 13.0)));
                }
            }
            mode_popup.setBezelStyle(NSBezelStyle::Toolbar);
            mode_popup.setToolTip(Some(&ns("Snipping mode")));
            on_action(&mode_popup, |sender| {
                if let Some(p) = sender.and_then(|s| s.downcast_ref::<NSPopUpButton>()) {
                    let idx = p.indexOfSelectedItem().clamp(0, 3) as usize;
                    settings::update(|s| s.snip_mode = SnipMode::ALL[idx]);
                }
            });

            let delay_popup = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(m), rect(0.0, 0.0, 130.0, 26.0), false);
            for (i, t) in ["No delay", "3 seconds", "5 seconds", "10 seconds"].iter().enumerate() {
                delay_popup.addItemWithTitle(&ns(t));
                if let Some(item) = delay_popup.lastItem() {
                    item.setImage(Some(&symbol(if i == 0 { "timer" } else { "clock" }, 13.0)));
                }
            }
            delay_popup.setBezelStyle(NSBezelStyle::Toolbar);
            delay_popup.setToolTip(Some(&ns("Snip delay")));
            on_action(&delay_popup, |sender| {
                if let Some(p) = sender.and_then(|s| s.downcast_ref::<NSPopUpButton>()) {
                    let d = [0, 3, 5, 10][p.indexOfSelectedItem().clamp(0, 3) as usize];
                    settings::update(|s| s.delay = d);
                }
            });

            // Markup tools.
            let mk_tool = |tool: Tool, sym: &str, tip: &str| {
                let w = w.clone();
                let b = toggle_button(sym, tip, move || {
                    if let Some(ed) = w.upgrade() {
                        ed.tool_clicked(tool);
                    }
                });
                fixed_size(&b, 36.0, 0.0);
                b
            };
            let tool_buttons = vec![
                (Tool::Pen, mk_tool(Tool::Pen, "pencil", "Ballpoint pen")),
                (Tool::Highlighter, mk_tool(Tool::Highlighter, "highlighter", "Highlighter")),
                (Tool::Eraser, mk_tool(Tool::Eraser, "eraser", "Eraser")),
                (Tool::Shape, mk_tool(Tool::Shape, "square.on.circle", "Shapes")),
            ];
            let ruler_button = {
                let w = w.clone();
                toggle_button("ruler", "Ruler / Protractor", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.ruler_clicked();
                    }
                })
            };
            fixed_size(&ruler_button, 36.0, 0.0);
            let crop_button = {
                let w = w.clone();
                toggle_button("crop", "Image crop", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.crop_clicked();
                    }
                })
            };
            fixed_size(&crop_button, 36.0, 0.0);
            let text_button = {
                let w = w.clone();
                toggle_button("text.viewfinder", "Text actions", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.text_actions_clicked();
                    }
                })
            };
            fixed_size(&text_button, 36.0, 0.0);
            let undo_button = {
                let w = w.clone();
                icon_button("arrow.uturn.backward", "Undo (⌘Z)", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.undo();
                    }
                })
            };
            let redo_button = {
                let w = w.clone();
                icon_button("arrow.uturn.forward", "Redo (⇧⌘Z)", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.redo();
                    }
                })
            };

            let zoom_button = {
                let w = w.clone();
                let b = unsafe { NSButton::buttonWithTitle_target_action(&ns("100%"), None, None, m) };
                b.setBezelStyle(NSBezelStyle::Toolbar);
                b.setImage(Some(&symbol("plus.magnifyingglass", 13.0)));
                b.setImagePosition(NSCellImagePosition::ImageLeading);
                b.setToolTip(Some(&ns("Zoom")));
                on_click(&b, move || {
                    if let Some(ed) = w.upgrade() {
                        ed.show_zoom_menu();
                    }
                });
                b
            };
            let save_button = {
                let w = w.clone();
                icon_button("square.and.arrow.down", "Save as (⌘S)", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.save_as();
                    }
                })
            };
            let copy_button = {
                let w = w.clone();
                icon_button("doc.on.doc", "Copy (⌘C)", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.copy();
                    }
                })
            };
            let share_button = {
                let w = w.clone();
                icon_button("square.and.arrow.up", "Share", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.share();
                    }
                })
            };
            let more_button = {
                let w = w.clone();
                icon_button("ellipsis", "See more", move || {
                    if let Some(ed) = w.upgrade() {
                        ed.show_more_menu();
                    }
                })
            };

            // --- Toolbar items --------------------------------------------------
            let capture_stack = hstack(&[&segment, &new_button, &mode_popup, &delay_popup], 6.0);
            let mut tool_views: Vec<&NSView> = tool_buttons.iter().map(|(_, b)| as_view(b)).collect();
            tool_views.push(&ruler_button);
            tool_views.push(&crop_button);
            tool_views.push(&text_button);
            let sep = separator();
            tool_views.push(&sep);
            tool_views.push(&undo_button);
            tool_views.push(&redo_button);
            let tools_stack = hstack(&tool_views, 2.0);
            let output_stack = hstack(&[&zoom_button, &save_button, &copy_button, &share_button], 2.0);
            let more_stack = hstack(&[&more_button], 0.0);

            let make_item = |id: &str, label_: &str, view: &NSView| {
                let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(m), &ns(id));
                item.setView(Some(view));
                item.setLabel(&ns(label_));
                item.setPaletteLabel(&ns(label_));
                item
            };
            let capture_item = make_item("capture", "Capture", &capture_stack);
            let tools_item = make_item("tools", "Markup", &tools_stack);
            let output_item = make_item("output", "Output", &output_stack);
            let more_item = make_item("more", "More", &more_stack);
            capture_item.setVisibilityPriority(NSToolbarItemVisibilityPriorityHigh);
            more_item.setVisibilityPriority(NSToolbarItemVisibilityPriorityHigh);

            let flex = unsafe { NSToolbarFlexibleSpaceItemIdentifier }.to_string();
            let tb_delegate = {
                let this = ToolbarDelegate::alloc(m).set_ivars(TbIvars {
                    items: RefCell::new(vec![
                        ("capture".into(), capture_item),
                        ("tools".into(), tools_item.clone()),
                        ("output".into(), output_item.clone()),
                        ("more".into(), more_item),
                    ]),
                    order: RefCell::new(vec![
                        "capture".into(),
                        flex.clone(),
                        "tools".into(),
                        flex,
                        "output".into(),
                        "more".into(),
                    ]),
                });
                let d: Retained<ToolbarDelegate> = unsafe { msg_send![super(this), init] };
                d
            };
            let toolbar = NSToolbar::initWithIdentifier(NSToolbar::alloc(m), &ns("SnippingEditorToolbar"));
            toolbar.setDelegate(Some(ProtocolObject::from_ref(&*tb_delegate)));
            toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
            toolbar.setAllowsUserCustomization(false);
            toolbar.setCenteredItemIdentifiers(&NSSet::from_retained_slice(&[ns("tools")]));
            window.setToolbar(Some(&toolbar));

            let win_delegate = {
                let this = EditorWindowDelegate::alloc(m).set_ivars(DelegateIvars {
                    editor: RefCell::new(weak.clone()),
                });
                let d: Retained<EditorWindowDelegate> = unsafe { msg_send![super(this), init] };
                d
            };
            window.setDelegate(Some(ProtocolObject::from_ref(&*win_delegate)));

            Editor {
                window,
                canvas,
                scroll,
                empty,
                bottom,
                bottom_content: RefCell::new(None),
                _tb_delegate: tb_delegate,
                _win_delegate: win_delegate,
                tools_item,
                output_item,
                tool_buttons,
                ruler_button,
                crop_button,
                text_button,
                undo_button,
                redo_button,
                zoom_button,
                share_button,
                more_button,
                segment,
                mode_popup,
                delay_popup,
                saved_path: RefCell::new(None),
                me: RefCell::new(weak.clone()),
                closing: Cell::new(false),
                copy_gen: Cell::new(0),
                popover: RefCell::new(None),
                ocr_gen: Cell::new(0),
                ocr_pending: Cell::new(false),
                observer: RefCell::new(None),
            }
        });

        // Canvas hooks.
        {
            let w = Rc::downgrade(&ed);
            ed.canvas.ivars().hooks.borrow_mut().changed = Some(Box::new(move || {
                if let Some(ed) = w.upgrade() {
                    ed.refresh_state();
                }
            }));
            let w = Rc::downgrade(&ed);
            ed.canvas.ivars().hooks.borrow_mut().key = Some(Box::new(move |k| {
                if let Some(ed) = w.upgrade() {
                    ed.canvas_key(k);
                }
            }));
        }
        // Keep the zoom label in sync with pinch-zoom.
        {
            let w = Rc::downgrade(&ed);
            let block = RcBlock::new(move |_n: std::ptr::NonNull<NSNotification>| {
                if let Some(ed) = w.upgrade() {
                    ed.update_zoom_label();
                }
            });
            let token = unsafe {
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(NSScrollViewDidEndLiveMagnifyNotification),
                    Some(&ed.scroll),
                    None,
                    &block,
                )
            };
            *ed.observer.borrow_mut() = Some(token);
        }

        ed.sync_capture_controls();
        ed.show_empty(true);
        ed.window.center();
        EDITORS.with(|e| e.borrow_mut().push(ed.clone()));
        ed
    }

    fn me(&self) -> Weak<Editor> {
        self.me.borrow().clone()
    }

    pub fn weak(&self) -> Weak<Editor> {
        self.me()
    }

    pub fn has_image(&self) -> bool {
        self.canvas.has_image()
    }

    pub fn is_dirty(&self) -> bool {
        self.canvas.is_dirty()
    }

    pub fn show(&self) {
        app::ensure_regular();
        self.window.makeKeyAndOrderFront(None);
        app::activate();
    }

    fn show_empty(&self, empty: bool) {
        self.empty.setHidden(!empty);
        self.scroll.setHidden(empty);
        self.tools_item.setHidden(empty);
        self.output_item.setHidden(empty);
        self.hide_bottom();
    }

    /// Load a snip into this window (resets markup).
    pub fn load(&self, image: CFRetained<CGImage>, scale: f64) {
        self.end_modes();
        self.canvas.set_image(image, scale);
        *self.saved_path.borrow_mut() = None;
        self.window.setRepresentedURL(None);
        self.show_empty(false);
        for (_, b) in &self.tool_buttons {
            set_on(b, false);
        }
        let tool = self.canvas.tool();
        self.select_tool_buttons(tool);
        set_on(&self.ruler_button, false);
        set_on(&self.crop_button, false);
        set_on(&self.text_button, false);
        self.fit_window_to_image();
        self.refresh_state();
    }

    pub fn set_saved_path(&self, path: Option<PathBuf>) {
        if let Some(p) = &path {
            self.window.setRepresentedURL(Some(&util::file_url(p)));
            self.window.setTitle(&ns(&p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()));
        }
        *self.saved_path.borrow_mut() = path;
    }

    fn fit_window_to_image(&self) {
        let size = self.canvas.image_size_pt();
        let screen = self.window.screen().or_else(|| NSScreen::mainScreen(mtm()));
        let vis = screen.map(|s| s.visibleFrame()).unwrap_or(rect(0.0, 0.0, 1440.0, 900.0));
        let frame = self.window.frame();
        let content = self.window.contentLayoutRect();
        let chrome_h = frame.size.height - content.size.height;
        let pad = 64.0;
        let w = (size.width + pad).clamp(MIN_W, vis.size.width * 0.92);
        let h = (size.height + pad + chrome_h).clamp(MIN_H + chrome_h, vis.size.height * 0.92);
        // Keep the window's top-center where it is when visible; else center.
        let (x, y) = if self.window.isVisible() {
            let cx = frame.origin.x + frame.size.width / 2.0;
            let top = frame.origin.y + frame.size.height;
            (cx - w / 2.0, top - h)
        } else {
            (vis.origin.x + (vis.size.width - w) / 2.0, vis.origin.y + (vis.size.height - h) / 2.0)
        };
        let x = x.clamp(vis.origin.x, vis.origin.x + vis.size.width - w);
        let y = y.clamp(vis.origin.y, vis.origin.y + vis.size.height - h);
        self.window.setFrame_display(rect(x, y, w, h), true);
        self.zoom_fit();
    }

    // --- Capture controls ---------------------------------------------------

    pub fn sync_capture_controls(&self) {
        let s = settings::get();
        let idx = SnipMode::ALL.iter().position(|m| *m == s.snip_mode).unwrap_or(0);
        self.mode_popup.selectItemAtIndex(idx as isize);
        self.delay_popup
            .selectItemAtIndex([0, 3, 5, 10].iter().position(|d| *d == s.delay).unwrap_or(0) as isize);
        let record = self.segment.selectedSegment() == 1;
        self.mode_popup.setEnabled(!record);
        self.delay_popup.setEnabled(!record);
    }

    /// Self-test helpers.
    pub(crate) fn tools_enabled(&self) -> bool {
        self.tool_buttons.iter().all(|(_, b)| b.isEnabled())
    }
    pub(crate) fn text_button_on(&self) -> bool {
        is_on(&self.text_button)
    }

    pub fn recording_selected(&self) -> bool {
        self.segment.selectedSegment() == 1
    }

    pub fn new_capture(&self) {
        let origin = Origin::Editor(self.me());
        if self.recording_selected() {
            app::start_record(origin);
        } else {
            app::start_snip(origin, None);
        }
    }

    // --- Tools ----------------------------------------------------------------

    fn select_tool_buttons(&self, tool: Tool) {
        for (t, b) in &self.tool_buttons {
            set_on(b, *t == tool);
        }
    }

    /// Leave crop / text-actions mode (and cancel a running text scan).
    fn end_modes(&self) {
        if self.canvas.tool() == Tool::Crop || self.canvas.st().crop.is_some() {
            self.canvas.cancel_crop();
        }
        let had_ocr = self.canvas.st().ocr.is_some();
        if had_ocr {
            self.canvas.clear_ocr();
        }
        self.ocr_gen.set(self.ocr_gen.get() + 1);
        self.ocr_pending.set(false);
        set_on(&self.crop_button, false);
        set_on(&self.text_button, false);
        self.close_popover();
        self.hide_bottom();
        self.set_tools_enabled(true);
    }

    fn close_popover(&self) {
        if let Some(p) = self.popover.borrow_mut().take() {
            p.close();
        }
        toolopts::detach_color_panel();
    }

    pub(crate) fn tool_clicked(&self, tool: Tool) {
        let current = self.canvas.tool();
        let button = self.tool_buttons.iter().find(|(t, _)| *t == tool).map(|(_, b)| b.clone());
        if current == tool {
            // Second click on the active tool opens its options (Windows flyout).
            if let Some(b) = &button {
                set_on(b, true);
                self.show_tool_options(tool, b);
            }
            return;
        }
        self.end_modes();
        self.canvas.set_tool(tool);
        self.select_tool_buttons(tool);
        set_on(&self.ruler_button, self.canvas.ruler_visible() || self.canvas.protractor_visible());
        self.window.makeFirstResponder(Some(&self.canvas));
    }

    fn show_tool_options(&self, tool: Tool, anchor: &NSButton) {
        let w = self.me();
        let pop = match tool {
            Tool::Pen => toolopts::pen_popover(self.canvas.clone(), false),
            Tool::Highlighter => toolopts::pen_popover(self.canvas.clone(), true),
            Tool::Shape => toolopts::shape_popover(self.canvas.clone(), |_| {}),
            Tool::Eraser => toolopts::eraser_popover(self.canvas.clone(), move || {
                if let Some(ed) = w.upgrade() {
                    ed.refresh_state();
                }
            }),
            _ => return,
        };
        if let Some(old) = self.popover.borrow_mut().take() {
            old.close();
        }
        pop.showRelativeToRect_ofView_preferredEdge(anchor.bounds(), anchor, NSRectEdge::MaxY);
        // Keep it alive while shown.
        *self.popover.borrow_mut() = Some(pop);
    }

    fn ruler_clicked(&self) {
        // Windows: the ruler button toggles the ruler; the flyout offers the protractor.
        let m = menu("Ruler");
        let w = self.me();
        let ruler = menu_item("Ruler", "", move || {
            if let Some(ed) = w.upgrade() {
                ed.canvas.toggle_ruler();
                ed.sync_ruler_button();
            }
        });
        ruler.setState(if self.canvas.ruler_visible() { NSControlStateValueOn } else { NSControlStateValueOff });
        let w = self.me();
        let prot = menu_item("Protractor", "", move || {
            if let Some(ed) = w.upgrade() {
                ed.canvas.toggle_protractor();
                ed.sync_ruler_button();
            }
        });
        prot.setState(if self.canvas.protractor_visible() { NSControlStateValueOn } else { NSControlStateValueOff });
        m.addItem(&ruler);
        m.addItem(&prot);
        m.addItem(&NSMenuItem::separatorItem(mtm()));
        let hint = NSMenuItem::new(mtm());
        hint.setTitle(&ns("Scroll over it to rotate · drag to move"));
        hint.setEnabled(false);
        m.addItem(&hint);
        self.sync_ruler_button();
        let b = &self.ruler_button;
        m.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0.0, b.bounds().size.height + 4.0), Some(b));
        self.sync_ruler_button();
    }

    fn sync_ruler_button(&self) {
        set_on(&self.ruler_button, self.canvas.ruler_visible() || self.canvas.protractor_visible());
    }

    pub(crate) fn set_tools_enabled(&self, enabled: bool) {
        for (_, b) in &self.tool_buttons {
            b.setEnabled(enabled);
        }
        self.ruler_button.setEnabled(enabled);
        self.text_button.setEnabled(enabled || is_on(&self.text_button));
        self.crop_button.setEnabled(enabled || is_on(&self.crop_button));
    }

    pub(crate) fn crop_clicked(&self) {
        if self.canvas.tool() == Tool::Crop {
            self.cancel_crop();
            return;
        }
        self.end_modes();
        self.select_tool_buttons(Tool::None);
        self.canvas.begin_crop();
        set_on(&self.crop_button, true);
        self.set_tools_enabled(false);
        let w = self.me();
        let cancel = text_icon_button("Cancel", "xmark", move || {
            if let Some(ed) = w.upgrade() {
                ed.cancel_crop();
            }
        });
        let w = self.me();
        let apply = text_icon_button("Apply", "checkmark", move || {
            if let Some(ed) = w.upgrade() {
                ed.apply_crop();
            }
        });
        apply.setKeyEquivalent(&ns("\r"));
        let hint = secondary_label("Drag the handles to crop.", 12.0);
        let bar = hstack(&[&hint, &cancel, &apply], 10.0);
        self.show_bottom(&bar);
        self.window.makeFirstResponder(Some(&self.canvas));
    }

    fn cancel_crop(&self) {
        self.canvas.cancel_crop();
        set_on(&self.crop_button, false);
        self.set_tools_enabled(true);
        self.hide_bottom();
    }

    fn apply_crop(&self) {
        self.canvas.apply_crop();
        set_on(&self.crop_button, false);
        self.set_tools_enabled(true);
        self.hide_bottom();
        self.zoom_fit();
    }

    pub(crate) fn text_actions_clicked(&self) {
        // Second click (results shown or scan running) closes text actions.
        if self.canvas.st().ocr.is_some() || self.ocr_pending.get() {
            self.end_modes();
            return;
        }
        self.end_modes();
        self.canvas.set_tool(Tool::None);
        self.select_tool_buttons(Tool::None);
        set_on(&self.text_button, true);
        let Some(img) = self.canvas.base_image() else { return };
        let gen = self.ocr_gen.get() + 1;
        self.ocr_gen.set(gen);
        self.ocr_pending.set(true);
        let spinner = NSProgressIndicator::new(mtm());
        spinner.setStyle(NSProgressIndicatorStyle::Spinning);
        spinner.setControlSize(NSControlSize::Small);
        unsafe { spinner.startAnimation(None) };
        let bar = hstack(&[&spinner, &label("Looking for text…", 12.5, false)], 8.0);
        self.show_bottom(&bar);
        let w = self.me();
        crate::ocr::recognize(img, move |lines| {
            let Some(ed) = w.upgrade() else { return };
            // Ignore results from a cancelled scan (user switched tool/mode).
            if ed.ocr_gen.get() != gen || !ed.ocr_pending.get() {
                return;
            }
            ed.ocr_pending.set(false);
            if lines.is_empty() {
                let w2 = Rc::downgrade(&ed);
                let close = icon_button("xmark", "Close", move || {
                    if let Some(ed) = w2.upgrade() {
                        ed.end_modes();
                    }
                });
                let bar = hstack(&[&label("No text found in this snip.", 12.5, false), &close], 10.0);
                ed.show_bottom(&bar);
                return;
            }
            ed.canvas.set_ocr(lines);
            ed.show_text_bar(None);
            ed.window.makeFirstResponder(Some(&ed.canvas));
        });
    }

    fn show_text_bar(&self, note: Option<&str>) {
        let w = self.me();
        let copy_all = text_icon_button("Copy all text", "doc.on.doc", move || {
            if let Some(ed) = w.upgrade() {
                if let Some(t) = ed.canvas.ocr_text(false) {
                    export::copy_text(&t);
                    ed.show_text_bar(Some("Text copied"));
                }
            }
        });
        let w = self.me();
        let redact = text_icon_button("Quick redact", "eye.slash", move || {
            if let Some(ed) = w.upgrade() {
                ed.show_redact_menu();
            }
        });
        let w = self.me();
        let close = icon_button("xmark", "Close text actions (Esc)", move || {
            if let Some(ed) = w.upgrade() {
                ed.end_modes();
            }
        });
        let hint = secondary_label(
            note.unwrap_or("Click or drag over text to select it, then press ⌘C."),
            12.0,
        );
        let bar = hstack(&[&hint, &copy_all, &redact, &close], 10.0);
        self.show_bottom(&bar);
    }

    fn show_redact_menu(&self) {
        let (emails, phones) = self.canvas.sensitive_count();
        let m = menu("Quick redact");
        let w = self.me();
        let all = menu_item(&format!("Redact all ({})", emails + phones), "", move || {
            if let Some(ed) = w.upgrade() {
                let n = ed.canvas.redact(true, true);
                ed.show_text_bar(Some(&format!("Redacted {n} item(s)")));
            }
        });
        all.setEnabled(emails + phones > 0);
        let w = self.me();
        let em = menu_item(&format!("Email addresses ({emails})"), "", move || {
            if let Some(ed) = w.upgrade() {
                let n = ed.canvas.redact(true, false);
                ed.show_text_bar(Some(&format!("Redacted {n} email address(es)")));
            }
        });
        em.setEnabled(emails > 0);
        let w = self.me();
        let ph = menu_item(&format!("Phone numbers ({phones})"), "", move || {
            if let Some(ed) = w.upgrade() {
                let n = ed.canvas.redact(false, true);
                ed.show_text_bar(Some(&format!("Redacted {n} phone number(s)")));
            }
        });
        ph.setEnabled(phones > 0);
        m.setAutoenablesItems(false);
        m.addItem(&all);
        m.addItem(&em);
        m.addItem(&ph);
        let anchor: &NSView = &self.bottom;
        m.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(anchor.bounds().size.width - 160.0, anchor.bounds().size.height), Some(anchor));
    }

    fn show_bottom(&self, content: &NSView) {
        if let Some(old) = self.bottom_content.borrow_mut().take() {
            old.removeFromSuperview();
        }
        let size = content.fittingSize();
        let pad = 10.0;
        let w = size.width + 2.0 * pad;
        let h = size.height.max(28.0) + 2.0 * pad;
        let parent = self.window.contentView().unwrap();
        let pb = parent.bounds();
        self.bottom.setFrame(rect(((pb.size.width - w) / 2.0).round(), 18.0, w, h));
        self.bottom.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin,
        );
        content.setFrame(rect(pad, (h - size.height) / 2.0, size.width, size.height));
        self.bottom.addSubview(content);
        self.bottom.setHidden(false);
        *self.bottom_content.borrow_mut() = Some(unsafe { Retained::retain(content as *const NSView as *mut NSView).unwrap() });
    }

    fn hide_bottom(&self) {
        if let Some(old) = self.bottom_content.borrow_mut().take() {
            old.removeFromSuperview();
        }
        self.bottom.setHidden(true);
    }

    fn canvas_key(&self, key: CanvasKey) {
        match key {
            CanvasKey::Escape => {
                if self.canvas.tool() == Tool::Crop {
                    self.cancel_crop();
                } else if self.canvas.st().ocr.is_some() {
                    self.end_modes();
                } else {
                    self.canvas.set_tool(Tool::None);
                    self.select_tool_buttons(Tool::None);
                }
            }
            CanvasKey::Return => {
                if self.canvas.tool() == Tool::Crop {
                    self.apply_crop();
                }
            }
        }
    }

    pub fn refresh_state(&self) {
        self.undo_button.setEnabled(self.canvas.can_undo());
        self.redo_button.setEnabled(self.canvas.can_redo());
        self.window.setDocumentEdited(self.canvas.is_dirty());
        if settings::get().auto_copy && self.canvas.is_dirty() {
            // Windows: "Automatically copy changes" keeps the clipboard in sync.
            // Debounced so drawing stays instant on large snips.
            let gen = self.copy_gen.get() + 1;
            self.copy_gen.set(gen);
            let w = self.me();
            util::after(0.6, move || {
                if let Some(ed) = w.upgrade() {
                    if ed.copy_gen.get() == gen {
                        if let Some(img) = ed.canvas.flatten() {
                            export::copy_image(&img);
                        }
                    }
                }
            });
        }
    }

    pub fn undo(&self) {
        self.history(true);
    }

    pub fn redo(&self) {
        self.history(false);
    }

    fn history(&self, undo: bool) {
        // Undoing a crop while the crop frame is up would apply the frame to
        // the restored image; leave crop mode first.
        if self.canvas.tool() == Tool::Crop {
            self.end_modes();
        }
        let resized = if undo { self.canvas.undo() } else { self.canvas.redo() };
        if resized {
            // OCR boxes no longer line up with the image.
            if self.canvas.st().ocr.is_some() || self.ocr_pending.get() {
                self.end_modes();
            }
            self.zoom_fit();
        }
        self.zoom_label_soon();
    }

    fn zoom_label_soon(&self) {
        self.update_zoom_label();
    }

    // --- Zoom -------------------------------------------------------------------

    fn update_zoom_label(&self) {
        let m = self.scroll.magnification();
        // Show zoom relative to the image's real pixel size (100% = 1:1 pixels on a
        // display of the snip's own scale).
        self.zoom_button.setTitle(&ns(&format!("{}%", (m * 100.0).round() as i64)));
    }

    pub fn zoom_fit(&self) {
        let img = self.canvas.image_size_pt();
        if img.width <= 0.0 {
            return;
        }
        let avail = self.scroll.contentSize();
        let m = ((avail.width - 40.0) / img.width).min((avail.height - 40.0) / img.height).min(1.0).max(0.05);
        self.scroll.setMagnification(m);
        self.update_zoom_label();
    }

    pub fn zoom_by(&self, factor: f64) {
        let m = (self.scroll.magnification() * factor).clamp(0.05, 16.0);
        let center = {
            let v = self.canvas.visibleRect();
            NSPoint::new(v.origin.x + v.size.width / 2.0, v.origin.y + v.size.height / 2.0)
        };
        self.scroll.setMagnification_centeredAtPoint(m, center);
        self.update_zoom_label();
    }

    pub fn zoom_actual(&self) {
        let v = self.canvas.visibleRect();
        let c = NSPoint::new(v.origin.x + v.size.width / 2.0, v.origin.y + v.size.height / 2.0);
        self.scroll.setMagnification_centeredAtPoint(1.0, c);
        self.update_zoom_label();
    }

    fn show_zoom_menu(&self) {
        let m = menu("Zoom");
        let w = self.me();
        m.addItem(&menu_item("Zoom in", "+", move || if let Some(e) = w.upgrade() { e.zoom_by(1.25) }));
        let w = self.me();
        m.addItem(&menu_item("Zoom out", "-", move || if let Some(e) = w.upgrade() { e.zoom_by(0.8) }));
        let w = self.me();
        m.addItem(&menu_item("Actual size", "0", move || if let Some(e) = w.upgrade() { e.zoom_actual() }));
        let w = self.me();
        m.addItem(&menu_item("Fit to window", "9", move || if let Some(e) = w.upgrade() { e.zoom_fit() }));
        let b = &self.zoom_button;
        m.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0.0, b.bounds().size.height + 4.0), Some(b));
    }

    // --- Output -----------------------------------------------------------------

    pub fn copy(&self) {
        if let Some(text) = self.canvas.ocr_text(true) {
            export::copy_text(&text);
            return;
        }
        if let Some(img) = self.canvas.flatten() {
            export::copy_image(&img);
        }
    }

    pub fn save_as(&self) {
        let Some(img) = self.canvas.flatten() else { return };
        let w = self.me();
        let name = self
            .saved_path
            .borrow()
            .as_ref()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| timestamp_name("Screenshot"));
        let dir = settings::save_dir();
        export::save_dialog(Some(&self.window), img, &name, Some(&dir), move |path| {
            if let (Some(ed), Some(p)) = (w.upgrade(), path) {
                ed.canvas.mark_saved();
                ed.set_saved_path(Some(p));
                ed.refresh_state();
                if ed.closing.get() {
                    ed.window.close();
                }
            } else if let Some(ed) = w.upgrade() {
                ed.closing.set(false);
            }
        });
    }

    fn temp_file(&self) -> Option<PathBuf> {
        let img = self.canvas.flatten()?;
        export::temp_png(&img, &timestamp_name("Screenshot"))
    }

    pub fn share(&self) {
        if let Some(p) = self.temp_file() {
            export::share_file(&p, &self.share_button);
        }
    }

    pub fn print(&self) {
        if let Some(img) = self.canvas.flatten() {
            export::print_image(&img, self.canvas.scale(), Some(&self.window));
        }
    }

    fn show_more_menu(&self) {
        let m = menu("More");
        let w = self.me();
        m.addItem(&menu_item("Open file…", "o", move || {
            let _ = &w;
            app::open_file_dialog();
        }));
        if self.has_image() {
            let w = self.me();
            m.addItem(&menu_item("Save as…", "s", move || if let Some(e) = w.upgrade() { e.save_as() }));
            let w = self.me();
            m.addItem(&menu_item("Copy", "c", move || if let Some(e) = w.upgrade() { e.copy() }));
            let w = self.me();
            m.addItem(&menu_item("Print…", "p", move || if let Some(e) = w.upgrade() { e.print() }));
            // Open with ▸
            let open_with = NSMenuItem::new(mtm());
            open_with.setTitle(&ns("Open with"));
            let sub = menu("Open with");
            if let Some(path) = self.temp_file() {
                for app in export::apps_for(&path).into_iter().take(12) {
                    let p = path.clone();
                    let url = app.url.clone();
                    let item = menu_item(&app.name, "", move || export::open_with(&p, Some(&url)));
                    item.setImage(Some(&app.icon));
                    sub.addItem(&item);
                }
            }
            open_with.setSubmenu(Some(&sub));
            m.addItem(&open_with);
            if let Some(p) = self.saved_path.borrow().clone() {
                m.addItem(&menu_item("Show in Finder", "", move || export::reveal_in_finder(&p)));
            }
        }
        m.addItem(&NSMenuItem::separatorItem(mtm()));
        m.addItem(&menu_item("Settings…", ",", app::open_settings));
        m.addItem(&menu_item("About Snipping", "", app::show_about));
        let b = &self.more_button;
        m.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(0.0, b.bounds().size.height + 4.0), Some(b));
    }

    // --- Window lifecycle ---------------------------------------------------------

    fn should_close(&self) -> bool {
        if self.closing.get() {
            return true;
        }
        if !(self.canvas.is_dirty() && settings::get().ask_to_save) {
            return true;
        }
        let alert = NSAlert::new(mtm());
        alert.setMessageText(&ns("Do you want to save changes to this snip?"));
        alert.setInformativeText(&ns("Your changes will be lost if you don't save them."));
        alert.addButtonWithTitle(&ns("Save"));
        alert.addButtonWithTitle(&ns("Don't Save"));
        alert.addButtonWithTitle(&ns("Cancel"));
        let w = self.me();
        let block = RcBlock::new(move |resp: NSModalResponse| {
            let Some(ed) = w.upgrade() else { return };
            match resp {
                r if r == NSAlertFirstButtonReturn => {
                    ed.closing.set(true);
                    // Let the alert sheet finish dismissing first.
                    let w2 = Rc::downgrade(&ed);
                    util::after(0.05, move || {
                        if let Some(ed) = w2.upgrade() {
                            ed.save_as();
                        }
                    });
                }
                r if r == NSAlertSecondButtonReturn => {
                    ed.closing.set(true);
                    ed.canvas.mark_saved();
                    let w2 = Rc::downgrade(&ed);
                    util::after(0.0, move || {
                        if let Some(ed) = w2.upgrade() {
                            ed.window.close();
                        }
                    });
                }
                _ => {}
            }
        });
        alert.beginSheetModalForWindow_completionHandler(&self.window, Some(&block));
        false
    }

    fn closed(&self) {
        self.close_popover();
        if let Some(token) = self.observer.borrow_mut().take() {
            unsafe {
                let any: &AnyObject = &*(&*token as *const ProtocolObject<dyn NSObjectProtocol> as *const AnyObject);
                NSNotificationCenter::defaultCenter().removeObserver(any)
            };
        }
        let me = self.me();
        // Defer removal so we don't drop ourselves inside the delegate callback.
        util::after(0.0, move || {
            EDITORS.with(|e| e.borrow_mut().retain(|x| !Weak::ptr_eq(&Rc::downgrade(x), &me)));
            app::window_closed();
        });
    }
}

// ---------------------------------------------------------------------------
// Module-level helpers used by the app
// ---------------------------------------------------------------------------

pub fn all() -> Vec<Rc<Editor>> {
    EDITORS.with(|e| e.borrow().clone())
}

/// The editor whose window is key right now (strict: no fallback).
pub fn focused_editor() -> Option<Rc<Editor>> {
    let app = NSApplication::sharedApplication(mtm());
    let key = app.keyWindow()?;
    all().into_iter().find(|e| *e.window == *key)
}

/// The editor that menu commands should target.
pub fn key_editor() -> Option<Rc<Editor>> {
    let app = NSApplication::sharedApplication(mtm());
    if let Some(key) = app.keyWindow() {
        let found = all().into_iter().find(|e| *e.window == *key);
        if found.is_some() {
            return found;
        }
    }
    LAST_KEY.with(|k| k.borrow().upgrade()).filter(|e| e.window.isVisible())
}

/// Show a snip in an editor, honoring the "multiple windows" setting.
pub fn open_image(image: CFRetained<CGImage>, scale: f64, saved: Option<PathBuf>) -> Rc<Editor> {
    let multiple = settings::get().multiple_windows;
    let reuse = if multiple {
        // Reuse only an empty window.
        all().into_iter().find(|e| !e.has_image() && e.window.isVisible())
    } else {
        key_editor().or_else(|| all().into_iter().next())
    };
    // Never replace unsaved edits: open a fresh window instead.
    let reuse = reuse.filter(|e| !e.is_dirty());
    let ed = reuse.unwrap_or_else(Editor::new);
    ed.load(image, scale);
    ed.set_saved_path(saved);
    ed.show();
    ed
}

pub fn open_empty() -> Rc<Editor> {
    let ed = key_editor().or_else(|| all().into_iter().next()).unwrap_or_else(Editor::new);
    ed.show();
    ed
}

pub fn any_dirty() -> bool {
    all().iter().any(|e| e.is_dirty())
}

#[allow(dead_code)]
pub fn retain_view(v: &NSView) -> Retained<NSView> {
    unsafe { Retained::retain(v as *const NSView as *mut NSView).unwrap() }
}

#[allow(dead_code)]
fn _unused(_: &AnyObject) {}

static TOOLBAR_DELEGATE_KEY: u8 = 0;

/// Install a toolbar built from custom-view items (used by other windows).
pub fn install_toolbar(window: &NSWindow, ident: &str, items: Vec<(&str, Retained<NSView>)>, order: Vec<String>) {
    let m = mtm();
    let tb_items: Vec<(String, Retained<NSToolbarItem>)> = items
        .into_iter()
        .map(|(id, view)| {
            let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(m), &ns(id));
            item.setView(Some(&view));
            item.setLabel(&ns(id));
            (id.to_string(), item)
        })
        .collect();
    let this = ToolbarDelegate::alloc(m).set_ivars(TbIvars {
        items: RefCell::new(tb_items),
        order: RefCell::new(order),
    });
    let d: Retained<ToolbarDelegate> = unsafe { msg_send![super(this), init] };
    let toolbar = NSToolbar::initWithIdentifier(NSToolbar::alloc(m), &ns(ident));
    toolbar.setDelegate(Some(ProtocolObject::from_ref(&*d)));
    toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
    toolbar.setAllowsUserCustomization(false);
    window.setToolbar(Some(&toolbar));
    util::retain_on(window, &d, &TOOLBAR_DELEGATE_KEY);
}

pub fn flex_id() -> String {
    unsafe { NSToolbarFlexibleSpaceItemIdentifier }.to_string()
}
