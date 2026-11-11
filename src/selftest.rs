//! `snipping --selftest <dir>`: exercises the app end-to-end without needing
//! Screen Recording permission (synthetic "screenshots"), and renders every
//! UI surface to PNG for inspection.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AllocAnyThread, Message};
use objc2_app_kit::*;
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::*;
use objc2_foundation::*;

use crate::annot::{pt, Item, ShapeKind};
use crate::capture::{self, bitmap_context, image_size, DisplayShot, Frozen, ScreenInfo, WinInfo};
use crate::editor::Editor;
use crate::export::{self, ImageFormat};
use crate::overlay::{self, OverlayResult, Purpose};
use crate::settings::SnipMode;
use crate::util::{self, after, mtm, ns, rect, Rgba};

struct Report {
    out: PathBuf,
    passed: Vec<String>,
    failed: Vec<String>,
}

thread_local! {
    static REPORT: RefCell<Option<Report>> = const { RefCell::new(None) };
    static PENDING: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Capture one of our own windows exactly as composited on screen (uses the
/// current-process ScreenCaptureKit API, which needs no TCC permission).
fn capture_own_window(window: Retained<NSWindow>, name: &'static str) {
    use block2::RcBlock;
    use objc2_screen_capture_kit::*;
    PENDING.with(|p| p.set(p.get() + 1));
    let wid = window.windowNumber() as u32;
    let frame = window.frame();
    let scale = window.backingScaleFactor();
    let out = out_dir().join(name);
    let block = RcBlock::new(move |content: *mut SCShareableContent, _err: *mut NSError| {
        let done = || util::on_main(|| PENDING.with(|p| p.set(p.get() - 1)));
        let Some(content) = (unsafe { content.as_ref() }) else {
            util::on_main(move || check(&format!("live capture {name}"), false, "no shareable content"));
            done();
            return;
        };
        let windows = unsafe { content.windows() };
        let Some(w) = windows.iter().find(|w| unsafe { w.windowID() } == wid) else {
            util::on_main(move || check(&format!("live capture {name}"), false, "window not found"));
            done();
            return;
        };
        unsafe {
            let filter = SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &w);
            let cfg = SCStreamConfiguration::new();
            cfg.setWidth((frame.size.width * scale) as usize);
            cfg.setHeight((frame.size.height * scale) as usize);
            cfg.setShowsCursor(false);
            let out = out.clone();
            let handler = RcBlock::new(move |img: *mut CGImage, err: *mut NSError| {
                let ok = match std::ptr::NonNull::new(img) {
                    Some(img) => {
                        let img: CFRetained<CGImage> = CFRetained::retain(img);
                        let ok = export::write_image(&img, &out).is_ok();
                        // Same image through the real auto-save path (background thread).
                        let bg = out.with_extension("bg.png");
                        let img2 = img.clone();
                        util::on_main(move || {
                            // Bookkeeping lives on the main thread (thread-local counter).
                            hold();
                            util::background(move || {
                                let res = export::write_image(&img2, &bg);
                                let size = std::fs::metadata(&bg).map(|m| m.len()).unwrap_or(0);
                                util::on_main(move || {
                                    check(&format!("background save of ScreenCaptureKit image ({name})"), res.is_ok() && size > 0, &format!("{res:?}, {size} bytes"));
                                    release();
                                });
                            });
                        });
                        ok
                    }
                    None => false,
                };
                let msg = if err.is_null() { String::new() } else { (*err).localizedDescription().to_string() };
                util::on_main(move || {
                    check(&format!("live capture {name}"), ok, &msg);
                    PENDING.with(|p| p.set(p.get() - 1));
                });
            });
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(&filter, &cfg, Some(&handler));
        }
    });
    unsafe { SCShareableContent::getCurrentProcessShareableContentWithCompletionHandler(&block) };
}

fn check(name: &str, ok: bool, detail: &str) {
    let line = if detail.is_empty() { name.to_string() } else { format!("{name}: {detail}") };
    println!("{} {line}", if ok { "PASS" } else { "FAIL" });
    REPORT.with(|r| {
        if let Some(r) = r.borrow_mut().as_mut() {
            if ok {
                r.passed.push(line);
            } else {
                r.failed.push(line);
            }
        }
    });
}

fn out_dir() -> PathBuf {
    REPORT.with(|r| r.borrow().as_ref().map(|r| r.out.clone()).unwrap_or_default())
}

/// Draw a fake desktop/app screenshot with real text for OCR.
fn synthetic_screenshot(w: usize, h: usize, scale: f64) -> CFRetained<CGImage> {
    let ctx = bitmap_context(w, h, None).unwrap();
    let c = Some(&*ctx);
    // Background gradient-ish bands.
    CGContext::set_rgb_fill_color(c, 0.93, 0.95, 0.98, 1.0);
    CGContext::fill_rect(c, rect(0.0, 0.0, w as f64, h as f64));
    CGContext::set_rgb_fill_color(c, 0.16, 0.38, 0.85, 1.0);
    CGContext::fill_rect(c, rect(0.0, h as f64 - 70.0 * scale, w as f64, 70.0 * scale));
    CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::fill_rect(c, rect(40.0 * scale, 60.0 * scale, w as f64 - 80.0 * scale, h as f64 - 180.0 * scale));
    CGContext::set_rgb_fill_color(c, 0.98, 0.55, 0.2, 1.0);
    CGContext::fill_ellipse_in_rect(c, rect(w as f64 - 260.0 * scale, 90.0 * scale, 160.0 * scale, 160.0 * scale));

    let gc = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&gc));
    let draw = |text: &str, x: f64, y: f64, size: f64, color: &NSColor, bold: bool| {
        let font = if bold { NSFont::boldSystemFontOfSize(size * scale) } else { NSFont::systemFontOfSize(size * scale) };
        let keys: [&NSString; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
        let vals: [&AnyObject; 2] = [&font, color];
        let attrs = NSDictionary::from_slices(&keys, &vals);
        unsafe { ns(text).drawAtPoint_withAttributes(NSPoint::new(x * scale, y * scale), Some(&attrs)) };
    };
    let white = NSColor::whiteColor();
    let dark = NSColor::colorWithSRGBRed_green_blue_alpha(0.1, 0.1, 0.12, 1.0);
    let hpt = h as f64 / scale;
    draw("Quarterly Report — Snipping self-test", 60.0, hpt - 50.0, 24.0, &white, true);
    draw("Invoice #48213 for ACME Corporation", 70.0, hpt - 140.0, 22.0, &dark, true);
    draw("Contact: jane.doe@example.com", 70.0, hpt - 190.0, 20.0, &dark, false);
    draw("Phone: (415) 555-0132", 70.0, hpt - 230.0, 20.0, &dark, false);
    draw("The quick brown fox jumps over the lazy dog.", 70.0, hpt - 280.0, 20.0, &dark, false);
    draw("Total due: $1,250.00", 70.0, hpt - 330.0, 20.0, &dark, true);
    NSGraphicsContext::restoreGraphicsState_class();
    CGBitmapContextCreateImage(c).unwrap()
}

fn save_png(img: &CGImage, name: &str) -> PathBuf {
    let p = out_dir().join(name);
    let _ = export::write_image(img, &p);
    p
}

fn render_view(view: &NSView, name: &str) -> Option<PathBuf> {
    view.layoutSubtreeIfNeeded();
    let bounds = view.bounds();
    let rep = view.bitmapImageRepForCachingDisplayInRect(bounds)?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    let p = out_dir().join(name);
    std::fs::write(&p, data.to_vec()).ok()?;
    Some(p)
}

fn render_window(window: &NSWindow, name: &str) -> Option<PathBuf> {
    window.displayIfNeeded();
    let content = window.contentView()?;
    // The theme frame includes the toolbar.
    let frame_view = unsafe { content.superview() }.unwrap_or(content);
    render_view(&frame_view, name)
}

pub fn run(out: PathBuf) {
    let _ = std::fs::create_dir_all(&out);
    // Isolate settings from the user's real ones.
    REPORT.with(|r| *r.borrow_mut() = Some(Report { out, passed: Vec::new(), failed: Vec::new() }));
    let app = NSApplication::sharedApplication(mtm());
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let steps: Vec<Box<dyn FnOnce()>> = vec![
        Box::new(test_environment),
        Box::new(test_export_and_ocr),
        Box::new(test_overlay),
        Box::new(test_editor),
        Box::new(test_canvas_interactions),
        Box::new(test_deliver_snip),
        Box::new(test_review_regressions),
        Box::new(test_recorder_states),
        Box::new(test_overlay_regressions),
        Box::new(test_toast_and_settings),
        Box::new(test_hotkeys),
        Box::new(test_live_about),
        Box::new(test_live_editor),
        Box::new(test_live_overlay),
    ];
    run_steps(steps);
}

fn run_steps(mut steps: Vec<Box<dyn FnOnce()>>) {
    if steps.is_empty() {
        finish();
        return;
    }
    let step = steps.remove(0);
    step();
    after(0.4, move || run_steps(steps));
}

fn finish() {
    if PENDING.with(|p| p.get()) > 0 {
        after(0.2, finish);
        return;
    }
    let (passed, failed, out) = REPORT.with(|r| {
        let r = r.borrow();
        let r = r.as_ref().unwrap();
        (r.passed.clone(), r.failed.clone(), r.out.clone())
    });
    let summary = format!(
        "{} passed, {} failed\n\nPASSED:\n{}\n\nFAILED:\n{}\n",
        passed.len(),
        failed.len(),
        passed.join("\n"),
        failed.join("\n")
    );
    let _ = std::fs::write(out.join("report.txt"), &summary);
    println!("\n==== {} passed, {} failed ====", passed.len(), failed.len());
    std::process::exit(if failed.is_empty() { 0 } else { 1 });
}

fn test_environment() {
    let screens = capture::screens();
    check("screens detected", !screens.is_empty(), &format!("{} display(s): {:?}", screens.len(), screens.iter().map(|s| (s.display_id, s.frame.size.width, s.frame.size.height, s.scale)).collect::<Vec<_>>()));
    let wins = capture::window_list();
    check("window list", true, &format!("{} on-screen windows", wins.len()));
    check(
        "screen recording permission (informational)",
        true,
        if capture::has_permission() { "granted" } else { "not granted to this process (expected for the test runner)" },
    );
}

fn test_export_and_ocr() {
    let img = synthetic_screenshot(1600, 1000, 2.0);
    save_png(&img, "00-synthetic.png");
    for fmt in ImageFormat::ALL {
        let bytes = export::encode(&img, fmt).unwrap_or_default();
        check(&format!("encode {}", fmt.title()), bytes.len() > 1000, &format!("{} bytes", bytes.len()));
        let p = out_dir().join(format!("export.{}", fmt.ext()));
        let _ = std::fs::write(&p, &bytes);
        let back = export::load_image_file(&p);
        check(&format!("decode {}", fmt.title()), back.as_ref().map(|(i, _)| image_size(i) == (1600, 1000)).unwrap_or(false), "");
    }
    // Clipboard round-trip.
    export::copy_image(&img);
    let pb = NSPasteboard::generalPasteboard();
    let has_png = unsafe { pb.dataForType(NSPasteboardTypePNG) }.map(|d| d.length() > 0).unwrap_or(false);
    check("clipboard image (PNG)", has_png, "");
    export::copy_text("snipping-selftest");
    let s = unsafe { pb.stringForType(NSPasteboardTypeString) }.map(|s| s.to_string());
    check("clipboard text", s.as_deref() == Some("snipping-selftest"), "");

    let t0 = std::time::Instant::now();
    let lines = crate::ocr::recognize_sync(&img);
    let text = lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join(" | ");
    check("OCR recognizes text", text.contains("quick brown fox"), &format!("{:?}: {text}", t0.elapsed()));
    let emails = lines.iter().flat_map(|l| l.sensitive.iter()).filter(|(_, k)| *k == crate::ocr::Sensitive::Email).count();
    let phones = lines.iter().flat_map(|l| l.sensitive.iter()).filter(|(_, k)| *k == crate::ocr::Sensitive::Phone).count();
    check("Quick redact finds email", emails >= 1, &format!("{emails}"));
    check("Quick redact finds phone", phones >= 1, &format!("{phones}"));
}

fn fake_frozen() -> Frozen {
    let infos = capture::screens();
    let displays: Vec<DisplayShot> = infos
        .iter()
        .map(|info| {
            let w = (info.frame.size.width * info.scale) as usize;
            let h = (info.frame.size.height * info.scale) as usize;
            DisplayShot { info: *info, image: synthetic_screenshot(w, h, info.scale) }
        })
        .collect();
    let first = infos[0].frame;
    let windows = vec![WinInfo {
        id: 1,
        frame: rect(first.origin.x + 100.0, first.origin.y + 120.0, 600.0, 400.0),
        owner: "Test".into(),
        title: "Fake window".into(),
    }];
    Frozen { displays, windows }
}

fn test_overlay() {
    overlay::set_test_hidden(true);
    let result: Rc<RefCell<Option<OverlayResult>>> = Rc::default();
    let r2 = result.clone();
    let frozen = fake_frozen();
    let d0: ScreenInfo = frozen.displays[0].info;
    overlay::show(overlay::Request {
        purpose: Purpose::Snip,
        mode: SnipMode::Rectangle,
        frozen,
        show_bar: true,
        done: Box::new(move |r| *r2.borrow_mut() = Some(r)),
    });
    let o = d0.frame.origin;
    let a = CGPoint::new(o.x + 200.0, o.y + 300.0);
    let b = CGPoint::new(o.x + 700.0, o.y + 600.0);
    overlay::simulate(0, a, b);
    if let Some(img) = overlay::render_first_display() {
        save_png(&img, "10-overlay-rect-drag.png");
        check("overlay renders", true, "");
    } else {
        check("overlay renders", false, "no image");
    }
    overlay::simulate_up(0, b);
    let got = result.borrow_mut().take();
    match got {
        Some(OverlayResult::Image { image, scale }) => {
            let (w, h) = image_size(&image);
            let expect = ((500.0 * scale).round() as usize, (300.0 * scale).round() as usize);
            check("rectangle snip size", (w, h) == expect, &format!("{w}x{h} (expected {}x{}) scale {scale}", expect.0, expect.1));
            save_png(&image, "11-rect-snip.png");
        }
        _ => check("rectangle snip size", false, "no image result"),
    }

    // Window mode + freeform + color picker on a fresh session.
    let result2: Rc<RefCell<Option<OverlayResult>>> = Rc::default();
    let r3 = result2.clone();
    overlay::show(overlay::Request {
        purpose: Purpose::Snip,
        mode: SnipMode::Window,
        frozen: fake_frozen(),
        show_bar: true,
        done: Box::new(move |r| *r3.borrow_mut() = Some(r)),
    });
    let inside = CGPoint::new(o.x + 300.0, o.y + 300.0);
    overlay::simulate_move(0, inside);
    if let Some(img) = overlay::render_first_display() {
        save_png(&img, "12-overlay-window-hover.png");
    }
    overlay::simulate_down(0, inside);
    overlay::simulate_up(0, inside);
    match result2.borrow_mut().take() {
        Some(OverlayResult::Image { image, scale }) => {
            let (w, h) = image_size(&image);
            check("window snip size", w == (600.0 * scale) as usize && h == (400.0 * scale) as usize, &format!("{w}x{h}"));
        }
        _ => check("window snip size", false, "no result"),
    }

    let result3: Rc<RefCell<Option<OverlayResult>>> = Rc::default();
    let r4 = result3.clone();
    overlay::show(overlay::Request {
        purpose: Purpose::Snip,
        mode: SnipMode::Freeform,
        frozen: fake_frozen(),
        show_bar: true,
        done: Box::new(move |r| *r4.borrow_mut() = Some(r)),
    });
    let c = CGPoint::new(o.x + 500.0, o.y + 450.0);
    overlay::simulate_down(0, CGPoint::new(c.x + 150.0, c.y));
    for i in 0..=36 {
        let a = i as f64 / 36.0 * std::f64::consts::TAU;
        // A wobbly lasso.
        let r = 1.0 + 0.15 * (a * 5.0).sin();
        overlay::simulate_drag(0, CGPoint::new(c.x + 150.0 * r * a.cos(), c.y + 100.0 * r * a.sin()));
    }
    if let Some(img) = overlay::render_first_display() {
        save_png(&img, "13-overlay-freeform.png");
    }
    overlay::simulate_up(0, c);
    match result3.borrow_mut().take() {
        Some(OverlayResult::Image { image, .. }) => {
            let (w, h) = image_size(&image);
            check("freeform snip", w > 100 && h > 100, &format!("{w}x{h}"));
            save_png(&image, "14-freeform-snip.png");
        }
        _ => check("freeform snip", false, "no result"),
    }

    // Color picker.
    let result4: Rc<RefCell<Option<OverlayResult>>> = Rc::default();
    let r5 = result4.clone();
    overlay::show(overlay::Request {
        purpose: Purpose::ColorPick,
        mode: SnipMode::Rectangle,
        frozen: fake_frozen(),
        show_bar: true,
        done: Box::new(move |r| *r5.borrow_mut() = Some(r)),
    });
    // Title bar band is blue (0.16, 0.38, 0.85) at the top of the display.
    let p = CGPoint::new(o.x + 30.0, o.y + d0.frame.size.height - 30.0);
    overlay::simulate_move(0, p);
    if let Some(img) = overlay::render_first_display() {
        save_png(&img, "15-overlay-color-picker.png");
    }
    overlay::simulate(0, p, p);
    match result4.borrow_mut().take() {
        Some(OverlayResult::Color(c)) => {
            let ok = (c.b - 0.85).abs() < 0.12 && c.r < 0.3;
            check("color picker", ok, &c.to_hex());
        }
        _ => check("color picker", false, "no result"),
    }
    overlay::set_test_hidden(false);
}

fn test_editor() {
    let img = synthetic_screenshot(1600, 1000, 2.0);
    let ed = Editor::new();
    ed.load(img, 2.0);
    let c = &ed.canvas;
    c.push_item(Item::Ink {
        highlighter: true,
        color: Rgba::hex(0xFFEB3B),
        width: 32.0,
        pts: vec![pt(140.0, 300.0), pt(500.0, 300.0), pt(820.0, 302.0)],
    });
    c.push_item(Item::Ink {
        highlighter: false,
        color: Rgba::hex(0xE81123),
        width: 8.0,
        pts: (0..60).map(|i| {
            let t = i as f64 / 59.0 * std::f64::consts::TAU;
            pt(1200.0 + 120.0 * t.cos(), 520.0 + 80.0 * t.sin())
        }).collect(),
    });
    c.push_item(Item::Shape { kind: ShapeKind::Arrow, a: pt(900.0, 760.0), b: pt(1130.0, 560.0), stroke: Rgba::hex(0x0078D4), fill: None, width: 8.0 });
    c.push_item(Item::Shape { kind: ShapeKind::Rect, a: pt(130.0, 560.0), b: pt(760.0, 700.0), stroke: Rgba::hex(0x16C60C), fill: None, width: 6.0 });
    c.push_item(Item::Shape { kind: ShapeKind::Ellipse, a: pt(1300.0, 150.0), b: pt(1500.0, 260.0), stroke: Rgba::hex(0x5C2D91), fill: Some(Rgba::hex(0xFFD700)), width: 4.0 });
    c.select(Some(3));
    check("annotations added", c.item_count() == 5, &format!("{}", c.item_count()));
    check("undo available", c.can_undo(), "");
    c.undo();
    check("undo removes item", c.item_count() == 4, &format!("{}", c.item_count()));
    c.redo();
    check("redo restores item", c.item_count() == 5, "");

    // OCR overlay + quick redact.
    let lines = crate::ocr::recognize_sync(&c.base_image().unwrap());
    c.set_ocr(lines);
    c.ocr_select_all();
    let n = c.redact(true, true);
    check("quick redact applied", n >= 2, &format!("{n} region(s)"));
    let text = c.ocr_text(true).unwrap_or_default();
    check("text actions select-all", text.contains("Invoice"), "");
    ed.window.setFrame_display(rect(0.0, 0.0, 1100.0, 760.0), true);
    ed.zoom_fit();
    render_window(&ed.window, "20-editor-text-actions.png");
    c.clear_ocr();
    c.toggle_ruler();
    c.toggle_protractor();
    render_window(&ed.window, "21-editor-markup-ruler.png");
    if let Some(tb) = ed.window.toolbar() {
        for (i, item) in tb.items().iter().enumerate() {
            if let Some(v) = item.view() {
                render_view(&v, &format!("25-toolbar-item-{i}.png"));
            }
        }
    }
    c.toggle_ruler();
    c.toggle_protractor();

    let flat = c.flatten().unwrap();
    save_png(&flat, "22-flattened.png");
    check("flatten keeps size", image_size(&flat) == (1600, 1000), "");

    // Crop.
    c.begin_crop();
    c.set_crop_rect(rect(100.0, 100.0, 800.0, 500.0));
    render_window(&ed.window, "23-editor-crop.png");
    c.apply_crop();
    let (w, h) = image_size(&c.base_image().unwrap());
    check("crop", (w, h) == (800, 500), &format!("{w}x{h}"));
    c.undo();
    let (w, h) = image_size(&c.base_image().unwrap());
    check("undo crop", (w, h) == (1600, 1000), &format!("{w}x{h}"));
    check("dirty after edits", c.is_dirty(), "");

    // Empty-state window.
    let empty = Editor::new();
    empty.window.setFrame_display(rect(0.0, 0.0, 900.0, 520.0), true);
    render_window(&empty.window, "24-editor-empty.png");
    c.mark_saved();
    ed.window.close();
    empty.window.close();
}

fn test_toast_and_settings() {
    let img = synthetic_screenshot(800, 500, 2.0);
    crate::toast::show(Some((&img, 2.0)), "Snip copied to clipboard and saved", "Select here to mark up and share.", None);
    if let Some(w) = crate::toast::current_window() {
        if let Some(v) = w.contentView() {
            render_view(&v, "30-toast.png");
            check("toast shown", w.isVisible(), "");
        }
    }
    crate::toast::hide_now();

    crate::prefs::open();
    if let Some(w) = crate::prefs::window() {
        w.setFrame_display(rect(0.0, 0.0, w.frame().size.width, 1400.0), true);
        if let Some(sv) = w.contentView().and_then(|c| c.downcast::<NSScrollView>().ok()) {
            if let Some(doc) = sv.documentView() {
                render_view(&doc, "31-settings.png");
                check("settings renders", doc.frame().size.height > 400.0, &format!("{:?}", doc.frame().size));
            }
        }
        w.close();
    }
}

fn test_hotkeys() {
    let errors = crate::hotkey::register(&[
        (crate::hotkey::HotkeyAction::Snip, crate::hotkey::Hotkey::DEFAULT_SNIP),
        (crate::hotkey::HotkeyAction::Record, crate::hotkey::Hotkey::DEFAULT_RECORD),
        (crate::hotkey::HotkeyAction::TextExtract, crate::hotkey::Hotkey::DEFAULT_TEXT),
    ]);
    check("global hotkeys register", errors.is_empty(), &format!("{errors:?}"));
    crate::hotkey::unregister_all();
    let bad = crate::hotkey::Hotkey { code: 0x01, mods: crate::hotkey::MOD_OPTION | crate::hotkey::MOD_SHIFT };
    check("rejects ⌥⇧ hotkeys (macOS 15 rule)", bad.validate().is_err(), "");
    let _ = (Path::new(""), Retained::<NSObject>::into_raw, CGImage::width, NSObject::alloc);
}

thread_local! {
    static LIVE: RefCell<Vec<Rc<Editor>>> = const { RefCell::new(Vec::new()) };
}

fn test_live_editor() {
    let img = synthetic_screenshot(1600, 1000, 2.0);
    let ed = Editor::new();
    ed.load(img, 2.0);
    ed.canvas.push_item(Item::Shape { kind: ShapeKind::Arrow, a: pt(900.0, 760.0), b: pt(1130.0, 560.0), stroke: Rgba::hex(0xE81123), fill: None, width: 8.0 });
    ed.canvas.push_item(Item::Ink { highlighter: true, color: Rgba::hex(0xFFEB3B), width: 32.0, pts: vec![pt(140.0, 300.0), pt(820.0, 302.0)] });
    ed.canvas.toggle_ruler();
    ed.window.setFrame_display(rect(80.0, 80.0, 1180.0, 760.0), true);
    ed.window.orderFrontRegardless();
    ed.zoom_fit();
    let empty = Editor::new();
    empty.window.setFrame_display(rect(140.0, 140.0, 900.0, 520.0), true);
    empty.window.orderFrontRegardless();
    crate::prefs::open();
    let w1 = ed.window.clone();
    let w2 = empty.window.clone();
    LIVE.with(|l| l.borrow_mut().extend([ed, empty]));
    hold();
    after(0.8, move || {
        release();
        capture_own_window(w1, "40-live-editor.png");
        capture_own_window(w2, "41-live-editor-empty.png");
        if let Some(p) = crate::prefs::window() {
            capture_own_window(p, "42-live-settings.png");
        }
    });
}

fn hold() {
    PENDING.with(|p| p.set(p.get() + 1));
}

fn release() {
    PENDING.with(|p| p.set(p.get() - 1));
}

fn test_live_overlay() {
    // Wait for the editor captures to finish before covering the screen.
    if PENDING.with(|p| p.get()) > 0 {
        hold();
        after(0.3, || {
            release();
            test_live_overlay();
        });
        return;
    }
    LIVE.with(|l| {
        for e in l.borrow().iter() {
            e.canvas.mark_saved();
            e.window.close();
        }
        l.borrow_mut().clear();
    });
    if let Some(p) = crate::prefs::window() {
        p.close();
    }
    overlay::show(overlay::Request {
        purpose: Purpose::Snip,
        mode: SnipMode::Rectangle,
        frozen: fake_frozen(),
        show_bar: true,
        done: Box::new(|_| {}),
    });
    // Recording toolbar for the same pass.
    let screen = capture::screens()[0];
    let f = screen.frame;
    crate::recorder::prepare(screen, rect(f.origin.x + 300.0, f.origin.y + 200.0, 700.0, 420.0), crate::app::Origin::Menu);
    hold();
    after(0.8, || {
        let app = NSApplication::sharedApplication(mtm());
        let names = ["43-live-overlay-a.png", "43-live-overlay-b.png", "43-live-overlay-c.png"];
        let mut i = 0;
        for w in app.windows().iter() {
            let name = w.className().to_string();
            if name.contains("OverlayWindow") && w.isVisible() && i < names.len() {
                capture_own_window(w.retain(), names[i]);
                i += 1;
            }
        }
        for w in app.windows().iter() {
            if w.isVisible() && w.level() > NSStatusWindowLevel && !w.className().to_string().contains("Overlay") {
                capture_own_window(w.retain(), "44-live-record-bar.png");
            }
        }
        after(1.5, || {
            overlay::cancel();
            crate::recorder::discard();
            release();
        });
    });
}

// ---------------------------------------------------------------------------
// Real mouse/keyboard events through the canvas (every tool)
// ---------------------------------------------------------------------------

fn send_mouse(canvas: &crate::canvas::CanvasView, kind: NSEventType, px: CGPoint, flags: NSEventModifierFlags) {
    let scale = canvas.scale();
    let win = canvas.window().expect("canvas in window");
    let loc = canvas.convertPoint_toView(NSPoint::new(px.x / scale, px.y / scale), None);
    let ev = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind, loc, flags, 0.0, win.windowNumber(), None, 0, 1, 1.0,
    )
    .expect("mouse event");
    match kind {
        NSEventType::LeftMouseDown => canvas.mouseDown(&ev),
        NSEventType::LeftMouseDragged => canvas.mouseDragged(&ev),
        NSEventType::LeftMouseUp => canvas.mouseUp(&ev),
        _ => canvas.mouseMoved(&ev),
    }
}

fn send_drag(canvas: &crate::canvas::CanvasView, a: CGPoint, b: CGPoint, flags: NSEventModifierFlags) {
    send_mouse(canvas, NSEventType::LeftMouseDown, a, flags);
    for i in 1..=10 {
        let t = i as f64 / 10.0;
        send_mouse(canvas, NSEventType::LeftMouseDragged, CGPoint::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t), flags);
    }
    send_mouse(canvas, NSEventType::LeftMouseUp, b, flags);
}

fn send_key(canvas: &crate::canvas::CanvasView, code: u16, chars: &str) {
    let win = canvas.window().expect("canvas in window");
    let ev = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        NSEventType::KeyDown, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 0.0, win.windowNumber(), None, &ns(chars), &ns(chars), false, code,
    )
    .expect("key event");
    canvas.keyDown(&ev);
}

fn test_canvas_interactions() {
    use crate::canvas::Tool;
    let none = NSEventModifierFlags::empty();
    let img = synthetic_screenshot(1600, 1000, 2.0);
    let ed = Editor::new();
    ed.load(img, 2.0);
    ed.window.setFrame_display(rect(0.0, 0.0, 1200.0, 800.0), true);
    ed.zoom_actual();
    let c = &ed.canvas;
    let count = || c.st().items.len();

    // Pen
    c.set_tool(Tool::Pen);
    send_drag(c, pt(200.0, 200.0), pt(600.0, 260.0), none);
    let ok = count() == 1 && matches!(&c.st().items[0], Item::Ink { highlighter: false, pts, .. } if pts.len() >= 5);
    check("pen draws a stroke", ok, &format!("{} items", count()));
    c.undo();
    check("undo pen stroke", count() == 0, "");
    c.redo();
    check("redo pen stroke", count() == 1, "");

    // Highlighter
    c.set_tool(Tool::Highlighter);
    send_drag(c, pt(200.0, 420.0), pt(700.0, 420.0), none);
    check("highlighter draws", count() == 2 && matches!(&c.st().items[1], Item::Ink { highlighter: true, .. }), "");

    // Eraser crosses the pen stroke only.
    c.set_tool(Tool::Eraser);
    send_drag(c, pt(400.0, 120.0), pt(400.0, 330.0), none);
    check("eraser removes touched stroke", count() == 1 && matches!(&c.st().items[0], Item::Ink { highlighter: true, .. }), &format!("{} items", count()));
    c.undo();
    check("undo erase", count() == 2, "");
    // A click on empty space erases nothing and adds no undo step.
    let undo_before = c.can_undo();
    send_drag(c, pt(1500.0, 950.0), pt(1500.0, 950.0), none);
    check("eraser on empty space is a no-op", count() == 2 && c.can_undo() == undo_before, "");

    // Shapes: draw, resize by handle, move, delete.
    c.set_tool(Tool::Shape);
    c.update_style(|s| { s.shape_kind = ShapeKind::Rect; s.shape_fill = None; });
    send_drag(c, pt(900.0, 300.0), pt(1200.0, 500.0), none);
    let shape = |c: &crate::canvas::CanvasView| c.st().items.last().and_then(|it| match it {
        Item::Shape { a, b, .. } => Some((*a, *b)),
        _ => None,
    });
    check("shape drawn and selected", count() == 3 && c.st().selected == Some(2), &format!("{:?}", shape(c)));
    send_drag(c, pt(1200.0, 500.0), pt(1300.0, 600.0), none);
    let r = shape(c);
    check("shape resized via handle", r.map(|(_, b)| (b.x - 1300.0).abs() < 1.0 && (b.y - 600.0).abs() < 1.0).unwrap_or(false), &format!("{r:?}"));
    send_drag(c, pt(900.0, 450.0), pt(950.0, 480.0), none);
    let m = shape(c);
    check("shape moved by dragging its edge", m.map(|(a, _)| (a.x - 950.0).abs() < 1.0 && (a.y - 330.0).abs() < 1.0).unwrap_or(false), &format!("{m:?}"));
    send_key(c, 51, "\u{7f}");
    check("delete key removes selected shape", count() == 2 && c.st().selected.is_none(), &format!("{} items", count()));
    c.update_style(|s| s.shape_kind = ShapeKind::Arrow);
    send_drag(c, pt(100.0, 800.0), pt(400.0, 830.0), NSEventModifierFlags::Shift);
    let a = shape(c);
    check("shift constrains arrow to 0/45/90°", a.map(|(a, b)| (a.y - b.y).abs() < 0.5).unwrap_or(false), &format!("{a:?}"));
    // Clicking empty space with the shape tool deselects without adding anything.
    let n = count();
    send_drag(c, pt(1500.0, 120.0), pt(1500.0, 120.0), none);
    check("click on empty space adds no shape", count() == n && c.st().selected.is_none(), "");

    // Ruler: strokes snap to the edge; dragging the body moves it.
    c.set_tool(Tool::Pen);
    c.toggle_ruler();
    let ruler = c.st().ruler.unwrap();
    let edge = ruler.c.y - ruler.thick / 2.0;
    let n = count();
    send_drag(c, pt(ruler.c.x - 150.0, edge - 6.0), pt(ruler.c.x + 150.0, edge - 30.0), none);
    let width = c.style().pen_width * c.scale();
    let snapped = c.st().items.get(n).map(|it| match it {
        Item::Ink { pts, .. } => pts.iter().all(|p| (p.y - (edge - width / 2.0)).abs() < 0.6),
        _ => false,
    });
    check("pen snaps to ruler edge", snapped == Some(true), &format!("{snapped:?}"));
    send_drag(c, ruler.c, pt(ruler.c.x + 60.0, ruler.c.y + 40.0), none);
    let moved = c.st().ruler.unwrap();
    check("ruler moves by dragging", (moved.c.x - ruler.c.x - 60.0).abs() < 1.0 && (moved.c.y - ruler.c.y - 40.0).abs() < 1.0, "");
    c.toggle_ruler();

    // Protractor arc snapping.
    c.toggle_protractor();
    let pr = c.st().protractor.unwrap();
    let n = count();
    send_drag(c, pt(pr.c.x + pr.r + 5.0, pr.c.y), pt(pr.c.x, pr.c.y - pr.r - 5.0), none);
    let on_arc = c.st().items.get(n).map(|it| match it {
        Item::Ink { pts, .. } => pts.iter().all(|p| (crate::annot::dist(*p, pr.c) - (pr.r + width / 2.0)).abs() < 0.6),
        _ => false,
    });
    check("pen snaps to protractor arc", on_arc == Some(true), &format!("{on_arc:?}"));
    c.toggle_protractor();

    // Pan (no tool) must not crash or edit.
    c.set_tool(Tool::None);
    let n = count();
    send_drag(c, pt(800.0, 500.0), pt(700.0, 400.0), none);
    check("pan leaves markup unchanged", count() == n, "");

    // Crop: drag the top-left handle, apply with Return, undo.
    c.begin_crop();
    send_drag(c, pt(0.0, 0.0), pt(100.0, 50.0), none);
    let cr = c.st().crop;
    check("crop handle drag", cr.map(|r| (r.origin.x - 100.0).abs() < 1.0 && (r.origin.y - 50.0).abs() < 1.0).unwrap_or(false), &format!("{cr:?}"));
    send_key(c, 36, "\r");
    let (w, h) = image_size(&c.base_image().unwrap());
    check("Return applies crop", (w, h) == (1500, 950) && c.st().crop.is_none(), &format!("{w}x{h}"));
    c.undo();
    let (w, h) = image_size(&c.base_image().unwrap());
    check("undo crop restores size", (w, h) == (1600, 1000), &format!("{w}x{h}"));

    // Text actions: click a line to select it, Esc closes.
    let lines = crate::ocr::recognize_sync(&c.base_image().unwrap());
    let first = lines.iter().position(|l| l.text.contains("Invoice")).unwrap_or(0);
    let target = lines.get(first).map(|l| CGPoint::new(l.rect.origin.x + l.rect.size.width / 2.0, l.rect.origin.y + l.rect.size.height / 2.0));
    c.set_ocr(lines);
    if let Some(p) = target {
        send_drag(c, p, p, none);
    }
    let sel = c.ocr_text(true).unwrap_or_default();
    check("click selects a text line", sel.contains("Invoice"), &sel);
    send_key(c, 53, "\u{1b}");
    check("Esc closes text actions", c.st().ocr.is_none(), "");

    // Undo everything, then redo everything.
    let mut steps = 0;
    while c.can_undo() && steps < 100 {
        c.undo();
        steps += 1;
    }
    check("undo back to the original", count() == 0, &format!("{steps} steps"));
    while c.can_redo() {
        c.redo();
    }
    let (w, h) = image_size(&c.base_image().unwrap());
    check("redo replays every step", count() >= 4 && (w, h) == (1500, 950), &format!("{} items, {w}x{h}", count()));
    render_window(&ed.window, "26-editor-after-interactions.png");
    c.mark_saved();
    ed.window.close();
}

fn test_deliver_snip() {
    let dir = out_dir().join("autosave");
    let _ = std::fs::remove_dir_all(&dir);
    let d2 = dir.to_string_lossy().into_owned();
    crate::settings::update(|s| {
        s.save_dir = d2.clone();
        s.auto_save = true;
        s.auto_copy = true;
        s.show_notification = true;
    });
    let img = synthetic_screenshot(1200, 700, 2.0);
    crate::app::deliver_snip(img, 2.0, crate::app::Origin::Menu);
    check("toast shown after hotkey-style snip", crate::toast::current_window().map(|w| w.isVisible()).unwrap_or(false), "");
    hold();
    after(1.5, move || {
        let files: Vec<_> = std::fs::read_dir(&dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        let good = files.iter().filter(|p| std::fs::metadata(p).map(|m| m.len() > 1000).unwrap_or(false)).count();
        check("auto-save writes the snip", files.len() == 1 && good == 1, &format!("{files:?}"));
        let pb = NSPasteboard::generalPasteboard();
        let has_png = unsafe { pb.dataForType(NSPasteboardTypePNG) }.map(|d| d.length() > 0).unwrap_or(false);
        check("auto-copy puts the snip on the clipboard", has_png, "");
        crate::toast::hide_now();
        release();
    });
}

/// Regression tests for issues found in code review.
fn test_review_regressions() {
    use crate::canvas::Tool;
    let none = NSEventModifierFlags::empty();
    let ed = Editor::new();
    ed.load(synthetic_screenshot(1600, 1000, 2.0), 2.0);
    ed.window.setFrame_display(rect(0.0, 0.0, 1200.0, 800.0), true);
    ed.zoom_actual();
    let c = ed.canvas.clone();

    // Shape restyle only changes the edited attribute; slider drags share one undo step.
    c.set_tool(Tool::Shape);
    c.update_style(|s| { s.shape_kind = ShapeKind::Rect; s.shape_stroke = Rgba::hex(0xE81123); s.shape_width = 4.0; });
    send_drag(&c, pt(100.0, 100.0), pt(400.0, 300.0), none);
    c.select(None);
    c.update_style(|s| { s.shape_kind = ShapeKind::Arrow; s.shape_stroke = Rgba::hex(0x0078D4); });
    send_drag(&c, pt(600.0, 600.0), pt(900.0, 700.0), none);
    // Select the rectangle by clicking its edge.
    send_drag(&c, pt(100.0, 200.0), pt(100.0, 200.0), none);
    let depth = c.undo_depth();
    for w in [5.0, 6.0, 7.0, 8.0, 9.0] {
        c.update_style(|s| s.shape_width = w);
    }
    let rect_item = c.st().items.first().cloned();
    let ok = matches!(rect_item, Some(Item::Shape { kind: ShapeKind::Rect, stroke, width, .. }) if stroke == Rgba::hex(0xE81123) && (width - 18.0).abs() < 0.01);
    check("restyling a shape keeps its kind and color", ok, &format!("{rect_item:?}"));
    check("slider drag = one undo step", c.undo_depth() == depth + 1, &format!("{} -> {}", depth, c.undo_depth()));

    // Clicking a handle without moving adds no undo step.
    let depth = c.undo_depth();
    send_drag(&c, pt(400.0, 300.0), pt(400.0, 300.0), none);
    check("handle click without drag adds no undo step", c.undo_depth() == depth, "");

    // Delete pressed mid-drag is ignored (no corrupted gesture).
    let n = c.item_count();
    send_mouse(&c, NSEventType::LeftMouseDown, pt(100.0, 200.0), none);
    send_key(&c, 51, "\u{7f}");
    send_mouse(&c, NSEventType::LeftMouseDragged, pt(150.0, 250.0), none);
    send_mouse(&c, NSEventType::LeftMouseUp, pt(150.0, 250.0), none);
    check("delete during drag is ignored", c.item_count() == n, &format!("{} -> {}", n, c.item_count()));
    let depth = c.undo_depth();
    c.undo();
    check("the drag is still undoable", c.undo_depth() == depth - 1, "");

    // Crop keeps the ruler on the image.
    c.toggle_ruler();
    let before = c.st().ruler.unwrap();
    c.begin_crop();
    c.set_crop_rect(rect(200.0, 100.0, 1000.0, 700.0));
    c.apply_crop();
    let moved = c.st().ruler.unwrap();
    check("crop shifts the ruler with the image", (moved.c.x - (before.c.x - 200.0)).abs() < 1.0 && (moved.c.y - (before.c.y - 100.0)).abs() < 1.0, &format!("{:?} -> {:?}", before.c, moved.c));
    c.toggle_ruler();

    // Crop mode, then a new snip: tools must be usable again.
    ed.crop_clicked();
    check("crop mode disables markup tools", !ed.tools_enabled(), "");
    ed.load(synthetic_screenshot(800, 600, 2.0), 2.0);
    check("new snip re-enables tools after crop mode", ed.tools_enabled() && c.st().crop.is_none(), "");

    // Text actions, then Crop before the scan finishes: the late OCR result
    // must not hijack crop mode.
    ed.text_actions_clicked();
    ed.crop_clicked();
    let ed2 = ed.clone();
    hold();
    after(2.0, move || {
        let c = &ed2.canvas;
        check(
            "late OCR result doesn't interrupt crop",
            c.tool() == Tool::Crop && c.st().ocr.is_none() && !ed2.text_button_on(),
            &format!("tool {:?}, ocr {}", c.tool(), c.st().ocr.is_some()),
        );
        ed2.canvas.mark_saved();
        ed2.window.close();
        release();
    });

    // OCR reading order is a total order (no panic) with staircase lines.
    let lines: Vec<crate::ocr::OcrLine> = (0..200)
        .map(|i| crate::ocr::OcrLine {
            text: format!("l{i}"),
            rect: rect(((i * 37) % 500) as f64, 100.0 + (i as f64) * 8.0, 50.0, 20.0),
            sensitive: Vec::new(),
        })
        .collect();
    let sorted = crate::ocr::reading_order(lines);
    let monotonic = sorted.windows(2).all(|w| w[0].rect.origin.y <= w[1].rect.origin.y + 20.0);
    check("OCR reading order sorts 200 staircase lines", sorted.len() == 200 && monotonic, "");
}

fn test_recorder_states() {
    use crate::recorder;
    let screen = capture::screens()[0];
    let f = screen.frame;
    let r = rect(f.origin.x + 100.0, f.origin.y + 100.0, 400.0, 300.0);
    recorder::prepare(screen, r, crate::app::Origin::Menu);
    check("recorder shows its toolbar", recorder::is_active(), "");
    recorder::discard();
    check("discard before start frees the recorder", !recorder::is_active() && !crate::app::is_busy(), "");

    recorder::prepare(screen, r, crate::app::Origin::Menu);
    recorder::press_start();
    recorder::stop();
    check("stop during countdown frees the recorder", !recorder::is_active() && !crate::app::is_busy(), "");
    // A stale countdown tick must not resurrect anything.
    hold();
    after(3.5, || {
        check("stale countdown callbacks are ignored", !recorder::is_active(), "");
        release();
    });
}

fn test_overlay_regressions() {
    overlay::set_test_hidden(true);
    let frozen = fake_frozen();
    let d0 = frozen.displays[0].info;
    let o = d0.frame.origin;

    // Window mode: a stray mouse-up (no press on the overlay) must not snap.
    let got: Rc<RefCell<Option<OverlayResult>>> = Rc::default();
    let g2 = got.clone();
    overlay::show(overlay::Request {
        purpose: Purpose::Snip,
        mode: SnipMode::Window,
        frozen,
        show_bar: true,
        done: Box::new(move |r| *g2.borrow_mut() = Some(r)),
    });
    let inside = CGPoint::new(o.x + 300.0, o.y + 300.0);
    overlay::simulate_up(0, inside);
    check("window mode ignores a stray mouse-up", got.borrow().is_none() && overlay::is_active(), "");
    overlay::simulate_down(0, inside);
    overlay::simulate_up(0, inside);
    check("window mode snaps on a real click", matches!(&*got.borrow(), Some(OverlayResult::Image { .. })), "");

    // Record session: the Rectangle button keeps it a recording.
    let got2: Rc<RefCell<Option<OverlayResult>>> = Rc::default();
    let g3 = got2.clone();
    overlay::show(overlay::Request {
        purpose: Purpose::Record,
        mode: SnipMode::Rectangle,
        frozen: fake_frozen(),
        show_bar: true,
        done: Box::new(move |r| *g3.borrow_mut() = Some(r)),
    });
    overlay::simulate_mode(SnipMode::Rectangle);
    check("rectangle button keeps a recording a recording", overlay::current_purpose() == Some(Purpose::Record), &format!("{:?}", overlay::current_purpose()));
    let app = NSApplication::sharedApplication(mtm());
    let overlay_windows: Vec<_> = app.windows().iter().filter(|w| w.className().to_string().contains("OverlayWindow")).collect();
    let still = !overlay_windows.is_empty() && overlay_windows.iter().all(|w| w.animationBehavior() == NSWindowAnimationBehavior::None);
    check("overlay windows appear without animation", still, &format!("{} windows", overlay_windows.len()));
    overlay::cancel();
    overlay::set_test_hidden(false);

    // Color picking on the very edge of a display.
    let fz = fake_frozen();
    let f = fz.displays[0].info.frame;
    let edge = CGPoint::new(f.origin.x + f.size.width - 0.25, f.origin.y + f.size.height - 0.25);
    check("color picker works at a display edge", fz.color_at(edge).is_some(), "");

    // Our own visible windows are snippable in Window mode.
    let ed = Editor::new();
    ed.window.setFrame_display(rect(200.0, 200.0, 900.0, 520.0), true);
    ed.window.orderFrontRegardless();
    let frame = ed.window.frame();
    hold();
    after(0.6, move || {
        let listed = capture::window_list().iter().any(|w| (w.frame.origin.x - frame.origin.x).abs() < 2.0 && (w.frame.size.width - frame.size.width).abs() < 2.0);
        check("own windows appear in window mode", listed, &format!("{frame:?}"));
        ed.window.close();
        release();
    });

    // Two snips in the same second get two files.
    let dir = out_dir().join("autosave2");
    let _ = std::fs::remove_dir_all(&dir);
    let d2 = dir.to_string_lossy().into_owned();
    crate::settings::update(|s| { s.save_dir = d2.clone(); s.auto_save = true; s.show_notification = false; });
    crate::app::deliver_snip(synthetic_screenshot(400, 300, 2.0), 2.0, crate::app::Origin::Menu);
    crate::app::deliver_snip(synthetic_screenshot(400, 300, 2.0), 2.0, crate::app::Origin::Menu);
    hold();
    after(1.5, move || {
        let n = std::fs::read_dir(&dir).map(|r| r.flatten().filter(|e| e.metadata().map(|m| m.len() > 1000).unwrap_or(false)).count()).unwrap_or(0);
        check("two snips in one second are both saved", n == 2, &format!("{n} files"));
        release();
    });
}

fn test_live_about() {
    let app = NSApplication::sharedApplication(mtm());
    let before: Vec<isize> = app.windows().iter().map(|w| w.windowNumber()).collect();
    crate::app::show_about();
    hold();
    after(0.8, move || {
        let app = NSApplication::sharedApplication(mtm());
        let about = app.windows().iter().find(|w| w.isVisible() && !before.contains(&w.windowNumber()));
        match about {
            Some(w) => {
                check("About window opens", true, "");
                capture_own_window(w.retain(), "45-live-about.png");
                let w2 = w.retain();
                after(1.0, move || w2.close());
            }
            None => check("About window opens", false, ""),
        }
        release();
    });
}
