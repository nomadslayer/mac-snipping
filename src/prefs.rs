//! Settings window (mirrors the Windows Snipping Tool settings page).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, DefinedClass, MainThreadOnly};
use objc2_app_kit::*;
use objc2_foundation::*;
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::hotkey::{self, Hotkey, HotkeyAction};
use crate::settings::{self, Settings, Theme};
use crate::util::{self, fixed_size, label, mtm, ns, on_action, rect, secondary_label, vstack, Rgba};

thread_local! {
    static WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    static ERROR_LABEL: RefCell<Option<Retained<NSTextField>>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Shortcut recorder
// ---------------------------------------------------------------------------

pub struct RecorderIvars {
    action: HotkeyAction,
    recording: Cell<bool>,
}

define_class!(
    #[unsafe(super = NSButton)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingShortcutField"]
    #[ivars = RecorderIvars]
    pub struct ShortcutField;

    impl ShortcutField {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _e: &NSEvent) {
            if self.ivars().recording.get() {
                self.stop_recording();
                return;
            }
            self.ivars().recording.set(true);
            // Release global shortcuts so the current combo can be re-typed.
            hotkey::unregister_all();
            self.setTitle(&ns("Type shortcut…  (Esc to cancel)"));
            if let Some(w) = self.window() {
                w.makeFirstResponder(Some(self));
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, e: &NSEvent) -> bool {
            let rec = self.ivars().recording.get();
            if rec {
                self.capture(e);
            }
            rec
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            if self.ivars().recording.get() {
                self.capture(e);
            }
        }

        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            if self.ivars().recording.get() {
                self.stop_recording();
            }
            true
        }
    }
);

impl ShortcutField {
    fn new(action: HotkeyAction) -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(RecorderIvars { action, recording: Cell::new(false) });
        let b: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 200.0, 26.0)] };
        b.setBezelStyle(NSBezelStyle::Push);
        b.setTitle(&ns(&current(action).display()));
        fixed_size(&b, 210.0, 0.0);
        b
    }

    fn capture(&self, e: &NSEvent) {
        let code = e.keyCode();
        if code == 53 {
            self.stop_recording();
            return;
        }
        let flags = e.modifierFlags();
        let hk = Hotkey::from_event(code, flags);
        let action = self.ivars().action;
        let s = settings::get();
        let taken_by = [
            (HotkeyAction::Snip, s.hotkey_snip, "New snip"),
            (HotkeyAction::Record, s.hotkey_record, "Record screen"),
            (HotkeyAction::TextExtract, s.hotkey_text, "Text extractor"),
        ]
        .into_iter()
        .find(|(a, k, _)| *a != action && *k == hk)
        .map(|(_, _, name)| name);
        if let Some(name) = taken_by {
            set_error(&format!("{} is already used for {name}.", hk.display()));
            return;
        }
        match hk.validate() {
            Ok(()) => {
                settings::update(|s| match action {
                    HotkeyAction::Snip => s.hotkey_snip = hk,
                    HotkeyAction::Record => s.hotkey_record = hk,
                    HotkeyAction::TextExtract => s.hotkey_text = hk,
                });
                self.stop_recording();
            }
            Err(msg) => {
                set_error(msg);
                util::log(msg);
            }
        }
    }

    fn stop_recording(&self) {
        self.ivars().recording.set(false);
        self.setTitle(&ns(&current(self.ivars().action).display()));
        reregister();
    }
}

fn current(action: HotkeyAction) -> Hotkey {
    let s = settings::get();
    match action {
        HotkeyAction::Snip => s.hotkey_snip,
        HotkeyAction::Record => s.hotkey_record,
        HotkeyAction::TextExtract => s.hotkey_text,
    }
}

fn set_error(msg: &str) {
    ERROR_LABEL.with(|l| {
        if let Some(l) = l.borrow().as_ref() {
            l.setStringValue(&ns(msg));
            l.setHidden(msg.is_empty());
        }
    });
}

fn reregister() {
    let errors = crate::app::register_hotkeys();
    let msg = errors.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>().join("\n");
    set_error(&msg);
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

fn switch(on: bool, f: impl Fn(&mut Settings, bool) + 'static) -> Retained<NSSwitch> {
    switch_then(on, f, || {})
}

/// A switch whose `then` runs after the setting has been saved.
fn switch_then(
    on: bool,
    f: impl Fn(&mut Settings, bool) + 'static,
    then: impl Fn() + 'static,
) -> Retained<NSSwitch> {
    let s = NSSwitch::initWithFrame(NSSwitch::alloc(mtm()), rect(0.0, 0.0, 40.0, 22.0));
    s.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
    on_action(&s, move |sender| {
        if let Some(sw) = sender.and_then(|x| x.downcast_ref::<NSSwitch>()) {
            let v = sw.state() == NSControlStateValueOn;
            settings::update(|st| f(st, v));
            then();
        }
    });
    s
}

fn popup(items: &[&str], selected: usize, f: impl Fn(usize) + 'static) -> Retained<NSPopUpButton> {
    let p = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm()), rect(0.0, 0.0, 160.0, 26.0), false);
    for i in items {
        p.addItemWithTitle(&ns(i));
    }
    p.selectItemAtIndex(selected as isize);
    on_action(&p, move |sender| {
        if let Some(p) = sender.and_then(|x| x.downcast_ref::<NSPopUpButton>()) {
            f(p.indexOfSelectedItem().max(0) as usize);
        }
    });
    p
}

fn folder_picker(current: String, f: impl Fn(&mut Settings, String) + 'static) -> Retained<NSStackView> {
    let path_label = secondary_label(&shorten(&current), 12.0);
    path_label.setLineBreakMode(NSLineBreakMode::ByTruncatingMiddle);
    fixed_size(&path_label, 170.0, 0.0);
    let pl = path_label.clone();
    let button = util::text_button("Change…", move || {
        let panel = NSOpenPanel::openPanel(mtm());
        panel.setCanChooseFiles(false);
        panel.setCanChooseDirectories(true);
        panel.setCanCreateDirectories(true);
        panel.setPrompt(Some(&ns("Choose")));
        if panel.runModal() == NSModalResponseOK {
            if let Some(p) = panel.URL().and_then(|u| u.path()) {
                let p = p.to_string();
                pl.setStringValue(&ns(&shorten(&p)));
                settings::update(|s| f(s, p.clone()));
            }
        }
    });
    util::hstack(&[&path_label, &button], 8.0)
}

fn shorten(p: &str) -> String {
    let home = util::home_dir().to_string_lossy().into_owned();
    match p.strip_prefix(&home) {
        Some(rest) => format!("~{rest}"),
        None => p.to_string(),
    }
}

const CARD_W: f64 = 620.0;
const LABEL_W: f64 = 300.0;

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingCardView"]
    pub struct CardView;

    impl CardView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _r: NSRect) {
            let b = crate::util::rect_inset(self.bounds(), 0.5);
            let path = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(b, 10.0, 10.0);
            NSColor::controlBackgroundColor().setFill();
            path.fill();
            NSColor::separatorColor().setStroke();
            path.setLineWidth(1.0);
            path.stroke();
        }
    }
);

impl CardView {
    fn new() -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, CARD_W, 100.0)] }
    }
}

fn section(title: &str, rows: Vec<(&str, Option<&str>, Retained<NSView>)>) -> Retained<NSStackView> {
    let m = mtm();
    let grid_rows: Vec<Retained<NSArray<NSView>>> = rows
        .iter()
        .map(|(t, sub, control)| {
            let left: Retained<NSView> = match sub {
                Some(sub) => {
                    let s = vstack(&[&label(t, 13.0, false), &secondary_label(sub, 11.0)], 2.0);
                    Retained::into_super(s)
                }
                None => Retained::into_super(Retained::into_super(label(t, 13.0, false))),
            };
            NSArray::from_retained_slice(&[left, control.clone()])
        })
        .collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&grid_rows), m);
    grid.setRowSpacing(14.0);
    grid.setColumnSpacing(24.0);
    grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
    grid.columnAtIndex(0).setWidth(LABEL_W);
    grid.columnAtIndex(1).setXPlacement(NSGridCellPlacement::Trailing);
    for i in 0..grid.numberOfRows() {
        grid.rowAtIndex(i).setYPlacement(NSGridCellPlacement::Center);
    }
    grid.setTranslatesAutoresizingMaskIntoConstraints(false);
    let card = CardView::new();
    card.setTranslatesAutoresizingMaskIntoConstraints(false);
    card.addSubview(&grid);
    grid.leadingAnchor().constraintEqualToAnchor_constant(&card.leadingAnchor(), 16.0).setActive(true);
    grid.trailingAnchor().constraintEqualToAnchor_constant(&card.trailingAnchor(), -16.0).setActive(true);
    grid.topAnchor().constraintEqualToAnchor_constant(&card.topAnchor(), 14.0).setActive(true);
    grid.bottomAnchor().constraintEqualToAnchor_constant(&card.bottomAnchor(), -14.0).setActive(true);
    card.widthAnchor().constraintEqualToConstant(CARD_W).setActive(true);
    let header = label(title, 14.0, true);
    header.setTextColor(Some(&NSColor::labelColor()));
    vstack(&[&header, &card], 8.0)
}

fn launch_at_login_set(on: bool) -> Result<(), String> {
    unsafe {
        let svc = SMAppService::mainAppService();
        if on {
            svc.registerAndReturnError().map_err(|e| e.localizedDescription().to_string())
        } else {
            svc.unregisterAndReturnError().map_err(|e| e.localizedDescription().to_string())
        }
    }
}

fn launch_at_login_enabled() -> bool {
    unsafe { SMAppService::mainAppService().status() == SMAppServiceStatus::Enabled }
}

pub fn build_content() -> Retained<NSView> {
    let m = mtm();
    let s = settings::get();

    let err = label("", 12.0, false);
    err.setTextColor(Some(&NSColor::systemRedColor()));
    err.setHidden(true);
    err.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    fixed_size(&err, CARD_W, 0.0);
    ERROR_LABEL.with(|l| *l.borrow_mut() = Some(err.clone()));

    let to_view = |v: Retained<NSView>| v;
    let shortcuts = section(
        "Shortcuts",
        vec![
            (
                "Use global shortcuts",
                Some("Works from any app, like Win + Shift + S on Windows."),
                to_view(Retained::into_super(Retained::into_super(switch_then(
                    s.hotkeys_enabled,
                    |st, v| st.hotkeys_enabled = v,
                    reregister,
                )))),
            ),
            ("New snip", None, Retained::into_super(Retained::into_super(Retained::into_super(ShortcutField::new(HotkeyAction::Snip))))),
            ("Record screen", None, Retained::into_super(Retained::into_super(Retained::into_super(ShortcutField::new(HotkeyAction::Record))))),
            ("Text extractor", None, Retained::into_super(Retained::into_super(Retained::into_super(ShortcutField::new(HotkeyAction::TextExtract))))),
        ],
    );

    let border_color = NSColorWell::colorWellWithStyle(NSColorWellStyle::Minimal, m);
    border_color.setColor(&s.border_color.ns());
    on_action(&border_color, |sender| {
        if let Some(w) = sender.and_then(|x| x.downcast_ref::<NSColorWell>()) {
            let c = Rgba::from_ns(&w.color());
            settings::update(|s| s.border_color = c);
        }
    });
    fixed_size(&border_color, 44.0, 24.0);
    let widths = [1.0, 2.0, 4.0, 8.0];
    let bw = popup(
        &["1 pt", "2 pt", "4 pt", "8 pt"],
        widths.iter().position(|w| *w == s.border_width).unwrap_or(1),
        move |i| settings::update(|s| s.border_width = widths[i]),
    );
    let border_row = util::hstack(
        &[&switch(s.border_enabled, |st, v| st.border_enabled = v), &border_color, &bw],
        10.0,
    );

    let sv = |v: Retained<NSSwitch>| -> Retained<NSView> { Retained::into_super(Retained::into_super(v)) };
    let stack_v = |v: Retained<NSStackView>| -> Retained<NSView> { Retained::into_super(v) };
    let popup_v = |v: Retained<NSPopUpButton>| -> Retained<NSView> { Retained::into_super(Retained::into_super(Retained::into_super(v))) };

    let snipping = section(
        "Snipping",
        vec![
            ("Automatically copy changes", Some("Copies snips and edits to the clipboard."), sv(switch(s.auto_copy, |st, v| st.auto_copy = v))),
            ("Automatically save screenshots", Some("Saves every snip as a PNG."), sv(switch(s.auto_save, |st, v| st.auto_save = v))),
            ("Screenshots folder", None, stack_v(folder_picker(s.save_dir.clone(), |st, p| st.save_dir = p))),
            ("Ask to save edited screenshots", None, sv(switch(s.ask_to_save, |st, v| st.ask_to_save = v))),
            ("Multiple windows", Some("Open each new snip in its own window."), sv(switch(s.multiple_windows, |st, v| st.multiple_windows = v))),
            ("Add border to each screenshot", None, stack_v(border_row)),
            ("Show notification after a snip", None, sv(switch(s.show_notification, |st, v| st.show_notification = v))),
        ],
    );

    let fps_values = [30u32, 60u32];
    let recording = section(
        "Screen recording",
        vec![
            ("Automatically save recordings", None, sv(switch(s.auto_save_recordings, |st, v| st.auto_save_recordings = v))),
            ("Recordings folder", None, stack_v(folder_picker(s.recordings_dir.clone(), |st, p| st.recordings_dir = p))),
            ("Include microphone input by default", None, sv(switch(s.record_mic, |st, v| st.record_mic = v))),
            ("Include system audio by default", None, sv(switch(s.record_system_audio, |st, v| st.record_system_audio = v))),
            (
                "Frame rate",
                None,
                popup_v(popup(&["30 fps", "60 fps"], if s.record_fps >= 60 { 1 } else { 0 }, move |i| {
                    settings::update(|s| s.record_fps = fps_values[i])
                })),
            ),
        ],
    );

    let theme_idx = match s.theme {
        Theme::System => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    };
    let login = NSSwitch::initWithFrame(NSSwitch::alloc(m), rect(0.0, 0.0, 40.0, 22.0));
    login.setState(if launch_at_login_enabled() { NSControlStateValueOn } else { NSControlStateValueOff });
    on_action(&login, |sender| {
        if let Some(sw) = sender.and_then(|x| x.downcast_ref::<NSSwitch>()) {
            let on = sw.state() == NSControlStateValueOn;
            if let Err(e) = launch_at_login_set(on) {
                util::alert("Couldn't change the login item", &e);
                sw.setState(if launch_at_login_enabled() { NSControlStateValueOn } else { NSControlStateValueOff });
            }
            settings::update(|s| s.launch_at_login = on);
        }
    });
    let app_section = section(
        "App",
        vec![
            (
                "App theme",
                None,
                popup_v(popup(&["Use system setting", "Light", "Dark"], theme_idx, |i| {
                    settings::update(|s| s.theme = [Theme::System, Theme::Light, Theme::Dark][i]);
                    crate::app::apply_theme();
                })),
            ),
            ("Open Snipping at login", Some("Keeps shortcuts ready after you sign in."), sv(login)),
        ],
    );

    let version = secondary_label(
        &format!(
            "Snipping {} · Built by {} ({})",
            env!("CARGO_PKG_VERSION"),
            crate::app::AUTHOR,
            crate::app::AUTHOR_URL
        ),
        11.0,
    );
    let perms = util::text_button("Open Screen Recording settings…", || {
        if let Some(url) = NSURL::URLWithString(&ns("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")) {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    });

    let stack = vstack(&[&shortcuts, &err, &snipping, &recording, &app_section, &perms, &version], 22.0);
    stack.setEdgeInsets(NSEdgeInsets { top: 22.0, left: 26.0, bottom: 26.0, right: 26.0 });
    let size = stack.fittingSize();
    stack.setFrame(rect(0.0, 0.0, size.width, size.height));

    // Scrollable (the window may be shorter than the content).
    let flipped = FlippedView::new(rect(0.0, 0.0, size.width, size.height));
    flipped.addSubview(&stack);
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(m), rect(0.0, 0.0, size.width, size.height.min(760.0)));
    scroll.setHasVerticalScroller(true);
    scroll.setDrawsBackground(false);
    scroll.setDocumentView(Some(&flipped));
    scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    let _ = PathBuf::new();
    Retained::into_super(scroll)
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingFlippedView"]
    pub struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl FlippedView {
    fn new(frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub fn open() {
    let existing = WINDOW.with(|w| w.borrow().clone());
    if let Some(w) = existing {
        w.makeKeyAndOrderFront(None);
        return;
    }
    let m = mtm();
    let content = build_content();
    let size = content.frame().size;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(m),
            rect(0.0, 0.0, size.width, size.height),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&ns("Settings"));
    window.setContentView(Some(&content));
    window.setContentMinSize(NSSize::new(size.width, 360.0));
    window.setContentMaxSize(NSSize::new(size.width, 4000.0));
    window.center();
    let weak = objc2::rc::Weak::from_retained(&window);
    util::on_window_events(
        &window,
        || {
            util::after(0.0, || {
                WINDOW.with(|w| *w.borrow_mut() = None);
                ERROR_LABEL.with(|l| *l.borrow_mut() = None);
                // A shortcut field may have been mid-capture with hotkeys off.
                crate::app::register_hotkeys();
                crate::app::window_closed();
            });
        },
        Some(move || {
            // Leaving the window ends any shortcut capture (re-enables hotkeys).
            if let Some(w) = weak.load() {
                w.makeFirstResponder(None);
            }
        }),
    );
    window.makeKeyAndOrderFront(None);
    // Scroll to top.
    if let Some(sv) = content.downcast_ref::<NSScrollView>() {
        if let Some(doc) = sv.documentView() {
            doc.scrollPoint(NSPoint::new(0.0, 0.0));
        }
    }
    WINDOW.with(|w| *w.borrow_mut() = Some(window));
    let _ = std::mem::size_of::<AnyObject>;
}

pub fn window() -> Option<Retained<NSWindow>> {
    WINDOW.with(|w| w.borrow().clone())
}
