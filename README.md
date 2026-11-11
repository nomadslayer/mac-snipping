# Snipping

**Built by [nomadslayer](https://github.com/nomadslayer).**

A fast, native macOS screen-capture app modeled on the **Windows 11 Snipping Tool**, written entirely in **Rust**.

It talks directly to Apple's frameworks through the [`objc2`](https://github.com/madsmtm/objc2) bindings — AppKit for the UI, ScreenCaptureKit for capture and recording, Vision for OCR, AVFoundation/AVKit for video and ImageIO for encoding. There is no web view and no runtime, and the app bundle is about 1.3 MB.

## Features

| Windows Snipping Tool | Snipping for macOS |
|---|---|
| `Win + Shift + S` capture bar | **⌃⇧S** (configurable) — frozen-screen overlay with a capture bar |
| Rectangle / Window / Full screen / Freeform modes | ✅ All four. Full screen covers all displays; works across mixed-DPI monitors |
| Snip delay (3 / 5 / 10 s) | ✅ Countdown shown in the menu bar |
| Screen recording, with mic and system audio | ✅ **⌃⇧R** — select a region, then Start, a 3-2-1 countdown, Pause/Resume, Stop, Discard → MP4 |
| Recording preview with save / copy / share | ✅ Player window with Save as, Copy, Share, Show in Finder and **Export GIF** |
| Text extractor | ✅ **⌃⇧T** — select an area and its text goes to the clipboard (Vision OCR) |
| Color picker | ✅ Loupe that copies the hex value (sRGB) |
| Toast "Snip copied to clipboard — select here to mark up" | ✅ Bottom-right notification with a thumbnail; click it to open the editor |
| Ballpoint pen, Highlighter (colors + sizes) | ✅ Click a tool twice for its color palette and size slider |
| Eraser (stroke eraser) + "Erase all ink" | ✅ |
| Ruler and Protractor (draw straight lines and arcs) | ✅ Drag to move, scroll or two-finger rotate to turn; strokes snap to the edge |
| Shapes (rectangle, oval, line, arrow) with outline and fill | ✅ Select, move or resize with handles; ⇧ constrains; ⌫ deletes |
| Image crop | ✅ Handles plus Apply/Cancel; Return and Esc work too |
| Text actions: copy text, Quick redact emails and phone numbers | ✅ Select lines, Copy all text, Quick redact menu |
| Undo / Redo, Zoom | ✅ ⌘Z / ⇧⌘Z, pinch, ⌘= / ⌘- / ⌘0 / ⌘9 |
| Save as (PNG / JPG / GIF), Copy, Share, Print, Open with | ✅ Plus HEIC, TIFF and BMP; macOS share sheet; Print; Open with ▸ |
| Settings: auto-copy, auto-save screenshots and recordings, ask to save edits, multiple windows, add border (color and width), mic and system audio defaults, theme | ✅ Same settings, plus shortcut recorders, frame rate and Open at login |
| Open an existing image to mark up | ✅ ⌘O, drag onto the Dock icon, or "Open With → Snipping" |

As on Windows, a snip started from the **app window** opens in the editor. A snip started with the **global shortcut** is copied to the clipboard (and auto-saved), and a toast lets you open it in the editor.

The app lives in the **menu bar**. Its Dock icon appears only while one of its windows is open, so the shortcuts stay instant.

### Default shortcuts

| Action | Shortcut |
|---|---|
| New snip | ⌃⇧S |
| Record screen (press again to stop) | ⌃⇧R |
| Text extractor | ⌃⇧T |

You can change them in **Settings**. The physical spot of Windows' `Win+Shift+S` on a Mac keyboard is ⌥⇧S, but since macOS 15 the system rejects global shortcuts that use only ⌥/⌥⇧, and ⌘⇧S would take over "Save As" in every app. That's why the defaults use ⌃.

## Install on another Mac

`cargo xtask dmg` produces **`dist/Snipping.dmg`**, a universal app that runs natively on both Apple silicon and Intel and needs **macOS 15 Sequoia or later**.

1. Copy `Snipping.dmg` to the other Mac. Open it and drag **Snipping** into **Applications**.
2. The first time you open it, macOS blocks it because it isn't notarized by Apple. Go to **System Settings → Privacy & Security**, scroll down and click **Open Anyway** next to the Snipping message, then confirm.
   - In Terminal you can instead run `xattr -dr com.apple.quarantine /Applications/Snipping.app`.
3. Take a snip (⌃⇧S) and allow **Screen & System Audio Recording** when asked, then reopen Snipping.

Updates signed with the same local certificate keep that permission. For warning-free distribution to other people, sign with an Apple **Developer ID** certificate and notarize the app, which needs an Apple Developer Program membership.

## Build and run (Rust only)

Requirements: macOS 15 or later, [Rust](https://rustup.rs), and the Xcode Command Line Tools (for Apple's linker and SDK).

```sh
cargo xtask bundle    # release build → dist/Snipping.app (icon, Info.plist, ad-hoc signature)
cargo xtask run       # bundle + launch
cargo xtask install   # bundle + copy to /Applications (falls back to ~/Applications)
cargo xtask dmg       # bundle + dist/Snipping.dmg (drag-to-Applications installer)
cargo xtask setup-signing   # one-time: stable signing so permissions survive rebuilds
```

`xtask` is a small Rust program in `xtask/` that assembles the `.app` and renders the app icon from vectors using `tiny-skia`.

### First launch: Screen Recording permission

macOS requires permission before any app can capture the screen. On the first snip, Snipping asks for it and offers to open **System Settings → Privacy & Security → Screen & System Audio Recording**. Turn on **Snipping**, then reopen the app if macOS asks you to.

### Keep the permission across rebuilds (recommended)

```sh
cargo xtask setup-signing   # one-time; macOS asks for your password once
```

This creates a self-signed **"Snipping Local Code Signing"** certificate, trusted only for code signing. It lives in its own keychain at `~/Library/Keychains/snipping-signing.keychain-db`, whose random password is stored owner-only in `~/Library/Application Support/Snipping/signing/`.

Every build is then signed with it. macOS ties Screen Recording permission to that certificate rather than to one build, so you grant it once. Without this step, builds are ad-hoc signed, and you'd need to remove and re-add Snipping in Screen & System Audio Recording after each rebuild.

To undo it:

1. Delete that keychain: `security delete-keychain ~/Library/Keychains/snipping-signing.keychain-db`
2. Remove the certificate's trust setting in Keychain Access.
3. Delete the `signing` folder.

If something goes wrong (for example a save fails), details are logged to `~/Library/Logs/Snipping/snipping.log`.

## Self-test

```sh
cargo build && ./target/debug/snipping --selftest /tmp/snipping-selftest
```

The self-test runs 98 checks that don't need Screen Recording permission. They use synthetic "screenshots" and cover:

- OCR and email/phone detection
- encode/decode in all six image formats, and the clipboard
- overlay selection math: rectangle, window, freeform and color picker
- markup, undo/redo, crop and Quick redact
- the toast, Settings and global hotkey registration
- every markup tool driven by real mouse and keyboard events
- recorder start/stop/discard edge cases
- regression tests for bugs found in code review
- live captures of the app's own windows

It also writes PNG renders of every UI surface to the output folder.

## Project layout

```
src/
  main.rs       app entry point
  app.rs        app delegate, menus, menu-bar item, capture/record/text/color flows
  capture.rs    ScreenCaptureKit display capture, window list, crop/composite
  overlay.rs    full-screen snip overlay + capture bar
  editor.rs     editor window, toolbar, zoom, crop/text-action bars, save/share
  canvas.rs     markup canvas: pen, highlighter, eraser, shapes, ruler, protractor, crop, OCR overlay
  annot.rs      annotation model + rendering
  toolopts.rs   pen / highlighter / shape / eraser option popovers
  ocr.rs        Vision text recognition + email/phone detection
  recorder.rs   screen recording (SCStream + SCRecordingOutput, pause via segments)
  video.rs      recording preview + GIF export
  export.rs     ImageIO encoding, clipboard, save panel, print, share, open with
  toast.rs      notification toast
  prefs.rs      Settings window + shortcut recorder
  hotkey.rs     global shortcuts (Carbon RegisterEventHotKey)
  settings.rs   persisted settings (~/Library/Application Support/Snipping)
  selftest.rs   --selftest harness
xtask/          bundler + icon renderer
```

## Not included

These Windows features depend on Microsoft services or Windows-specific hardware: Bing visual search, Copilot / "Perfect screenshot" AI cropping, and pen-pressure inking.
