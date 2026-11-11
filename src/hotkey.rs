//! System-wide keyboard shortcuts using the Carbon hot key API.
//!
//! `RegisterEventHotKey` is still the supported way for a non-sandboxed app to
//! own a global shortcut without Accessibility permission.

use std::cell::RefCell;
use std::ffi::c_void;

use objc2_app_kit::NSEventModifierFlags;
use serde::{Deserialize, Serialize};

use crate::util::{log, on_main};

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;
type EventHandlerUPP =
    unsafe extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerUPP,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OSStatus;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const K_EVENT_CLASS_KEYBOARD: u32 = fourcc(b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = fourcc(b"----");
const TYPE_EVENT_HOT_KEY_ID: u32 = fourcc(b"hkid");
const SIGNATURE: u32 = fourcc(b"SNIP");

// Carbon modifier bits.
pub const MOD_CMD: u32 = 0x0100;
pub const MOD_SHIFT: u32 = 0x0200;
pub const MOD_OPTION: u32 = 0x0800;
pub const MOD_CONTROL: u32 = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    /// Virtual key code (kVK_*).
    pub code: u32,
    /// Carbon modifier mask.
    pub mods: u32,
}

impl Hotkey {
    pub const DEFAULT_SNIP: Hotkey = Hotkey { code: 0x01, mods: MOD_CONTROL | MOD_SHIFT }; // ⌃⇧S
    pub const DEFAULT_RECORD: Hotkey = Hotkey { code: 0x0F, mods: MOD_CONTROL | MOD_SHIFT }; // ⌃⇧R
    pub const DEFAULT_TEXT: Hotkey = Hotkey { code: 0x11, mods: MOD_CONTROL | MOD_SHIFT }; // ⌃⇧T

    pub fn from_event(code: u16, flags: NSEventModifierFlags) -> Hotkey {
        let mut mods = 0;
        if flags.contains(NSEventModifierFlags::Command) {
            mods |= MOD_CMD;
        }
        if flags.contains(NSEventModifierFlags::Shift) {
            mods |= MOD_SHIFT;
        }
        if flags.contains(NSEventModifierFlags::Option) {
            mods |= MOD_OPTION;
        }
        if flags.contains(NSEventModifierFlags::Control) {
            mods |= MOD_CONTROL;
        }
        Hotkey { code: code as u32, mods }
    }

    /// Whether macOS will accept this combination as a global shortcut.
    /// (Since macOS 15, shortcuts using only ⌥ or ⌥⇧ are rejected; function
    /// keys may be used on their own.)
    pub fn validate(&self) -> Result<(), &'static str> {
        let is_fkey = key_name(self.code).starts_with('F') && key_name(self.code).len() > 1;
        if self.mods & (MOD_CMD | MOD_CONTROL) == 0 && !is_fkey {
            if self.mods & MOD_OPTION != 0 {
                return Err("macOS does not allow global shortcuts that use only ⌥ or ⌥⇧. Add ⌘ or ⌃.");
            }
            return Err("Use at least one of ⌘ or ⌃ (or a function key).");
        }
        Ok(())
    }

    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.mods & MOD_CONTROL != 0 {
            s.push('⌃');
        }
        if self.mods & MOD_OPTION != 0 {
            s.push('⌥');
        }
        if self.mods & MOD_SHIFT != 0 {
            s.push('⇧');
        }
        if self.mods & MOD_CMD != 0 {
            s.push('⌘');
        }
        s.push_str(key_name(self.code));
        s
    }

    /// Key equivalent string + modifier flags for NSMenuItem display.
    pub fn menu_equivalent(&self) -> (String, NSEventModifierFlags) {
        let key = match key_name(self.code) {
            k if k.chars().count() == 1 => k.to_lowercase(),
            _ => String::new(),
        };
        let mut f = NSEventModifierFlags::empty();
        if self.mods & MOD_CMD != 0 {
            f |= NSEventModifierFlags::Command;
        }
        if self.mods & MOD_SHIFT != 0 {
            f |= NSEventModifierFlags::Shift;
        }
        if self.mods & MOD_OPTION != 0 {
            f |= NSEventModifierFlags::Option;
        }
        if self.mods & MOD_CONTROL != 0 {
            f |= NSEventModifierFlags::Control;
        }
        (key, f)
    }
}

pub fn key_name(code: u32) -> &'static str {
    match code {
        0x00 => "A", 0x01 => "S", 0x02 => "D", 0x03 => "F", 0x04 => "H", 0x05 => "G",
        0x06 => "Z", 0x07 => "X", 0x08 => "C", 0x09 => "V", 0x0B => "B", 0x0C => "Q",
        0x0D => "W", 0x0E => "E", 0x0F => "R", 0x10 => "Y", 0x11 => "T", 0x12 => "1",
        0x13 => "2", 0x14 => "3", 0x15 => "4", 0x16 => "6", 0x17 => "5", 0x18 => "=",
        0x19 => "9", 0x1A => "7", 0x1B => "-", 0x1C => "8", 0x1D => "0", 0x1E => "]",
        0x1F => "O", 0x20 => "U", 0x21 => "[", 0x22 => "I", 0x23 => "P", 0x24 => "↩",
        0x25 => "L", 0x26 => "J", 0x27 => "'", 0x28 => "K", 0x29 => ";", 0x2A => "\\",
        0x2B => ",", 0x2C => "/", 0x2D => "N", 0x2E => "M", 0x2F => ".", 0x30 => "⇥",
        0x31 => "Space", 0x32 => "`", 0x33 => "⌫", 0x35 => "⎋",
        0x7A => "F1", 0x78 => "F2", 0x63 => "F3", 0x76 => "F4", 0x60 => "F5", 0x61 => "F6",
        0x62 => "F7", 0x64 => "F8", 0x65 => "F9", 0x6D => "F10", 0x67 => "F11", 0x6F => "F12",
        0x69 => "F13", 0x6B => "F14", 0x71 => "F15", 0x6A => "F16", 0x40 => "F17",
        0x4F => "F18", 0x50 => "F19", 0x5A => "F20",
        0x7B => "←", 0x7C => "→", 0x7D => "↓", 0x7E => "↑",
        0x73 => "↖", 0x77 => "↘", 0x74 => "⇞", 0x79 => "⇟", 0x75 => "⌦",
        _ => "?",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAction {
    Snip = 1,
    Record = 2,
    TextExtract = 3,
}

struct Registry {
    installed: bool,
    refs: Vec<EventHotKeyRef>,
    handler: Option<Box<dyn Fn(HotkeyAction)>>,
}

thread_local! {
    static REGISTRY: RefCell<Registry> = const { RefCell::new(Registry {
        installed: false,
        refs: Vec::new(),
        handler: None,
    }) };
}

unsafe extern "C" fn carbon_handler(
    _next: EventHandlerCallRef,
    event: EventRef,
    _user: *mut c_void,
) -> OSStatus {
    let mut id = EventHotKeyID::default();
    let status = GetEventParameter(
        event,
        K_EVENT_PARAM_DIRECT_OBJECT,
        TYPE_EVENT_HOT_KEY_ID,
        std::ptr::null_mut(),
        std::mem::size_of::<EventHotKeyID>(),
        std::ptr::null_mut(),
        &mut id as *mut EventHotKeyID as *mut c_void,
    );
    if status != 0 || id.signature != SIGNATURE {
        return -9874; // eventNotHandledErr
    }
    let action = match id.id {
        1 => HotkeyAction::Snip,
        2 => HotkeyAction::Record,
        3 => HotkeyAction::TextExtract,
        _ => return -9874,
    };
    // Defer so that the Carbon dispatch returns before we open UI.
    on_main(move || {
        REGISTRY.with(|r| {
            if let Some(h) = r.borrow().handler.as_ref() {
                h(action);
            }
        })
    });
    0
}

pub fn set_handler(f: impl Fn(HotkeyAction) + 'static) {
    REGISTRY.with(|r| r.borrow_mut().handler = Some(Box::new(f)));
}

/// (Re)register the given shortcuts. Returns per-shortcut errors.
pub fn register(keys: &[(HotkeyAction, Hotkey)]) -> Vec<(HotkeyAction, String)> {
    let mut errors = Vec::new();
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        unsafe {
            if !r.installed {
                let spec = EventTypeSpec {
                    event_class: K_EVENT_CLASS_KEYBOARD,
                    event_kind: K_EVENT_HOT_KEY_PRESSED,
                };
                let mut handler_ref: EventHandlerRef = std::ptr::null_mut();
                let st = InstallEventHandler(
                    GetApplicationEventTarget(),
                    carbon_handler,
                    1,
                    &spec,
                    std::ptr::null_mut(),
                    &mut handler_ref,
                );
                if st != 0 {
                    log(&format!("InstallEventHandler failed: {st}"));
                }
                r.installed = true;
            }
            for hk in r.refs.drain(..) {
                UnregisterEventHotKey(hk);
            }
            for (action, key) in keys {
                if let Err(e) = key.validate() {
                    errors.push((*action, e.to_string()));
                    continue;
                }
                let mut out: EventHotKeyRef = std::ptr::null_mut();
                let st = RegisterEventHotKey(
                    key.code,
                    key.mods,
                    EventHotKeyID { signature: SIGNATURE, id: *action as u32 },
                    GetApplicationEventTarget(),
                    0,
                    &mut out,
                );
                if st == 0 && !out.is_null() {
                    r.refs.push(out);
                } else {
                    errors.push((
                        *action,
                        format!("{} is already in use by another app (error {st}).", key.display()),
                    ));
                }
            }
        }
    });
    errors
}

pub fn unregister_all() {
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        for hk in r.refs.drain(..) {
            unsafe {
                UnregisterEventHotKey(hk);
            }
        }
    });
}
