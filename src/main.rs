//! Snipping — a native macOS screen capture tool modeled on the Windows
//! Snipping Tool. Written in Rust on top of AppKit, ScreenCaptureKit,
//! Vision and AVFoundation (via the objc2 bindings).

#![allow(unused_unsafe)]
#![allow(clippy::missing_safety_doc)]
#![allow(deprecated)]

mod annot;
mod app;
mod canvas;
mod capture;
mod editor;
mod export;
mod hotkey;
mod ocr;
mod overlay;
mod prefs;
mod recorder;
mod selftest;
mod settings;
mod toast;
mod toolopts;
mod util;
mod video;

use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

fn main() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let delegate = app::AppDelegate::new();
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    app.run();
    drop(delegate);
}
