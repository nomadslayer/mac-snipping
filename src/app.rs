//! Application controller: delegate, menus, status item, hotkeys and the
//! snip / record / text-extract / color-pick flows.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Weak;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, MainThreadOnly, AllocAnyThread};
use objc2_app_kit::*;
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::*;
use objc2_foundation::*;

use crate::capture::{self, bitmap_context, image_size, Frozen};
use crate::editor::{self, Editor};
use crate::export;
use crate::hotkey::{self, HotkeyAction};
use crate::overlay::{self, OverlayResult, Purpose};
use crate::settings::{self, SnipMode, Theme};
use crate::toast;
use crate::util::{self, after, menu, menu_item, menu_item_mods, mtm, ns, rect, symbol, timestamp_name, unique_path};

#[derive(Clone)]
pub enum Origin {
    Hotkey,
    Menu,
    Editor(Weak<Editor>),
}

impl Origin {
    fn editor(&self) -> Option<std::rc::Rc<Editor>> {
        match self {
            Origin::Editor(w) => w.upgrade(),
            _ => None,
        }
    }
}

thread_local! {
    static STATUS_ITEM: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static PREV_APP: RefCell<Option<Retained<NSRunningApplication>>> = const { RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// App delegate
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingAppDelegate"]
    pub struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            launched();
        }

        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn should_terminate_after_last_window_closed(&self, _app: &NSApplication) -> bool {
            false
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, visible: bool) -> bool {
            if !visible {
                editor::open_empty();
            }
            true
        }

        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            for url in urls.iter() {
                if let Some(p) = url.path() {
                    open_path(PathBuf::from(p.to_string()));
                }
            }
        }

        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            if editor::any_dirty() && settings::get().ask_to_save {
                let a = NSAlert::new(mtm());
                a.setMessageText(&ns("You have unsaved snips."));
                a.setInformativeText(&ns("Quit Snipping and discard your changes?"));
                a.addButtonWithTitle(&ns("Quit"));
                a.addButtonWithTitle(&ns("Cancel"));
                if a.runModal() != NSAlertFirstButtonReturn {
                    return NSApplicationTerminateReply::TerminateCancel;
                }
            }
            if crate::recorder::is_recording() {
                crate::recorder::discard();
            }
            NSApplicationTerminateReply::TerminateNow
        }

        #[unsafe(method(applicationSupportsSecureRestorableState:))]
        fn secure_restorable(&self, _app: &NSApplication) -> bool {
            true
        }
    }
);

impl AppDelegate {
    pub fn new() -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn launched() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--selftest") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "selftest-out".into());
        crate::selftest::run(PathBuf::from(out));
        return;
    }
    apply_theme();
    build_main_menu();
    build_status_item();
    hotkey::set_handler(|action| match action {
        HotkeyAction::Snip => start_snip(Origin::Hotkey, None),
        HotkeyAction::Record => start_record(Origin::Hotkey),
        HotkeyAction::TextExtract => start_snip(Origin::Hotkey, Some(Purpose::TextExtract)),
    });
    register_hotkeys();

    let background = args.iter().any(|a| a == "--background");
    let files: Vec<PathBuf> = args
        .iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    if !files.is_empty() {
        for f in files {
            open_path(f);
        }
    } else if background {
        set_accessory();
    } else {
        editor::open_empty();
    }
    if std::env::var_os("SNIPPING_DEBUG").is_some() {
      after(1.5, || {
        STATUS_ITEM.with(|s| {
            let s = s.borrow();
            let Some(item) = s.as_ref() else {
                util::log("status item: missing");
                return;
            };
            let vis = item.isVisible();
            let info = item
                .button(mtm())
                .and_then(|b| b.window())
                .map(|w| format!("window frame {:?} visible {} occlusion {:?}", w.frame(), w.isVisible(), w.occlusionState()))
                .unwrap_or_else(|| "no button window".into());
            util::log(&format!("status item: isVisible={vis} {info}"));
        });
      });
    }
    if args.iter().any(|a| a == "--snip") {
        after(0.5, || start_snip(Origin::Menu, None));
    }
}

pub fn register_hotkeys() -> Vec<(HotkeyAction, String)> {
    let s = settings::get();
    if !s.hotkeys_enabled {
        hotkey::unregister_all();
        rebuild_status_menu();
        return Vec::new();
    }
    let errors = hotkey::register(&[
        (HotkeyAction::Snip, s.hotkey_snip),
        (HotkeyAction::Record, s.hotkey_record),
        (HotkeyAction::TextExtract, s.hotkey_text),
    ]);
    for (a, e) in &errors {
        util::log(&format!("hotkey {a:?}: {e}"));
    }
    rebuild_status_menu();
    errors
}

pub fn apply_theme() {
    let app = NSApplication::sharedApplication(mtm());
    let name = match settings::get().theme {
        Theme::System => None,
        Theme::Light => Some(unsafe { NSAppearanceNameAqua }),
        Theme::Dark => Some(unsafe { NSAppearanceNameDarkAqua }),
    };
    let appearance = name.and_then(NSAppearance::appearanceNamed);
    app.setAppearance(appearance.as_deref());
}

// ---------------------------------------------------------------------------
// Activation policy (Dock icon only while windows are open)
// ---------------------------------------------------------------------------

pub fn ensure_regular() {
    let app = NSApplication::sharedApplication(mtm());
    if app.activationPolicy() != NSApplicationActivationPolicy::Regular {
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    }
}

pub fn set_accessory() {
    let app = NSApplication::sharedApplication(mtm());
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

pub fn activate() {
    let app = NSApplication::sharedApplication(mtm());
    app.activate();
    if !app.isActive() {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

pub fn window_closed() {
    let app = NSApplication::sharedApplication(mtm());
    let any_visible = app.windows().iter().any(|w| {
        w.isVisible() && w.level() == NSNormalWindowLevel && w.canBecomeMainWindow()
    });
    if !any_visible && !crate::recorder::is_recording() {
        set_accessory();
    }
}

fn remember_frontmost() {
    let ws = NSWorkspace::sharedWorkspace();
    let me = NSRunningApplication::currentApplication();
    let front = ws.frontmostApplication().filter(|a| a.processIdentifier() != me.processIdentifier());
    PREV_APP.with(|p| *p.borrow_mut() = front);
}

fn restore_frontmost() {
    if let Some(app) = PREV_APP.with(|p| p.borrow_mut().take()) {
        #[allow(deprecated)]
        app.activateWithOptions(NSApplicationActivationOptions::empty());
    }
}

// ---------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------

fn std_item(title: &str, action: objc2::runtime::Sel, key: &str) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm()), &ns(title), Some(action), &ns(key))
    }
}

fn with_editor(f: impl Fn(&Editor) + 'static) -> impl Fn() + 'static {
    move || {
        if let Some(ed) = editor::focused_editor() {
            f(&ed);
        }
    }
}

/// Route to the focused editor, otherwise send the standard action up the
/// responder chain (text fields in sheets / Settings keep working).
fn editor_or_responder(f: impl Fn(&Editor) + 'static, action: objc2::runtime::Sel) -> impl Fn() + 'static {
    move || match editor::focused_editor() {
        Some(ed) => f(&ed),
        None => send_to_responder(action),
    }
}

fn build_main_menu() {
    let m = mtm();
    let app = NSApplication::sharedApplication(m);
    let main = menu("Main");
    let cmd = NSEventModifierFlags::Command;
    let cmd_shift = NSEventModifierFlags::Command | NSEventModifierFlags::Shift;
    let cmd_opt = NSEventModifierFlags::Command | NSEventModifierFlags::Option;

    // App menu
    let app_menu = menu("Snipping");
    app_menu.addItem(&menu_item("About Snipping", "", show_about));
    app_menu.addItem(&NSMenuItem::separatorItem(m));
    app_menu.addItem(&menu_item("Settings…", ",", open_settings));
    app_menu.addItem(&NSMenuItem::separatorItem(m));
    let services = menu("Services");
    let services_item = NSMenuItem::new(m);
    services_item.setTitle(&ns("Services"));
    services_item.setSubmenu(Some(&services));
    app_menu.addItem(&services_item);
    app.setServicesMenu(Some(&services));
    app_menu.addItem(&NSMenuItem::separatorItem(m));
    app_menu.addItem(&std_item("Hide Snipping", sel!(hide:), "h"));
    let hide_others = std_item("Hide Others", sel!(hideOtherApplications:), "h");
    hide_others.setKeyEquivalentModifierMask(cmd_opt);
    app_menu.addItem(&hide_others);
    app_menu.addItem(&std_item("Show All", sel!(unhideAllApplications:), ""));
    app_menu.addItem(&NSMenuItem::separatorItem(m));
    app_menu.addItem(&std_item("Quit Snipping", sel!(terminate:), "q"));

    // File
    let file = menu("File");
    file.addItem(&menu_item("New Snip", "n", || match editor::focused_editor() {
        Some(ed) => start_snip(Origin::Editor(ed.weak()), None),
        None => start_snip(Origin::Menu, None),
    }));
    file.addItem(&menu_item_mods("New Recording", "n", cmd_shift, || match editor::focused_editor() {
        Some(ed) => start_record(Origin::Editor(ed.weak())),
        None => start_record(Origin::Menu),
    }));
    file.addItem(&menu_item_mods("Text Extractor", "t", cmd_shift, || {
        start_snip(Origin::Menu, Some(Purpose::TextExtract))
    }));
    file.addItem(&menu_item_mods("Color Picker", "c", cmd_opt, || {
        start_snip(Origin::Menu, Some(Purpose::ColorPick))
    }));
    file.addItem(&NSMenuItem::separatorItem(m));
    file.addItem(&menu_item("Open…", "o", open_file_dialog));
    file.addItem(&NSMenuItem::separatorItem(m));
    file.addItem(&std_item("Close", sel!(performClose:), "w"));
    file.addItem(&menu_item("Save As…", "s", || {
        if let Some(e) = editor::focused_editor() {
            e.save_as();
        } else if let Some(v) = crate::video::focused() {
            v.save_as();
        }
    }));
    file.addItem(&NSMenuItem::separatorItem(m));
    file.addItem(&menu_item("Print…", "p", with_editor(|e| e.print())));

    // Edit
    let edit = menu("Edit");
    edit.addItem(&menu_item("Undo", "z", editor_or_responder(|e| e.undo(), sel!(undo:))));
    edit.addItem(&menu_item_mods("Redo", "z", cmd_shift, editor_or_responder(|e| e.redo(), sel!(redo:))));
    edit.addItem(&NSMenuItem::separatorItem(m));
    edit.addItem(&std_item("Cut", sel!(cut:), "x"));
    edit.addItem(&menu_item("Copy", "c", || {
        if let Some(e) = editor::focused_editor() {
            e.copy();
        } else if let Some(v) = crate::video::focused() {
            v.copy();
        } else {
            send_to_responder(sel!(copy:));
        }
    }));
    edit.addItem(&std_item("Paste", sel!(paste:), "v"));
    edit.addItem(&menu_item("Select All", "a", editor_or_responder(|e| e.canvas.ocr_select_all(), sel!(selectAll:))));

    // View
    let view = menu("View");
    view.addItem(&menu_item("Zoom In", "=", with_editor(|e| e.zoom_by(1.25))));
    view.addItem(&menu_item("Zoom Out", "-", with_editor(|e| e.zoom_by(0.8))));
    view.addItem(&menu_item("Actual Size", "0", with_editor(|e| e.zoom_actual())));
    view.addItem(&menu_item("Zoom to Fit", "9", with_editor(|e| e.zoom_fit())));
    let _ = cmd;

    // Window
    let window = menu("Window");
    window.addItem(&std_item("Minimize", sel!(performMiniaturize:), "m"));
    window.addItem(&std_item("Zoom", sel!(performZoom:), ""));
    window.addItem(&NSMenuItem::separatorItem(m));
    window.addItem(&std_item("Bring All to Front", sel!(arrangeInFront:), ""));
    app.setWindowsMenu(Some(&window));

    // Help
    let help = menu("Help");
    help.addItem(&menu_item("Snipping Help", "?", show_help));
    app.setHelpMenu(Some(&help));

    for (title, sub) in [
        ("Snipping", app_menu),
        ("File", file),
        ("Edit", edit),
        ("View", view),
        ("Window", window),
        ("Help", help),
    ] {
        let item = NSMenuItem::new(m);
        item.setTitle(&ns(title));
        item.setSubmenu(Some(&sub));
        main.addItem(&item);
    }
    app.setMainMenu(Some(&main));
}

fn send_to_responder(action: objc2::runtime::Sel) {
    let app = NSApplication::sharedApplication(mtm());
    unsafe {
        app.sendAction_to_from(action, None, None);
    }
}

fn build_status_item() {
    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm()) {
        let img = symbol("scissors", 15.0);
        img.setTemplate(true);
        button.setImage(Some(&img));
        button.setImagePosition(NSCellImagePosition::ImageLeading);
        button.setToolTip(Some(&ns("Snipping")));
    }
    STATUS_ITEM.with(|s| *s.borrow_mut() = Some(item));
    rebuild_status_menu();
}

fn rebuild_status_menu() {
    STATUS_ITEM.with(|s| {
        let s = s.borrow();
        let Some(item) = s.as_ref() else { return };
        let m = mtm();
        let st = settings::get();
        let menu_ = menu("Snipping");
        let with_hk = |title: &str, hk: crate::hotkey::Hotkey, f: fn()| {
            let it = menu_item(title, "", f);
            if st.hotkeys_enabled {
                let (key, mods) = hk.menu_equivalent();
                it.setKeyEquivalent(&ns(&key));
                it.setKeyEquivalentModifierMask(mods);
            }
            it
        };
        menu_.addItem(&with_hk("New snip", st.hotkey_snip, || start_snip(Origin::Menu, None)));
        menu_.addItem(&with_hk("Record screen", st.hotkey_record, || start_record(Origin::Menu)));
        menu_.addItem(&with_hk("Text extractor", st.hotkey_text, || {
            start_snip(Origin::Menu, Some(Purpose::TextExtract))
        }));
        menu_.addItem(&menu_item("Color picker", "", || start_snip(Origin::Menu, Some(Purpose::ColorPick))));
        menu_.addItem(&NSMenuItem::separatorItem(m));
        menu_.addItem(&menu_item("Open Snipping", "", || {
            editor::open_empty();
        }));
        menu_.addItem(&menu_item("Open image…", "", open_file_dialog));
        menu_.addItem(&menu_item("Settings…", "", open_settings));
        menu_.addItem(&NSMenuItem::separatorItem(m));
        menu_.addItem(&std_item("Quit Snipping", sel!(terminate:), ""));
        item.setMenu(Some(&menu_));
    });
}

fn set_status_title(t: &str) {
    STATUS_ITEM.with(|s| {
        if let Some(item) = s.borrow().as_ref() {
            if let Some(b) = item.button(mtm()) {
                b.setTitle(&ns(t));
            }
        }
    });
}

pub fn set_status_recording(on: bool) {
    STATUS_ITEM.with(|s| {
        if let Some(item) = s.borrow().as_ref() {
            if let Some(b) = item.button(mtm()) {
                let img = symbol(if on { "record.circle" } else { "scissors" }, 15.0);
                img.setTemplate(!on);
                b.setImage(Some(&img));
                b.setContentTintColor(if on { Some(NSColor::systemRedColor()) } else { None }.as_deref());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Small windows / dialogs
// ---------------------------------------------------------------------------

pub const AUTHOR: &str = "nomadslayer";
pub const AUTHOR_URL: &str = "https://github.com/nomadslayer";
pub const REPO_URL: &str = "https://github.com/nomadslayer/mac-snipping";

/// UTF-16 range of `needle` in `hay` (NSString ranges are UTF-16 based).
fn utf16_range(hay: &str, needle: &str) -> Option<NSRange> {
    let start = hay.find(needle)?;
    let loc = hay[..start].encode_utf16().count();
    Some(NSRange::new(loc, needle.encode_utf16().count()))
}

pub fn show_about() {
    let app = NSApplication::sharedApplication(mtm());
    ensure_regular();
    activate();
    let text = format!(
        "Built by {AUTHOR}\n{AUTHOR_URL}\n\nSource: {REPO_URL}\n\nA fast, native macOS snipping tool written in Rust."
    );
    let credits = NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &ns(&text));
    let all = NSRange::new(0, text.encode_utf16().count());
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(NSTextAlignment::Center);
    unsafe {
        credits.addAttribute_value_range(NSFontAttributeName, &NSFont::systemFontOfSize(11.0), all);
        credits.addAttribute_value_range(NSForegroundColorAttributeName, &NSColor::secondaryLabelColor(), all);
        credits.addAttribute_value_range(NSParagraphStyleAttributeName, &para, all);
        if let Some(r) = utf16_range(&text, &format!("Built by {AUTHOR}")) {
            credits.addAttribute_value_range(NSFontAttributeName, &NSFont::boldSystemFontOfSize(12.0), r);
            credits.addAttribute_value_range(NSForegroundColorAttributeName, &NSColor::labelColor(), r);
        }
        for url in [AUTHOR_URL, REPO_URL] {
            if let (Some(r), Some(link)) = (utf16_range(&text, url), NSURL::URLWithString(&ns(url))) {
                credits.addAttribute_value_range(NSLinkAttributeName, &link, r);
            }
        }
        let keys: [&NSString; 1] = [NSAboutPanelOptionCredits];
        let vals: [&AnyObject; 1] = [&credits];
        app.orderFrontStandardAboutPanelWithOptions(&NSDictionary::from_slices(&keys, &vals));
    }
}

fn show_help() {
    let s = settings::get();
    util::alert(
        "Snipping Help",
        &format!(
            "{}  New snip (rectangle, window, full screen, freeform)\n\
             {}  Record a screen region\n\
             {}  Text extractor (copy text from anywhere on screen)\n\n\
             In the editor:\n\
             • Click a tool twice to change its color and size.\n\
             • Ruler: scroll over it to rotate, drag it to move. Draw along its edge for straight lines.\n\
             • Shapes: hold ⇧ for squares, circles and 45° lines; drag handles to resize; ⌫ deletes.\n\
             • Text actions: select text and press ⌘C, or use Quick redact.\n\
             • ⌘S Save as · ⌘C Copy · ⌘Z / ⇧⌘Z Undo / Redo · ⌘= / ⌘- Zoom · ⌘0 Actual size",
            s.hotkey_snip.display(),
            s.hotkey_record.display(),
            s.hotkey_text.display()
        ),
    );
}

pub fn open_settings() {
    ensure_regular();
    crate::prefs::open();
    activate();
}

pub fn open_file_dialog() {
    let m = mtm();
    let panel = NSOpenPanel::openPanel(m);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(true);
    let types: Vec<_> = ["public.image"]
        .iter()
        .filter_map(|t| objc2_uniform_type_identifiers::UTType::typeWithIdentifier(&ns(t)))
        .collect();
    panel.setAllowedContentTypes(&NSArray::from_retained_slice(&types));
    ensure_regular();
    activate();
    if panel.runModal() == NSModalResponseOK {
        for url in panel.URLs().iter() {
            if let Some(p) = url.path() {
                open_path(PathBuf::from(p.to_string()));
            }
        }
    } else {
        window_closed();
    }
}

fn open_path(path: PathBuf) {
    match export::load_image_file(&path) {
        Some((img, scale)) => {
            let ed = editor::open_image(img, scale, Some(path));
            ed.refresh_state();
        }
        None => util::alert("Couldn't open file", &format!("{} isn't a supported image.", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Capture flows
// ---------------------------------------------------------------------------

/// Identifies this exact build (macOS ties Screen Recording permission to the
/// code signature, which changes with every build of an ad-hoc signed app).
fn build_id() -> String {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .map(|m| {
            let t = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            format!("{}-{}-{}", env!("CARGO_PKG_VERSION"), t, m.len())
        })
        .unwrap_or_default()
}

const SCREEN_RECORDING_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// Capture failed because Screen Recording isn't (yet) allowed for this build.
fn handle_permission_error(origin: &Origin) {
    restore_editor(origin);
    let build = build_id();
    if settings::get().permission_prompted_build != build {
        // The first request from a new build gets macOS's own prompt; don't
        // stack a second dialog on top of it.
        settings::update(|s| s.permission_prompted_build = build.clone());
        capture::request_permission();
        return;
    }
    show_permission_help();
}

pub fn show_permission_help() {
    ensure_regular();
    activate();
    let path = NSBundle::mainBundle().bundlePath().to_string();
    let a = NSAlert::new(mtm());
    a.setMessageText(&ns("Screen Recording isn't turned on for this copy of Snipping"));
    a.setInformativeText(&ns(&format!(
        "macOS links the permission to one exact copy of the app. If Snipping already shows as          on, that switch belongs to an older copy.\n\n         1. Open System Settings → Privacy & Security → Screen & System Audio Recording.\n         2. Select Snipping and click − to remove it. Then click + and choose:\n    {path}\n         3. Come back and click Quit & Reopen."
    )));
    a.addButtonWithTitle(&ns("Open System Settings"));
    a.addButtonWithTitle(&ns("Quit & Reopen"));
    a.addButtonWithTitle(&ns("Cancel"));
    let resp = a.runModal();
    if resp == NSAlertFirstButtonReturn {
        if let Some(url) = NSURL::URLWithString(&ns(SCREEN_RECORDING_SETTINGS)) {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
        // Reveal the app so it can be dragged into the list.
        NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[util::file_url(std::path::Path::new(&path))]));
    } else if resp == NSAlertSecondButtonReturn {
        relaunch();
    } else {
        window_closed();
    }
}

/// Quit and start this same app bundle again (macOS applies a new Screen
/// Recording grant only after a relaunch).
pub fn relaunch() {
    let path = NSBundle::mainBundle().bundlePath().to_string();
    let quoted = format!("'{}'", path.replace('\'', "'\\''"));
    let _ = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("sleep 1; /usr/bin/open -n {quoted}"))
        .spawn();
    NSApplication::sharedApplication(mtm()).terminate(None);
}

fn countdown(n: u32, then: Box<dyn FnOnce()>) {
    if n == 0 {
        set_status_title("");
        then();
        return;
    }
    set_status_title(&format!(" {n}"));
    after(1.0, move || countdown(n - 1, then));
}

pub fn is_busy() -> bool {
    BUSY.with(|b| b.get()) || overlay::is_active() || crate::recorder::is_active() || modal_open()
}

/// An app-modal dialog (alert, open panel…) is running: a screen-saver-level
/// overlay would cover it and be unusable, so captures wait.
fn modal_open() -> bool {
    NSApplication::sharedApplication(mtm()).modalWindow().is_some()
}

/// Begin a snip (or text extract / color pick). Mirrors Windows: from the
/// app window the result opens in the editor; from the hotkey it goes to the
/// clipboard with a notification.
pub fn start_snip(origin: Origin, purpose: Option<Purpose>) {
    if is_busy() {
        return;
    }
    let s = settings::get();
    let purpose = purpose.unwrap_or(Purpose::Snip);
    let mode = s.snip_mode;
    let delay = if matches!(origin, Origin::Editor(_) | Origin::Menu) && purpose == Purpose::Snip {
        s.delay
    } else {
        0
    };
    BUSY.with(|b| b.set(true));
    toast::hide_now();
    if let Some(ed) = origin.editor() {
        // Windows hides its window while you snip.
        ed.window.orderOut(None);
    }
    if matches!(origin, Origin::Hotkey) {
        remember_frontmost();
    }
    let go = move || {
        // Give the window server a frame to remove our window before capture.
        after(0.08, move || capture_then(origin, purpose, mode));
    };
    countdown(delay, Box::new(go));
}

fn capture_then(origin: Origin, purpose: Purpose, mode: SnipMode) {
    let screens = capture::screens();
    let t0 = std::time::Instant::now();
    capture::capture_displays(screens, move |res| {
        util::log(&format!("captured displays in {:?}", t0.elapsed()));
        let shots = match res {
            Ok(s) => s,
            Err(e) => {
                BUSY.with(|b| b.set(false));
                if e.permission {
                    handle_permission_error(&origin);
                } else {
                    restore_editor(&origin);
                    util::alert("Couldn't capture the screen", &e.message);
                }
                return;
            }
        };
        let frozen = Frozen { displays: shots, windows: capture::window_list() };
        // Full-screen snips happen instantly, without the overlay.
        if purpose == Purpose::Snip && mode == SnipMode::FullScreen {
            BUSY.with(|b| b.set(false));
            if let Some((image, scale)) = frozen.full_desktop() {
                deliver_snip(image, scale, origin);
            }
            return;
        }
        let origin2 = origin.clone();
        overlay::show(overlay::Request {
            purpose,
            mode,
            frozen,
            show_bar: true,
            done: Box::new(move |r| {
                BUSY.with(|b| b.set(false));
                on_overlay_result(r, origin2);
            }),
        });
    });
}

fn restore_editor(origin: &Origin) {
    if let Some(ed) = origin.editor() {
        ed.show();
    } else if matches!(origin, Origin::Hotkey) {
        restore_frontmost();
    }
}

fn on_overlay_result(r: OverlayResult, origin: Origin) {
    match r {
        OverlayResult::Cancelled => restore_editor(&origin),
        OverlayResult::Image { image, scale } => deliver_snip(image, scale, origin),
        OverlayResult::Text { image } => {
            restore_editor(&origin);
            crate::ocr::recognize(image, |lines| {
                let refs: Vec<&crate::ocr::OcrLine> = lines.iter().collect();
                let text = crate::ocr::join_lines(&refs);
                if text.trim().is_empty() {
                    toast::show(None, "No text found", "Try selecting a larger area.", None);
                } else {
                    export::copy_text(&text);
                    let preview: String = text.chars().take(90).collect::<String>().replace('\n', " ");
                    toast::show(None, "Text copied to clipboard", &preview, None);
                }
            });
        }
        OverlayResult::Color(c) => {
            restore_editor(&origin);
            let hex = c.to_hex();
            export::copy_text(&hex);
            let rgb = format!(
                "{hex}  ·  rgb({}, {}, {})",
                (c.r * 255.0).round() as i64,
                (c.g * 255.0).round() as i64,
                (c.b * 255.0).round() as i64
            );
            // Show a swatch as the toast thumbnail.
            let swatch = solid_image(c, 64, 40);
            toast::show(swatch.as_deref().map(|i| (i, 1.0)), "Color copied to clipboard", &rgb, None);
        }
        OverlayResult::Region { screen, rect } => {
            crate::recorder::prepare(screen, rect, origin);
        }
    }
}

fn solid_image(c: util::Rgba, w: usize, h: usize) -> Option<CFRetained<CGImage>> {
    let ctx = bitmap_context(w, h, None)?;
    CGContext::set_fill_color_with_color(Some(&ctx), Some(&c.cg()));
    CGContext::fill_rect(Some(&ctx), rect(0.0, 0.0, w as f64, h as f64));
    CGBitmapContextCreateImage(Some(&ctx))
}

/// Add the optional border (Settings → "Add border to each screenshot").
fn with_border(img: CFRetained<CGImage>, scale: f64) -> CFRetained<CGImage> {
    let s = settings::get();
    if !s.border_enabled || s.border_width <= 0.0 {
        return img;
    }
    let b = (s.border_width * scale).round().max(1.0);
    let (w, h) = image_size(&img);
    let (nw, nh) = (w as f64 + 2.0 * b, h as f64 + 2.0 * b);
    let Some(ctx) = bitmap_context(nw as usize, nh as usize, CGImage::color_space(Some(&img)).as_deref()) else {
        return img;
    };
    let c = Some(&*ctx);
    CGContext::set_fill_color_with_color(c, Some(&s.border_color.cg()));
    CGContext::fill_rect(c, rect(0.0, 0.0, nw, nh));
    CGContext::clear_rect(c, rect(b, b, w as f64, h as f64));
    CGContext::draw_image(c, rect(b, b, w as f64, h as f64), Some(&img));
    CGBitmapContextCreateImage(c).unwrap_or(img)
}

/// Clipboard / auto-save / notification / editor, exactly like Windows.
pub fn deliver_snip(image: CFRetained<CGImage>, scale: f64, origin: Origin) {
    let s = settings::get();
    let image = with_border(image, scale);
    let mut saved: Option<PathBuf> = None;
    if s.auto_save {
        // Encode + write off the main thread so the UI appears instantly.
        let dir = settings::save_dir();
        // Create the folder first so the empty placeholder really reserves the
        // name (two snips in the same second must not overwrite each other).
        let reserved = std::fs::create_dir_all(&dir).and_then(|_| {
            let path = unique_path(&dir, &timestamp_name("Screenshot"), "png");
            std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map(|_| path)
        });
        match reserved {
            Ok(path) => {
                let img = image.clone();
                let p2 = path.clone();
                util::background(move || {
                    if let Err(e) = export::write_image(&img, &p2) {
                        let _ = std::fs::remove_file(&p2);
                        let msg = format!("{}: {e}", p2.display());
                        util::log(&format!("auto-save failed: {msg}"));
                        util::on_main(move || {
                            toast::show(None, "Couldn't save the screenshot", &msg, None);
                        });
                    }
                });
                saved = Some(path);
            }
            Err(e) => {
                util::log(&format!("auto-save failed: {}: {e}", dir.display()));
                toast::show(None, "Couldn't save the screenshot", &format!("{}: {e}", dir.display()), None);
            }
        }
    }
    if s.auto_copy {
        // Clipboard after the UI is up (PNG encoding can take a moment).
        let img = image.clone();
        after(0.0, move || export::copy_image(&img));
    }
    match &origin {
        Origin::Editor(w) => {
            let target = w.upgrade();
            match target {
                Some(ed) if !(s.multiple_windows && ed.has_image()) && !ed.is_dirty() => {
                    ed.load(image, scale);
                    ed.set_saved_path(saved);
                    ed.show();
                }
                _ => {
                    let ed = Editor::new();
                    ed.load(image, scale);
                    ed.set_saved_path(saved);
                    ed.show();
                    if let Some(prev) = w.upgrade() {
                        prev.window.orderFront(None);
                    }
                    ed.window.makeKeyAndOrderFront(None);
                }
            }
        }
        Origin::Hotkey | Origin::Menu => {
            if matches!(origin, Origin::Hotkey) {
                restore_frontmost();
            }
            if !s.show_notification {
                return;
            }
            let title = if s.auto_copy { "Snip copied to clipboard" } else if saved.is_some() { "Snip saved" } else { "Snip ready" };
            let subtitle = match (&saved, s.auto_copy) {
                (Some(_), true) => "Also saved to Screenshots. Select here to mark up and share.",
                _ => "Select here to mark up and share.",
            };
            let img2 = image.clone();
            toast::show(
                Some((&image, scale)),
                title,
                subtitle,
                Some(Box::new(move || {
                    editor::open_image(img2.clone(), scale, saved.clone());
                })),
            );
        }
    }
}

pub fn start_record(origin: Origin) {
    if modal_open() {
        return;
    }
    // The record shortcut / menu item stops an active recording.
    if crate::recorder::is_recording() {
        crate::recorder::stop();
        return;
    }
    if is_busy() {
        return;
    }
    BUSY.with(|b| b.set(true));
    toast::hide_now();
    if let Some(ed) = origin.editor() {
        ed.window.orderOut(None);
    }
    after(0.08, move || capture_then(origin, Purpose::Record, SnipMode::Rectangle));
}
