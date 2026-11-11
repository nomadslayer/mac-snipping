//! Screen recording (ScreenCaptureKit + SCRecordingOutput → MP4).
//!
//! Pause/resume is implemented by recording segments (removing/adding the
//! recording output on a running stream) and joining them on stop.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, MainThreadOnly, Message};
use objc2_app_kit::*;
use objc2_av_foundation::*;
use objc2_core_foundation::CGRect;
use objc2_core_graphics::CGColor;
use objc2_core_media::{kCMTimeZero, CMTime, CMTimeRange};
use objc2_foundation::*;
use objc2_quartz_core::{CAShapeLayer, CATransaction};
use objc2_screen_capture_kit::*;

use crate::app::{self, Origin};
use crate::capture::ScreenInfo;
use crate::settings;
use crate::util::{
    self, after, file_url, fixed_size, hstack, icon_button, label, log, mtm, ns, on_main, rect,
    set_on, temp_dir, text_icon_button, timestamp_name, toggle_button, unique_path, SendBox,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready,
    Countdown,
    Starting,
    Recording,
    Paused,
    Stopping,
}

struct Ui {
    frame_window: Retained<NSWindow>,
    bar: Retained<NSPanel>,
    start: Retained<NSButton>,
    mic: Retained<NSButton>,
    audio: Retained<NSButton>,
    pause: Retained<NSButton>,
    stop: Retained<NSButton>,
    time: Retained<NSTextField>,
    count_label: Retained<NSTextField>,
}

struct Recorder {
    /// Unique per recording session; stale callbacks compare against it.
    id: u64,
    state: State,
    screen: ScreenInfo,
    rect: CGRect,
    origin: Origin,
    ui: Ui,
    stream: Option<Retained<SCStream>>,
    output: Option<Retained<SCRecordingOutput>>,
    delegate: Retained<RecDelegate>,
    segments: Vec<PathBuf>,
    pending_finishes: usize,
    stream_stopped: bool,
    elapsed: f64,
    started_at: Option<Instant>,
    discard: bool,
    failed: Option<String>,
}

thread_local! {
    static REC: RefCell<Option<Recorder>> = const { RefCell::new(None) };
    static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

fn current_id() -> Option<u64> {
    REC.with(|r| r.borrow().as_ref().map(|r| r.id))
}

/// Re-show the editor that was hidden for this recording (if any).
fn restore_origin(origin: &Origin) {
    if let Origin::Editor(w) = origin {
        if let Some(ed) = w.upgrade() {
            ed.show();
            return;
        }
    }
    app::window_closed();
}

pub fn is_active() -> bool {
    REC.with(|r| r.borrow().is_some())
}

pub fn is_recording() -> bool {
    REC.with(|r| {
        r.borrow()
            .as_ref()
            .map(|r| matches!(r.state, State::Recording | State::Paused | State::Starting))
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------
// Delegates (called on ScreenCaptureKit queues)
// ---------------------------------------------------------------------------

define_class!(
    #[unsafe(super = NSObject)]
    #[name = "SnippingRecordingDelegate"]
    pub struct RecDelegate;

    unsafe impl NSObjectProtocol for RecDelegate {}

    unsafe impl SCRecordingOutputDelegate for RecDelegate {
        #[unsafe(method(recordingOutputDidStartRecording:))]
        fn did_start(&self, _o: &SCRecordingOutput) {
            log("recording segment started");
        }

        #[unsafe(method(recordingOutput:didFailWithError:))]
        fn did_fail(&self, _o: &SCRecordingOutput, error: &NSError) {
            let msg = error.localizedDescription().to_string();
            log(&format!("recording failed: {msg}"));
            on_main(move || segment_failed(msg));
        }

        #[unsafe(method(recordingOutputDidFinishRecording:))]
        fn did_finish(&self, _o: &SCRecordingOutput) {
            on_main(segment_finished);
        }
    }

    unsafe impl SCStreamDelegate for RecDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _s: &SCStream, error: &NSError) {
            let msg = error.localizedDescription().to_string();
            log(&format!("stream stopped: {msg}"));
            on_main(move || {
                // e.g. the user stopped sharing from the menu bar.
                let active = REC.with(|r| r.borrow().as_ref().map(|r| r.state));
                if matches!(active, Some(State::Recording) | Some(State::Paused)) {
                    stop();
                }
            });
        }
    }
);

impl RecDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

fn make_frame_window(r: CGRect) -> Retained<NSWindow> {
    let m = mtm();
    let pad = 4.0;
    let f = rect(r.origin.x - pad, r.origin.y - pad, r.size.width + 2.0 * pad, r.size.height + 2.0 * pad);
    let w = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(m),
            f,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        w.setReleasedWhenClosed(false);
        w.setOpaque(false);
        w.setBackgroundColor(Some(&NSColor::clearColor()));
        w.setIgnoresMouseEvents(true);
        w.setHasShadow(false);
        w.setAnimationBehavior(NSWindowAnimationBehavior::None);
        w.setLevel(NSStatusWindowLevel);
        w.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
    }
    let v = NSView::initWithFrame(NSView::alloc(m), rect(0.0, 0.0, f.size.width, f.size.height));
    v.setWantsLayer(true);
    let layer = v.layer().unwrap();
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    for (color, phase) in [(CGColor::new_srgb(1.0, 1.0, 1.0, 0.9), 0.0), (CGColor::new_srgb(0.91, 0.07, 0.14, 1.0), 6.0)] {
        let s = CAShapeLayer::layer();
        s.setFrame(rect(0.0, 0.0, f.size.width, f.size.height));
        let path = objc2_core_graphics::CGMutablePath::new();
        unsafe {
            objc2_core_graphics::CGMutablePath::add_rect(
                Some(&path),
                std::ptr::null(),
                rect(pad - 1.0, pad - 1.0, r.size.width + 2.0, r.size.height + 2.0),
            )
        };
        s.setPath(Some(&path));
        s.setFillColor(None);
        s.setStrokeColor(Some(&color));
        s.setLineWidth(2.0);
        s.setLineDashPattern(Some(&NSArray::from_retained_slice(&[NSNumber::new_f64(6.0), NSNumber::new_f64(6.0)])));
        s.setLineDashPhase(phase);
        layer.addSublayer(&s);
    }
    CATransaction::commit();
    w.setContentView(Some(&v));
    w
}

fn build_ui(r: CGRect, screen: ScreenInfo) -> Ui {
    let m = mtm();
    let s = settings::get();
    let frame_window = make_frame_window(r);

    let start = text_icon_button("Start", "record.circle.fill", start_clicked);
    start.setImage(Some(&util::symbol_colored("record.circle.fill", 14.0, &NSColor::systemRedColor())));
    start.setBezelStyle(NSBezelStyle::Push);
    let mic = toggle_button("mic.fill", "Microphone", || {
        let on = REC.with(|r| r.borrow().as_ref().map(|r| util::is_on(&r.ui.mic))).unwrap_or(false);
        settings::update(|s| s.record_mic = on);
    });
    set_on(&mic, s.record_mic);
    let audio = toggle_button("speaker.wave.2.fill", "System audio", || {
        let on = REC.with(|r| r.borrow().as_ref().map(|r| util::is_on(&r.ui.audio))).unwrap_or(false);
        settings::update(|s| s.record_system_audio = on);
    });
    set_on(&audio, s.record_system_audio);
    let pause = icon_button("pause.fill", "Pause", toggle_pause);
    let stop_b = icon_button("stop.fill", "Stop", stop);
    stop_b.setImage(Some(&util::symbol_colored("stop.fill", 15.0, &NSColor::systemRedColor())));
    let time = label("00:00", 13.0, true);
    time.setFont(Some(&NSFont::monospacedDigitSystemFontOfSize_weight(13.0, unsafe { NSFontWeightSemibold })));
    let discard_b = icon_button("trash", "Discard", discard);
    for b in [&mic, &audio, &pause, &stop_b, &discard_b] {
        fixed_size(b, 34.0, 28.0);
    }
    pause.setHidden(true);
    stop_b.setHidden(true);
    time.setHidden(true);

    let stack = hstack(&[&start, &mic, &audio, &time, &pause, &stop_b, &discard_b], 6.0);
    stack.setEdgeInsets(NSEdgeInsets { top: 8.0, left: 12.0, bottom: 8.0, right: 12.0 });
    let size = NSSize::new(380.0, 46.0);

    // Place the bar above the region (or inside its top edge if no room).
    let sf = screen.frame;
    let mut x = r.origin.x + r.size.width / 2.0 - size.width / 2.0;
    let mut y = r.origin.y + r.size.height + 12.0;
    if y + size.height > sf.origin.y + sf.size.height - 30.0 {
        y = r.origin.y + r.size.height - size.height - 12.0;
    }
    x = x.clamp(sf.origin.x + 8.0, sf.origin.x + sf.size.width - size.width - 8.0);
    y = y.clamp(sf.origin.y + 8.0, sf.origin.y + sf.size.height - size.height - 8.0);

    let bar = unsafe {
        NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(m),
            rect(x, y, size.width, size.height),
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        bar.setReleasedWhenClosed(false);
        bar.setLevel(NSStatusWindowLevel + 1);
        bar.setOpaque(false);
        bar.setBackgroundColor(Some(&NSColor::clearColor()));
        bar.setHasShadow(true);
        bar.setMovableByWindowBackground(true);
        bar.setHidesOnDeactivate(false);
        bar.setAnimationBehavior(NSWindowAnimationBehavior::None);
        bar.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
    }
    let bg = util::hud_container(rect(0.0, 0.0, size.width, size.height), 12.0);
    bg.addSubview(&stack);
    stack.setFrame(rect(0.0, 0.0, size.width, size.height));
    bar.setContentView(Some(&bg));

    let count_label = label("3", 96.0, true);
    count_label.setTextColor(Some(&NSColor::whiteColor()));
    count_label.setAlignment(NSTextAlignment::Center);
    count_label.setHidden(true);
    if let Some(v) = frame_window.contentView() {
        let b = v.bounds();
        count_label.setFrame(rect(0.0, b.size.height / 2.0 - 60.0, b.size.width, 120.0));
        count_label.setWantsLayer(true);
        if let Some(l) = count_label.layer() {
            l.setShadowColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
            l.setShadowOpacity(0.6);
            l.setShadowRadius(8.0);
        }
        v.addSubview(&count_label);
    }

    Ui { frame_window, bar, start, mic, audio, pause, stop: stop_b, time, count_label }
}

fn fit_bar(ui: &Ui) {
    if let Some(content) = ui.bar.contentView() {
        if let Some(stack) = content.subviews().iter().next() {
            let size = stack.fittingSize();
            let f = ui.bar.frame();
            let cx = f.origin.x + f.size.width / 2.0;
            let nf = rect(cx - size.width / 2.0, f.origin.y, size.width, size.height.max(44.0));
            ui.bar.setFrame_display(nf, true);
            content.setFrame(rect(0.0, 0.0, nf.size.width, nf.size.height));
            stack.setFrame(rect(0.0, 0.0, nf.size.width, nf.size.height));
        }
    }
}

/// Show the region frame and the recording toolbar; recording starts when
/// the user presses Start (Windows behavior).
pub fn prepare(screen: ScreenInfo, r: CGRect, origin: Origin) {
    if is_active() {
        return;
    }
    // Snap to whole points; keep the region on its display.
    let r = crate::util::rect_integral(crate::util::rect_intersection(r, screen.frame).unwrap_or(screen.frame));
    let ui = build_ui(r, screen);
    ui.frame_window.orderFrontRegardless();
    ui.bar.orderFrontRegardless();
    fit_bar(&ui);
    REC.with(|rec| {
        let id = NEXT_ID.with(|n| {
            let v = n.get();
            n.set(v + 1);
            v
        });
        *rec.borrow_mut() = Some(Recorder {
            id,
            state: State::Ready,
            screen,
            rect: r,
            origin,
            ui,
            stream: None,
            output: None,
            delegate: RecDelegate::new(),
            segments: Vec::new(),
            pending_finishes: 0,
            stream_stopped: false,
            elapsed: 0.0,
            started_at: None,
            discard: false,
            failed: None,
        })
    });
}

/// Self-test hook: press Start.
pub(crate) fn press_start() {
    start_clicked();
}

fn start_clicked() {
    let mic = REC.with(|r| {
        let mut g = r.borrow_mut();
        let Some(rec) = g.as_mut() else { return None };
        if rec.state != State::Ready {
            return None;
        }
        rec.state = State::Countdown;
        rec.ui.start.setHidden(true);
        rec.ui.mic.setEnabled(false);
        rec.ui.audio.setEnabled(false);
        Some(util::is_on(&rec.ui.mic))
    });
    let Some(mic) = mic else { return };
    let Some(id) = current_id() else { return };
    let begin = move || countdown(id, 3);
    if mic {
        // Ask for microphone access up-front.
        unsafe {
            if let Some(media) = AVMediaTypeAudio {
                let block = RcBlock::new(move |granted: Bool| {
                    let granted = granted.as_bool();
                    on_main(move || {
                        if current_id() != Some(id) {
                            return;
                        }
                        if !granted {
                            REC.with(|r| {
                                if let Some(rec) = r.borrow().as_ref() {
                                    set_on(&rec.ui.mic, false);
                                }
                            });
                        }
                        countdown(id, 3);
                    });
                });
                AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block);
                return;
            }
        }
    }
    begin();
}

fn countdown(id: u64, n: u32) {
    let alive = REC.with(|r| {
        let g = r.borrow();
        let Some(rec) = g.as_ref() else { return false };
        if rec.id != id || rec.state != State::Countdown {
            return false;
        }
        if n == 0 {
            rec.ui.count_label.setHidden(true);
        } else {
            rec.ui.count_label.setStringValue(&ns(&n.to_string()));
            rec.ui.count_label.setHidden(false);
        }
        true
    });
    if !alive {
        return;
    }
    if n == 0 {
        start_stream();
    } else {
        after(1.0, move || countdown(id, n - 1));
    }
}

fn even(v: f64) -> usize {
    let n = v.round() as usize;
    (n + 1) & !1
}

fn start_stream() {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.state = State::Starting;
        }
    });
    let block = RcBlock::new(|content: *mut SCShareableContent, err: *mut NSError| {
        let content = unsafe { content.as_ref() }.map(|c| c.retain());
        let err = if err.is_null() { None } else { Some(unsafe { (*err).localizedDescription().to_string() }) };
        let content = SendBox(content);
        on_main(move || match content.into_inner() {
            Some(c) => build_stream(&c),
            None => fail(&err.unwrap_or_else(|| "Screen content unavailable".into())),
        });
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
}

fn new_output(delegate: &RecDelegate, path: &std::path::Path) -> Retained<SCRecordingOutput> {
    unsafe {
        let cfg = SCRecordingOutputConfiguration::new();
        cfg.setOutputURL(&file_url(path));
        if let Some(t) = AVFileTypeMPEG4 {
            cfg.setOutputFileType(t);
        }
        if let Some(c) = AVVideoCodecTypeH264 {
            cfg.setVideoCodecType(c);
        }
        SCRecordingOutput::initWithConfiguration_delegate(
            SCRecordingOutput::alloc(),
            &cfg,
            ProtocolObject::from_ref(delegate),
        )
    }
}

fn build_stream(content: &SCShareableContent) {
    let result: Result<(), String> = REC.with(|r| {
        let mut g = r.borrow_mut();
        let Some(rec) = g.as_mut() else { return Ok(()) };
        if rec.state != State::Starting {
            return Ok(());
        }
        unsafe {
            let displays = content.displays();
            let display = displays
                .iter()
                .find(|d| d.displayID() == rec.screen.display_id)
                .ok_or("The display is no longer available")?;
            let my_pid = std::process::id() as i32;
            let mine: Vec<Retained<SCRunningApplication>> = content
                .applications()
                .iter()
                .filter(|a| a.processID() == my_pid)
                .collect();
            let filter = SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::from_retained_slice(&mine),
                &NSArray::new(),
            );
            let s = settings::get();
            let sf = rec.screen.frame;
            let local = rect(
                rec.rect.origin.x - sf.origin.x,
                sf.size.height - (rec.rect.origin.y - sf.origin.y) - rec.rect.size.height,
                rec.rect.size.width,
                rec.rect.size.height,
            );
            let cfg = SCStreamConfiguration::new();
            cfg.setSourceRect(local);
            cfg.setWidth(even(local.size.width * rec.screen.scale));
            cfg.setHeight(even(local.size.height * rec.screen.scale));
            cfg.setMinimumFrameInterval(CMTime::new(1, s.record_fps.clamp(10, 60) as i32));
            cfg.setShowsCursor(true);
            cfg.setQueueDepth(8);
            let sys_audio = util::is_on(&rec.ui.audio);
            let mic = util::is_on(&rec.ui.mic);
            cfg.setCapturesAudio(sys_audio);
            cfg.setExcludesCurrentProcessAudio(true);
            cfg.setSampleRate(48_000);
            cfg.setChannelCount(2);
            cfg.setCaptureMicrophone(mic);

            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &cfg,
                Some(ProtocolObject::from_ref(&*rec.delegate)),
            );
            let seg = unique_path(&temp_dir(), &format!("segment-{}", rec.segments.len() + 1), "mp4");
            let out = new_output(&rec.delegate, &seg);
            stream.addRecordingOutput_error(&out).map_err(|e| e.localizedDescription().to_string())?;
            rec.segments.push(seg);
            rec.output = Some(out);
            let block = RcBlock::new(|err: *mut NSError| {
                let err = if err.is_null() { None } else { Some(unsafe { (*err).localizedDescription().to_string() }) };
                on_main(move || match err {
                    Some(e) => fail(&e),
                    None => started(),
                });
            });
            stream.startCaptureWithCompletionHandler(Some(&block));
            rec.stream = Some(stream);
        }
        Ok(())
    });
    if let Err(e) = result {
        fail(&e);
    }
}

fn started() {
    let ok = REC.with(|r| {
        let mut g = r.borrow_mut();
        let Some(rec) = g.as_mut() else { return false };
        // Stop/Discard may have been pressed while the stream was starting.
        if rec.state != State::Starting {
            return false;
        }
        rec.state = State::Recording;
        rec.started_at = Some(Instant::now());
        rec.ui.pause.setHidden(false);
        rec.ui.stop.setHidden(false);
        rec.ui.time.setHidden(false);
        rec.ui.mic.setHidden(true);
        rec.ui.audio.setHidden(true);
        fit_bar(&rec.ui);
        true
    });
    if ok {
        app::set_status_recording(true);
        tick();
    }
}

fn elapsed(rec: &Recorder) -> f64 {
    rec.elapsed + rec.started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0)
}

fn tick() {
    let again = REC.with(|r| {
        let g = r.borrow();
        let Some(rec) = g.as_ref() else { return false };
        if !matches!(rec.state, State::Recording | State::Paused) {
            return false;
        }
        let t = elapsed(rec) as u64;
        let text = if t >= 3600 {
            format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
        } else {
            format!("{:02}:{:02}", t / 60, t % 60)
        };
        rec.ui.time.setStringValue(&ns(&text));
        let color = if rec.state == State::Paused { NSColor::secondaryLabelColor() } else { NSColor::labelColor() };
        rec.ui.time.setTextColor(Some(&color));
        true
    });
    if again {
        after(0.25, tick);
    }
}

fn toggle_pause() {
    REC.with(|r| {
        let mut g = r.borrow_mut();
        let Some(rec) = g.as_mut() else { return };
        let Some(stream) = rec.stream.clone() else { return };
        match rec.state {
            State::Recording => {
                if let Some(out) = rec.output.take() {
                    unsafe {
                        if stream.removeRecordingOutput_error(&out).is_ok() {
                            rec.pending_finishes += 1;
                        }
                    }
                }
                rec.elapsed = elapsed(rec);
                rec.started_at = None;
                rec.state = State::Paused;
                rec.ui.pause.setImage(Some(&util::symbol("play.fill", 15.0)));
                rec.ui.pause.setToolTip(Some(&ns("Resume")));
            }
            State::Paused => {
                let seg = unique_path(&temp_dir(), &format!("segment-{}", rec.segments.len() + 1), "mp4");
                let out = new_output(&rec.delegate, &seg);
                match unsafe { stream.addRecordingOutput_error(&out) } {
                    Ok(()) => {
                        rec.segments.push(seg);
                        rec.output = Some(out);
                        rec.started_at = Some(Instant::now());
                        rec.state = State::Recording;
                        rec.ui.pause.setImage(Some(&util::symbol("pause.fill", 15.0)));
                        rec.ui.pause.setToolTip(Some(&ns("Pause")));
                    }
                    Err(e) => log(&format!("resume failed: {}", e.localizedDescription())),
                }
            }
            _ => {}
        }
    });
}

pub fn stop() {
    end(false);
}

pub fn discard() {
    end(true);
}

fn end(discard: bool) {
    let stream = REC.with(|r| {
        let mut g = r.borrow_mut();
        let rec = g.as_mut()?;
        rec.discard = discard;
        match rec.state {
            State::Ready | State::Countdown => {
                rec.state = State::Stopping;
                rec.stream_stopped = true;
                rec.discard = true; // nothing was recorded yet
                return None;
            }
            State::Starting if rec.stream.is_none() => {
                // Still waiting for screen content: nothing recorded, and
                // build_stream will see we're no longer Starting.
                rec.state = State::Stopping;
                rec.stream_stopped = true;
                rec.discard = true;
                return None;
            }
            State::Stopping => return None,
            _ => {}
        }
        rec.state = State::Stopping;
        rec.elapsed = elapsed(rec);
        rec.started_at = None;
        if let (Some(stream), Some(out)) = (rec.stream.clone(), rec.output.take()) {
            unsafe {
                if stream.removeRecordingOutput_error(&out).is_ok() {
                    rec.pending_finishes += 1;
                }
            }
        }
        rec.ui.bar.orderOut(None);
        rec.ui.frame_window.orderOut(None);
        rec.stream.clone()
    });
    let id = current_id();
    match stream {
        Some(stream) => {
            let block = RcBlock::new(move |_err: *mut NSError| {
                on_main(move || {
                    REC.with(|r| {
                        if let Some(rec) = r.borrow_mut().as_mut().filter(|r| Some(r.id) == id) {
                            rec.stream_stopped = true;
                        }
                    });
                    maybe_finalize();
                });
            });
            unsafe { stream.stopCaptureWithCompletionHandler(Some(&block)) };
            // Safety net in case a callback never arrives (this session only).
            after(10.0, move || {
                REC.with(|r| {
                    if let Some(rec) = r.borrow_mut().as_mut().filter(|r| Some(r.id) == id) {
                        rec.stream_stopped = true;
                        rec.pending_finishes = 0;
                    }
                });
                maybe_finalize();
            });
        }
        None => maybe_finalize(),
    }
}

fn segment_finished() {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.pending_finishes = rec.pending_finishes.saturating_sub(1);
        }
    });
    maybe_finalize();
}

fn segment_failed(msg: String) {
    REC.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            rec.failed = Some(msg.clone());
        }
    });
    let state = REC.with(|r| r.borrow().as_ref().map(|r| r.state));
    match state {
        Some(State::Stopping) => segment_finished(),
        // Keep the parts that did record instead of throwing everything away.
        Some(State::Recording) | Some(State::Paused) => {
            segment_finished();
            stop();
        }
        Some(_) => fail(&msg),
        None => {}
    }
}

fn fail(msg: &str) {
    let state = REC.with(|r| r.borrow().as_ref().map(|r| r.state));
    match state {
        None => return,
        // The user already stopped; the stop path finishes up.
        Some(State::Stopping) => return,
        _ => {}
    }
    let rec = teardown(true);
    app::set_status_recording(false);
    util::alert("Recording failed", msg);
    if let Some(rec) = rec {
        restore_origin(&rec.origin);
    }
}

/// Close windows, stop the stream, optionally delete segments.
fn teardown(delete: bool) -> Option<Recorder> {
    let rec = REC.with(|r| r.borrow_mut().take())?;
    rec.ui.bar.orderOut(None);
    rec.ui.frame_window.orderOut(None);
    if let Some(stream) = &rec.stream {
        if matches!(rec.state, State::Recording | State::Paused | State::Starting) {
            unsafe { stream.stopCaptureWithCompletionHandler(None) };
        }
    }
    if delete {
        for s in &rec.segments {
            let _ = std::fs::remove_file(s);
        }
    }
    Some(rec)
}

fn maybe_finalize() {
    let ready = REC.with(|r| {
        r.borrow()
            .as_ref()
            .map(|r| r.state == State::Stopping && r.stream_stopped && r.pending_finishes == 0)
            .unwrap_or(false)
    });
    if !ready {
        return;
    }
    let discard = REC.with(|r| r.borrow().as_ref().map(|r| r.discard).unwrap_or(true));
    let Some(rec) = teardown(discard) else { return };
    app::set_status_recording(false);
    if discard {
        restore_origin(&rec.origin);
        return;
    }
    let segments: Vec<PathBuf> = rec
        .segments
        .iter()
        .filter(|p| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false))
        .cloned()
        .collect();
    if segments.is_empty() {
        util::alert(
            "Recording failed",
            rec.failed.as_deref().unwrap_or("No video was recorded."),
        );
        restore_origin(&rec.origin);
        return;
    }
    let out = unique_path(&temp_dir(), &timestamp_name("Screen Recording"), "mp4");
    let duration = rec.elapsed;
    let parts = segments.clone();
    join_segments(segments, out, move |res| match res {
        Ok(path) => finished(path, duration),
        Err(e) => save_parts(&parts, &e),
    });
}

/// Joining failed: don't lose footage — move each part to the recordings folder.
fn save_parts(parts: &[PathBuf], err: &str) {
    let dir = settings::recordings_dir();
    let _ = std::fs::create_dir_all(&dir);
    let stem = timestamp_name("Screen Recording");
    let mut saved = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        let dest = unique_path(&dir, &format!("{stem} (part {})", i + 1), "mp4");
        if std::fs::rename(p, &dest).is_ok() || std::fs::copy(p, &dest).is_ok() {
            saved.push(dest);
        }
    }
    util::alert(
        "Couldn't join the recording",
        &format!("{err}\n\nThe {} recorded part(s) were saved to {}.", saved.len(), dir.display()),
    );
    if let Some(first) = saved.first() {
        crate::export::reveal_in_finder(first);
    }
}

fn finished(path: PathBuf, _duration: f64) {
    let s = settings::get();
    let mut saved = None;
    if s.auto_save_recordings {
        let dir = settings::recordings_dir();
        let _ = std::fs::create_dir_all(&dir);
        let dest = unique_path(&dir, &timestamp_name("Screen Recording"), "mp4");
        if std::fs::copy(&path, &dest).is_ok() {
            saved = Some(dest);
        }
    }
    crate::video::open(path, saved);
}

/// Concatenate MP4 segments (passthrough, no re-encode).
fn join_segments(segments: Vec<PathBuf>, out: PathBuf, done: impl FnOnce(Result<PathBuf, String>) + 'static) {
    if segments.len() == 1 {
        let res = std::fs::rename(&segments[0], &out)
            .or_else(|_| std::fs::copy(&segments[0], &out).map(|_| ()))
            .map(|_| out.clone())
            .map_err(|e| e.to_string());
        done(res);
        return;
    }
    unsafe {
        let comp = AVMutableComposition::composition();
        let mut cursor = kCMTimeZero;
        for seg in &segments {
            let asset = AVURLAsset::URLAssetWithURL_options(&file_url(seg), None);
            let dur = asset.duration();
            let range = CMTimeRange { start: kCMTimeZero, duration: dur };
            if let Err(e) = comp.insertTimeRange_ofAsset_atTime_error(range, &asset, cursor) {
                done(Err(e.localizedDescription().to_string()));
                return;
            }
            cursor = cursor.add(dur);
        }
        let Some(export) = AVAssetExportSession::initWithAsset_presetName(
            AVAssetExportSession::alloc(),
            &comp,
            AVAssetExportPresetPassthrough,
        ) else {
            done(Err("Could not create export session".into()));
            return;
        };
        export.setOutputURL(Some(&file_url(&out)));
        export.setOutputFileType(AVFileTypeMPEG4);
        let done = std::cell::RefCell::new(Some(SendBox(Box::new(done) as Box<dyn FnOnce(Result<PathBuf, String>)>)));
        let exp = export.clone();
        let segs = segments.clone();
        let block = RcBlock::new(move || {
            let status = exp.status();
            let err = exp.error().map(|e| e.localizedDescription().to_string());
            let out = out.clone();
            let segs = segs.clone();
            if let Some(cb) = done.borrow_mut().take() {
                on_main(move || {
                    let f = cb.into_inner();
                    if status == AVAssetExportSessionStatus::Completed {
                        for s in &segs {
                            let _ = std::fs::remove_file(s);
                        }
                        f(Ok(out));
                    } else {
                        let _ = std::fs::remove_file(&out);
                        f(Err(err.unwrap_or_else(|| "Export failed".into())));
                    }
                });
            }
        });
        export.exportAsynchronouslyWithCompletionHandler(&block);
    }
}

