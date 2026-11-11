//! Persistent user settings (JSON in ~/Library/Application Support/Snipping).

use std::cell::RefCell;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hotkey::Hotkey;
use crate::util::{home_dir, Rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnipMode {
    Rectangle,
    Window,
    FullScreen,
    Freeform,
}

impl SnipMode {
    pub const ALL: [SnipMode; 4] = [
        SnipMode::Rectangle,
        SnipMode::Window,
        SnipMode::FullScreen,
        SnipMode::Freeform,
    ];
    pub fn title(self) -> &'static str {
        match self {
            SnipMode::Rectangle => "Rectangle",
            SnipMode::Window => "Window",
            SnipMode::FullScreen => "Full screen",
            SnipMode::Freeform => "Freeform",
        }
    }
    pub fn symbol(self) -> &'static str {
        match self {
            SnipMode::Rectangle => "rectangle.dashed",
            SnipMode::Window => "macwindow",
            SnipMode::FullScreen => "display",
            SnipMode::Freeform => "lasso",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Shortcuts
    pub hotkeys_enabled: bool,
    pub hotkey_snip: Hotkey,
    pub hotkey_record: Hotkey,
    pub hotkey_text: Hotkey,

    // Snipping
    pub auto_copy: bool,
    pub auto_save: bool,
    pub save_dir: String,
    pub ask_to_save: bool,
    pub multiple_windows: bool,
    pub border_enabled: bool,
    pub border_color: Rgba,
    pub border_width: f64,
    pub snip_mode: SnipMode,
    pub delay: u32,
    pub show_notification: bool,

    // Screen recording
    pub auto_save_recordings: bool,
    pub recordings_dir: String,
    pub record_mic: bool,
    pub record_system_audio: bool,
    pub record_fps: u32,

    // App
    pub theme: Theme,
    pub launch_at_login: bool,
    /// Build that last hit a Screen Recording permission error (macOS shows
    /// its own prompt only the first time each build asks).
    pub permission_prompted_build: String,

    // Editor tool state (remembered between sessions, like Windows)
    pub pen_color: Rgba,
    pub pen_width: f64,
    pub highlighter_color: Rgba,
    pub highlighter_width: f64,
    pub shape_kind: u32,
    pub shape_stroke: Rgba,
    pub shape_fill: Option<Rgba>,
    pub shape_width: f64,
}

impl Default for Settings {
    fn default() -> Self {
        let home = home_dir();
        Self {
            hotkeys_enabled: true,
            hotkey_snip: Hotkey::DEFAULT_SNIP,
            hotkey_record: Hotkey::DEFAULT_RECORD,
            hotkey_text: Hotkey::DEFAULT_TEXT,

            auto_copy: true,
            auto_save: true,
            save_dir: home.join("Pictures/Screenshots").to_string_lossy().into_owned(),
            ask_to_save: true,
            multiple_windows: false,
            border_enabled: false,
            border_color: Rgba::hex(0x3B3B3B),
            border_width: 2.0,
            snip_mode: SnipMode::Rectangle,
            delay: 0,
            show_notification: true,

            auto_save_recordings: true,
            recordings_dir: home
                .join("Movies/Screen Recordings")
                .to_string_lossy()
                .into_owned(),
            record_mic: false,
            record_system_audio: true,
            record_fps: 30,

            theme: Theme::System,
            launch_at_login: false,
            permission_prompted_build: String::new(),

            pen_color: Rgba::hex(0x000000),
            pen_width: 4.0,
            highlighter_color: Rgba::hex(0xFFEB3B),
            highlighter_width: 16.0,
            shape_kind: 0,
            shape_stroke: Rgba::hex(0xE81123),
            shape_fill: None,
            shape_width: 4.0,
        }
    }
}

thread_local! {
    static SETTINGS: RefCell<Settings> = RefCell::new(load());
}

pub fn config_dir() -> PathBuf {
    home_dir().join("Library/Application Support/Snipping")
}

fn settings_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SNIPPING_SETTINGS") {
        return PathBuf::from(p);
    }
    config_dir().join("settings.json")
}

fn load() -> Settings {
    std::fs::read(settings_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(s: &Settings) {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// A snapshot of the current settings.
pub fn get() -> Settings {
    SETTINGS.with(|s| s.borrow().clone())
}

/// Mutate settings and persist them.
pub fn update(f: impl FnOnce(&mut Settings)) {
    let snapshot = SETTINGS.with(|s| {
        let mut s = s.borrow_mut();
        f(&mut s);
        s.clone()
    });
    save(&snapshot);
}

pub fn save_dir() -> PathBuf {
    PathBuf::from(get().save_dir)
}

pub fn recordings_dir() -> PathBuf {
    PathBuf::from(get().recordings_dir)
}
