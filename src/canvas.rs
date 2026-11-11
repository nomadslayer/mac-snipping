//! The editing canvas: displays the snip and handles every markup tool.
//!
//! The view is flipped (top-left origin) and sized in points; the model is in
//! image pixels. `scale` converts between the two (pixels per point).

use std::cell::RefCell;
use std::f64::consts::PI;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadOnly, Message};
use objc2_app_kit::*;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::*;
use objc2_foundation::*;

use crate::annot::{dist, draw_image_flipped, draw_item, draw_items, pt, Item, ShapeKind};
use crate::capture::{bitmap_context, image_size};
use crate::ocr::{OcrLine, Sensitive};
use crate::settings;
use crate::util::{mtm, ns, rect, rect_contains, rect_from_points, rect_integral, Rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    None,
    Pen,
    Highlighter,
    Eraser,
    Shape,
    Crop,
    Text,
}

#[derive(Clone, Debug)]
pub struct Style {
    pub pen_color: Rgba,
    pub pen_width: f64,
    pub hl_color: Rgba,
    pub hl_width: f64,
    pub shape_kind: ShapeKind,
    pub shape_stroke: Rgba,
    pub shape_fill: Option<Rgba>,
    pub shape_width: f64,
}

impl Style {
    pub fn from_settings() -> Style {
        let s = settings::get();
        Style {
            pen_color: s.pen_color,
            pen_width: s.pen_width,
            hl_color: s.highlighter_color,
            hl_width: s.highlighter_width,
            shape_kind: ShapeKind::from_index(s.shape_kind),
            shape_stroke: s.shape_stroke,
            shape_fill: s.shape_fill,
            shape_width: s.shape_width,
        }
    }
    pub fn persist(&self) {
        let st = self.clone();
        settings::update(move |s| {
            s.pen_color = st.pen_color;
            s.pen_width = st.pen_width;
            s.highlighter_color = st.hl_color;
            s.highlighter_width = st.hl_width;
            s.shape_kind = st.shape_kind.index();
            s.shape_stroke = st.shape_stroke;
            s.shape_fill = st.shape_fill;
            s.shape_width = st.shape_width;
        });
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Ruler {
    pub c: CGPoint,
    pub angle: f64,
    pub len: f64,
    pub thick: f64,
}

impl Ruler {
    fn to_local(&self, p: CGPoint) -> CGPoint {
        let (s, c) = self.angle.sin_cos();
        let dx = p.x - self.c.x;
        let dy = p.y - self.c.y;
        pt(dx * c + dy * s, -dx * s + dy * c)
    }
    fn from_local(&self, l: CGPoint) -> CGPoint {
        let (s, c) = self.angle.sin_cos();
        pt(self.c.x + l.x * c - l.y * s, self.c.y + l.x * s + l.y * c)
    }
    fn contains(&self, p: CGPoint) -> bool {
        let l = self.to_local(p);
        l.x.abs() <= self.len / 2.0 && l.y.abs() <= self.thick / 2.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Protractor {
    pub c: CGPoint,
    pub r: f64,
}

pub struct OcrOverlay {
    pub lines: Vec<OcrLine>,
    pub selected: Vec<bool>,
}

#[derive(Clone)]
struct Snapshot {
    base: CFRetained<CGImage>,
    items: Vec<Item>,
}

#[derive(Clone, Copy)]
enum Snap {
    Line { a: CGPoint, dir: CGPoint },
    Arc { c: CGPoint, r: f64 },
}

impl Snap {
    fn apply(&self, p: CGPoint) -> CGPoint {
        match *self {
            Snap::Line { a, dir } => {
                let t = (p.x - a.x) * dir.x + (p.y - a.y) * dir.y;
                pt(a.x + dir.x * t, a.y + dir.y * t)
            }
            Snap::Arc { c, r } => {
                let d = dist(p, c).max(1e-6);
                pt(c.x + (p.x - c.x) / d * r, c.y + (p.y - c.y) / d * r)
            }
        }
    }
}

enum Gesture {
    Ink { item: Item, snap: Option<Snap> },
    Erase,
    NewShape { start: CGPoint },
    MoveShape { idx: usize, last: CGPoint, moved: bool },
    ResizeShape { idx: usize, handle: usize, moved: bool },
    MoveRuler { last: CGPoint },
    MoveProtractor { last: CGPoint },
    Crop { handle: usize, start_rect: CGRect, start: CGPoint },
    TextSelect { start: CGPoint },
    Pan { last: NSPoint },
}

pub struct CanvasState {
    pub base: Option<CFRetained<CGImage>>,
    pub scale: f64,
    pub items: Vec<Item>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    pending: Option<Snapshot>,
    pub tool: Tool,
    pub style: Style,
    gesture: Option<Gesture>,
    pub selected: Option<usize>,
    pub ruler: Option<Ruler>,
    pub protractor: Option<Protractor>,
    pub crop: Option<CGRect>,
    pub ocr: Option<OcrOverlay>,
    pub dirty: bool,
    hover: Option<CGPoint>,
    /// (shape index, time) of the last style edit, for undo coalescing.
    last_style_edit: Option<(usize, std::time::Instant)>,
}

impl CanvasState {
    fn new() -> Self {
        Self {
            base: None,
            scale: 1.0,
            items: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            pending: None,
            tool: Tool::None,
            style: Style::from_settings(),
            gesture: None,
            selected: None,
            ruler: None,
            protractor: None,
            crop: None,
            ocr: None,
            dirty: false,
            hover: None,
            last_style_edit: None,
        }
    }

    pub fn size_px(&self) -> (f64, f64) {
        self.base
            .as_ref()
            .map(|b| {
                let (w, h) = image_size(b);
                (w as f64, h as f64)
            })
            .unwrap_or((0.0, 0.0))
    }

    fn snapshot(&self) -> Option<Snapshot> {
        Some(Snapshot { base: self.base.clone()?, items: self.items.clone() })
    }

    fn begin_change(&mut self) {
        self.pending = self.snapshot();
        self.last_style_edit = None;
    }

    fn commit_change(&mut self) {
        if let Some(s) = self.pending.take() {
            self.undo.push(s);
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
            self.redo.clear();
            self.dirty = true;
        }
    }

    /// Record an undo point immediately (for one-shot edits).
    fn checkpoint(&mut self) {
        self.begin_change();
        self.commit_change();
    }
}

pub enum CanvasKey {
    Escape,
    Return,
}

#[derive(Default)]
pub struct Hooks {
    pub changed: Option<Box<dyn Fn()>>,
    pub key: Option<Box<dyn Fn(CanvasKey)>>,
}

pub struct CanvasIvars {
    pub state: RefCell<CanvasState>,
    pub hooks: RefCell<Hooks>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
}

define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[name = "SnippingCanvasView"]
    #[ivars = CanvasIvars]
    pub struct CanvasView;

    impl CanvasView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.on_mouse_down(event);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.on_mouse_dragged(event);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.on_mouse_up(event);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let p = self.event_px(event);
            let tool = {
                let mut st = self.ivars().state.borrow_mut();
                st.hover = Some(p);
                st.tool
            };
            if tool == Tool::Eraser {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().state.borrow_mut().hover = None;
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            if !self.on_scroll(event) {
                unsafe {
                    let _: () = msg_send![super(self), scrollWheel: event];
                }
            }
        }

        #[unsafe(method(rotateWithEvent:))]
        fn rotate_with_event(&self, event: &NSEvent) {
            let rot = event.rotation() as f64;
            let changed = {
                let mut st = self.ivars().state.borrow_mut();
                if let Some(r) = st.ruler.as_mut() {
                    r.angle -= rot.to_radians();
                    true
                } else {
                    false
                }
            };
            if changed {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.on_key(event) {
                unsafe {
                    let _: () = msg_send![super(self), keyDown: event];
                }
            }
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let tool = self.ivars().state.borrow().tool;
            let cursor = match tool {
                Tool::Pen | Tool::Highlighter | Tool::Shape | Tool::Eraser => NSCursor::crosshairCursor(),
                Tool::Text => NSCursor::IBeamCursor(),
                Tool::None => NSCursor::openHandCursor(),
                Tool::Crop => NSCursor::arrowCursor(),
            };
            self.addCursorRect_cursor(self.visibleRect(), &cursor);
        }

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            if let Some(old) = self.ivars().tracking.borrow_mut().take() {
                self.removeTrackingArea(&old);
            }
            let area = unsafe {
                NSTrackingArea::initWithRect_options_owner_userInfo(
                    NSTrackingArea::alloc(),
                    NSRect::ZERO,
                    NSTrackingAreaOptions::MouseMoved
                        | NSTrackingAreaOptions::MouseEnteredAndExited
                        | NSTrackingAreaOptions::ActiveInKeyWindow
                        | NSTrackingAreaOptions::InVisibleRect,
                    Some(self as &AnyObject),
                    None,
                )
            };
            self.addTrackingArea(&area);
            *self.ivars().tracking.borrow_mut() = Some(area);
            unsafe {
                let _: () = msg_send![super(self), updateTrackingAreas];
            }
        }
    }
);

const HANDLE_PT: f64 = 6.0;

impl CanvasView {
    pub fn new() -> Retained<Self> {
        let this = Self::alloc(mtm()).set_ivars(CanvasIvars {
            state: RefCell::new(CanvasState::new()),
            hooks: RefCell::new(Hooks::default()),
            tracking: RefCell::new(None),
        });
        let view: Retained<Self> =
            unsafe { msg_send![super(this), initWithFrame: rect(0.0, 0.0, 100.0, 100.0)] };
        view
    }

    pub fn st(&self) -> std::cell::Ref<'_, CanvasState> {
        self.ivars().state.borrow()
    }

    fn st_mut(&self) -> std::cell::RefMut<'_, CanvasState> {
        self.ivars().state.borrow_mut()
    }

    fn notify(&self) {
        let hooks = self.ivars().hooks.borrow();
        if let Some(f) = hooks.changed.as_ref() {
            f();
        }
    }

    fn mag(&self) -> f64 {
        self.enclosingScrollView().map(|s| s.magnification()).unwrap_or(1.0).max(0.01)
    }

    /// Pixels that correspond to `points` on screen at the current zoom.
    fn screen_px(&self, points: f64) -> f64 {
        points * self.st().scale / self.mag()
    }

    fn event_px(&self, event: &NSEvent) -> CGPoint {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        let s = self.st().scale;
        pt(p.x * s, p.y * s)
    }

    fn invalidate_px(&self, r: CGRect) {
        let s = self.st().scale;
        let pad = 4.0;
        self.setNeedsDisplayInRect(rect(
            r.origin.x / s - pad,
            r.origin.y / s - pad,
            r.size.width / s + 2.0 * pad,
            r.size.height / s + 2.0 * pad,
        ));
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    pub fn set_image(&self, image: CFRetained<CGImage>, scale: f64) {
        {
            let mut st = self.st_mut();
            let style = st.style.clone();
            let tool = st.tool;
            *st = CanvasState::new();
            st.style = style;
            st.tool = if matches!(tool, Tool::Crop | Tool::Text) { Tool::None } else { tool };
            st.base = Some(image);
            st.scale = scale.max(0.1);
        }
        self.resize_to_image();
        self.notify();
    }

    pub fn has_image(&self) -> bool {
        self.st().base.is_some()
    }

    fn resize_to_image(&self) {
        let (w, h, s) = {
            let st = self.st();
            let (w, h) = st.size_px();
            (w, h, st.scale)
        };
        self.setFrameSize(NSSize::new((w / s).max(1.0), (h / s).max(1.0)));
        self.setNeedsDisplay(true);
    }

    pub fn image_size_pt(&self) -> NSSize {
        let st = self.st();
        let (w, h) = st.size_px();
        NSSize::new(w / st.scale, h / st.scale)
    }

    pub fn tool(&self) -> Tool {
        self.st().tool
    }

    pub fn set_tool(&self, tool: Tool) {
        {
            let mut st = self.st_mut();
            st.tool = tool;
            if tool != Tool::Shape {
                st.selected = None;
            }
        }
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.setNeedsDisplay(true);
    }

    pub fn style(&self) -> Style {
        self.st().style.clone()
    }

    /// Update the tool style; also restyles the selected shape (like Windows).
    /// Only the attributes that actually changed are applied to the shape, and
    /// rapid changes (slider drags, color-panel ticks) share one undo step.
    pub fn update_style(&self, f: impl FnOnce(&mut Style)) {
        let changed_shape = {
            let mut st = self.st_mut();
            let before = st.style.clone();
            f(&mut st.style);
            st.style.persist();
            let after = st.style.clone();
            let scale = st.scale;
            match st.selected {
                Some(idx) if st.gesture.is_none() && matches!(st.items.get(idx), Some(Item::Shape { .. })) => {
                    let mut item = st.items[idx].clone();
                    if let Item::Shape { kind, stroke, fill, width, .. } = &mut item {
                        if after.shape_kind != before.shape_kind {
                            *kind = after.shape_kind;
                        }
                        if after.shape_stroke != before.shape_stroke {
                            *stroke = after.shape_stroke;
                        }
                        if after.shape_fill != before.shape_fill {
                            *fill = after.shape_fill;
                        }
                        if after.shape_width != before.shape_width {
                            *width = after.shape_width * scale;
                        }
                    }
                    let differs = format!("{:?}", item) != format!("{:?}", st.items[idx]);
                    if differs {
                        let now = std::time::Instant::now();
                        let coalesce = matches!(st.last_style_edit, Some((i, t)) if i == idx && now.duration_since(t).as_secs_f64() < 1.5);
                        if !coalesce {
                            st.checkpoint();
                        } else {
                            st.redo.clear();
                            st.dirty = true;
                        }
                        st.items[idx] = item;
                        st.last_style_edit = Some((idx, now));
                    }
                    differs
                }
                _ => false,
            }
        };
        self.setNeedsDisplay(true);
        if changed_shape {
            self.notify();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.st().undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.st().redo.is_empty()
    }

    /// Returns true when the image size changed (crop undone/redone).
    pub fn undo(&self) -> bool {
        let resized = {
            let mut st = self.st_mut();
            if st.gesture.is_some() {
                return false;
            }
            let Some(cur) = st.snapshot() else { return false };
            let Some(prev) = st.undo.pop() else { return false };
            st.redo.push(cur);
            st.last_style_edit = None;
            let resized = !same_size(st.base.as_deref(), &prev.base);
            st.base = Some(prev.base);
            st.items = prev.items;
            st.selected = None;
            st.dirty = true;
            if resized {
                st.crop = None;
            }
            resized
        };
        if resized {
            self.resize_to_image();
        }
        self.setNeedsDisplay(true);
        self.notify();
        resized
    }

    /// Returns true when the image size changed (crop undone/redone).
    pub fn redo(&self) -> bool {
        let resized = {
            let mut st = self.st_mut();
            if st.gesture.is_some() {
                return false;
            }
            let Some(cur) = st.snapshot() else { return false };
            let Some(next) = st.redo.pop() else { return false };
            st.undo.push(cur);
            st.last_style_edit = None;
            let resized = !same_size(st.base.as_deref(), &next.base);
            st.base = Some(next.base);
            st.items = next.items;
            st.selected = None;
            st.dirty = true;
            if resized {
                st.crop = None;
            }
            resized
        };
        if resized {
            self.resize_to_image();
        }
        self.setNeedsDisplay(true);
        self.notify();
        resized
    }

    /// "Erase all ink" — removes every annotation.
    pub fn clear_ink(&self) {
        {
            let mut st = self.st_mut();
            if st.items.is_empty() || st.gesture.is_some() {
                return;
            }
            st.checkpoint();
            st.items.clear();
            st.selected = None;
        }
        self.setNeedsDisplay(true);
        self.notify();
    }

    pub fn delete_selected(&self) -> bool {
        let done = {
            let mut st = self.st_mut();
            match st.selected {
                Some(idx) if idx < st.items.len() && st.gesture.is_none() => {
                    st.checkpoint();
                    st.items.remove(idx);
                    st.selected = None;
                    true
                }
                _ => false,
            }
        };
        if done {
            self.setNeedsDisplay(true);
            self.notify();
        }
        done
    }

    fn visible_center_px(&self) -> CGPoint {
        let v = self.visibleRect();
        let s = self.st().scale;
        pt((v.origin.x + v.size.width / 2.0) * s, (v.origin.y + v.size.height / 2.0) * s)
    }

    pub fn toggle_ruler(&self) -> bool {
        let c = self.visible_center_px();
        let vis_w = self.visibleRect().size.width;
        let on = {
            let mut st = self.st_mut();
            if st.ruler.is_some() {
                st.ruler = None;
                false
            } else {
                let s = st.scale;
                let len_pt = (vis_w * 0.85).clamp(280.0, 1200.0);
                st.ruler = Some(Ruler { c, angle: 0.0, len: len_pt * s, thick: 56.0 * s });
                true
            }
        };
        self.setNeedsDisplay(true);
        on
    }

    pub fn toggle_protractor(&self) -> bool {
        let c = self.visible_center_px();
        let vis = self.visibleRect().size;
        let on = {
            let mut st = self.st_mut();
            if st.protractor.is_some() {
                st.protractor = None;
                false
            } else {
                let s = st.scale;
                let r_pt = (vis.width.min(vis.height) * 0.35).clamp(90.0, 260.0);
                st.protractor = Some(Protractor { c, r: r_pt * s });
                true
            }
        };
        self.setNeedsDisplay(true);
        on
    }

    pub fn ruler_visible(&self) -> bool {
        self.st().ruler.is_some()
    }

    pub fn protractor_visible(&self) -> bool {
        self.st().protractor.is_some()
    }

    // --- Crop ---------------------------------------------------------------

    pub fn begin_crop(&self) {
        {
            let mut st = self.st_mut();
            let (w, h) = st.size_px();
            st.crop = Some(rect(0.0, 0.0, w, h));
            st.tool = Tool::Crop;
            st.selected = None;
        }
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.setNeedsDisplay(true);
    }

    pub fn cancel_crop(&self) {
        {
            let mut st = self.st_mut();
            st.crop = None;
            st.tool = Tool::None;
        }
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.setNeedsDisplay(true);
    }

    pub fn apply_crop(&self) {
        {
            let mut st = self.st_mut();
            let Some(c) = st.crop.take() else { return };
            st.tool = Tool::None;
            let (w, h) = st.size_px();
            let c = rect_integral(c);
            let full = c.origin.x <= 0.0
                && c.origin.y <= 0.0
                && c.size.width >= w
                && c.size.height >= h;
            if !full && c.size.width >= 1.0 && c.size.height >= 1.0 {
                if let Some(cropped) = st
                    .base
                    .as_ref()
                    .and_then(|b| CGImage::with_image_in_rect(Some(b), c))
                {
                    st.checkpoint();
                    st.base = Some(cropped);
                    for it in st.items.iter_mut() {
                        it.translate(-c.origin.x, -c.origin.y);
                    }
                    let (dx, dy) = (c.origin.x, c.origin.y);
                    if let Some(o) = st.ocr.as_mut() {
                        for l in o.lines.iter_mut() {
                            l.rect.origin.x -= dx;
                            l.rect.origin.y -= dy;
                            for (r, _) in l.sensitive.iter_mut() {
                                r.origin.x -= dx;
                                r.origin.y -= dy;
                            }
                        }
                    }
                    // Keep the ruler / protractor on the cropped image.
                    let (nw, nh) = (c.size.width, c.size.height);
                    if let Some(r) = st.ruler.as_mut() {
                        r.c.x = (r.c.x - dx).clamp(0.0, nw);
                        r.c.y = (r.c.y - dy).clamp(0.0, nh);
                    }
                    if let Some(p) = st.protractor.as_mut() {
                        p.c.x = (p.c.x - dx).clamp(0.0, nw);
                        p.c.y = (p.c.y - dy).clamp(0.0, nh);
                    }
                }
            }
        }
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.resize_to_image();
        self.notify();
    }

    // --- Text actions --------------------------------------------------------

    pub fn set_ocr(&self, lines: Vec<OcrLine>) {
        {
            let mut st = self.st_mut();
            let n = lines.len();
            st.ocr = Some(OcrOverlay { lines, selected: vec![false; n] });
            st.tool = Tool::Text;
            st.selected = None;
        }
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.setNeedsDisplay(true);
    }

    pub fn clear_ocr(&self) {
        {
            let mut st = self.st_mut();
            st.ocr = None;
            if st.tool == Tool::Text {
                st.tool = Tool::None;
            }
        }
        if let Some(w) = self.window() {
            w.invalidateCursorRectsForView(self);
        }
        self.setNeedsDisplay(true);
    }

    pub fn ocr_text(&self, selected_only: bool) -> Option<String> {
        let st = self.st();
        let o = st.ocr.as_ref()?;
        let lines: Vec<&OcrLine> = o
            .lines
            .iter()
            .zip(o.selected.iter())
            .filter(|(_, s)| !selected_only || **s)
            .map(|(l, _)| l)
            .collect();
        if lines.is_empty() {
            return None;
        }
        Some(crate::ocr::join_lines(&lines))
    }

    pub fn ocr_select_all(&self) {
        if let Some(o) = self.st_mut().ocr.as_mut() {
            o.selected.iter_mut().for_each(|s| *s = true);
        }
        self.setNeedsDisplay(true);
    }

    /// Quick redact: cover detected emails / phone numbers. Returns count.
    pub fn redact(&self, emails: bool, phones: bool) -> usize {
        let n = {
            let mut st = self.st_mut();
            let Some(o) = st.ocr.as_ref() else { return 0 };
            let rects: Vec<CGRect> = o
                .lines
                .iter()
                .flat_map(|l| l.sensitive.iter())
                .filter(|(_, k)| (emails && *k == Sensitive::Email) || (phones && *k == Sensitive::Phone))
                .map(|(r, _)| *r)
                .collect();
            if rects.is_empty() {
                return 0;
            }
            st.checkpoint();
            for r in &rects {
                st.items.push(Item::Redact { r: *r });
            }
            rects.len()
        };
        self.setNeedsDisplay(true);
        self.notify();
        n
    }

    pub fn sensitive_count(&self) -> (usize, usize) {
        let st = self.st();
        let Some(o) = st.ocr.as_ref() else { return (0, 0) };
        let mut e = 0;
        let mut p = 0;
        for (_, k) in o.lines.iter().flat_map(|l| l.sensitive.iter()) {
            match k {
                Sensitive::Email => e += 1,
                Sensitive::Phone => p += 1,
            }
        }
        (e, p)
    }

    // --- Output --------------------------------------------------------------

    pub fn is_dirty(&self) -> bool {
        self.st().dirty
    }

    pub fn mark_saved(&self) {
        self.st_mut().dirty = false;
    }

    pub fn scale(&self) -> f64 {
        self.st().scale
    }

    /// The image with all markup burned in.
    pub fn flatten(&self) -> Option<CFRetained<CGImage>> {
        let st = self.st();
        let base = st.base.as_ref()?;
        render_flat(base, &st.items)
    }

    pub fn base_image(&self) -> Option<CFRetained<CGImage>> {
        self.st().base.clone()
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    fn draw(&self) {
        let Some(gc) = NSGraphicsContext::currentContext() else { return };
        let ctx = gc.CGContext();
        let c = Some(&*ctx);
        let mag = self.mag();
        let st = self.st();
        let Some(base) = st.base.as_ref() else { return };
        let (w, h) = st.size_px();
        let s = st.scale;
        CGContext::save_g_state(c);
        CGContext::scale_ctm(c, 1.0 / s, 1.0 / s);

        // Crisp pixels when zoomed in, smooth when zoomed out.
        let quality = if mag / s * 1.0 >= 2.0 {
            CGInterpolationQuality::None
        } else {
            CGInterpolationQuality::High
        };
        CGContext::set_interpolation_quality(c, quality);
        draw_image_flipped(&ctx, base, w, h);
        CGContext::set_interpolation_quality(c, CGInterpolationQuality::High);
        draw_items(&ctx, &st.items);

        if let Some(Gesture::Ink { item, .. }) = st.gesture.as_ref() {
            draw_item(&ctx, item);
        }

        let hpx = HANDLE_PT * s / mag;

        // Selected shape handles.
        if let Some(Item::Shape { kind, a, b, .. }) = st.selected.and_then(|i| st.items.get(i)) {
            let r = rect_from_points(*a, *b);
            CGContext::save_g_state(c);
            CGContext::set_rgb_stroke_color(c, 0.0, 0.47, 0.84, 1.0);
            CGContext::set_line_width(c, 1.0 * s / mag);
            let dash = [4.0 * s / mag, 3.0 * s / mag];
            unsafe { CGContext::set_line_dash(c, 0.0, dash.as_ptr(), 2) };
            if matches!(kind, ShapeKind::Rect | ShapeKind::Ellipse) {
                CGContext::stroke_rect(c, r);
            }
            CGContext::restore_g_state(c);
            for hp in shape_handles(*kind, *a, *b) {
                draw_handle(&ctx, hp, hpx, s / mag);
            }
        }

        if let Some(o) = st.ocr.as_ref() {
            draw_ocr(&ctx, o, w, h, s / mag);
        }
        if let Some(r) = st.ruler.as_ref() {
            draw_ruler(r, &ctx, s);
        }
        if let Some(p) = st.protractor.as_ref() {
            draw_protractor(p, &ctx, s);
        }
        if let Some(cr) = st.crop {
            draw_crop(&ctx, cr, w, h, s / mag);
        }
        if st.tool == Tool::Eraser {
            if let Some(p) = st.hover {
                let r = 8.0 * s / mag;
                CGContext::set_rgb_stroke_color(c, 0.2, 0.2, 0.2, 0.9);
                CGContext::set_line_width(c, 1.0 * s / mag);
                CGContext::stroke_ellipse_in_rect(c, rect(p.x - r, p.y - r, 2.0 * r, 2.0 * r));
                CGContext::set_rgb_stroke_color(c, 1.0, 1.0, 1.0, 0.9);
                let r2 = r + 1.0 * s / mag;
                CGContext::stroke_ellipse_in_rect(c, rect(p.x - r2, p.y - r2, 2.0 * r2, 2.0 * r2));
            }
        }
        CGContext::restore_g_state(c);
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    fn detect_snap(&self, st: &CanvasState, p: CGPoint, width: f64) -> Option<Snap> {
        let tol = 20.0 * st.scale / self.mag();
        if let Some(r) = st.ruler {
            let l = r.to_local(p);
            if l.x.abs() <= r.len / 2.0 + tol {
                for edge in [-r.thick / 2.0, r.thick / 2.0] {
                    if (l.y - edge).abs() <= tol {
                        let out = edge + edge.signum() * width / 2.0;
                        let a = r.from_local(pt(0.0, out));
                        let (sn, cs) = r.angle.sin_cos();
                        return Some(Snap::Line { a, dir: pt(cs, sn) });
                    }
                }
            }
        }
        if let Some(pr) = st.protractor {
            let d = dist(p, pr.c);
            if (d - pr.r).abs() <= tol {
                return Some(Snap::Arc { c: pr.c, r: pr.r + width / 2.0 });
            }
        }
        None
    }

    /// Is `p` inside the ruler / protractor body (and away from its edges)?
    fn grab_instrument(&self, st: &CanvasState, p: CGPoint) -> Option<Gesture> {
        let tol = 20.0 * st.scale / self.mag();
        if let Some(r) = st.ruler {
            let l = r.to_local(p);
            if r.contains(p) && l.y.abs() < r.thick / 2.0 - tol.min(r.thick / 3.0) {
                return Some(Gesture::MoveRuler { last: p });
            }
        }
        if let Some(pr) = st.protractor {
            if dist(p, pr.c) < pr.r - tol {
                return Some(Gesture::MoveProtractor { last: p });
            }
        }
        None
    }

    fn on_mouse_down(&self, event: &NSEvent) {
        if let Some(w) = self.window() {
            w.makeFirstResponder(Some(self));
        }
        let p = self.event_px(event);
        let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
        // Everything that reads state must happen before the mutable borrow
        // below (a nested borrow would panic and abort the app).
        let hpx = self.screen_px(HANDLE_PT + 4.0);
        let erase_tol = self.screen_px(8.0);
        let shape_tol = self.screen_px(6.0);
        let mut needs_notify = false;
        {
            let mut guard = self.st_mut();
            let st = &mut *guard;
            if st.base.is_none() {
                return;
            }
            let gesture = match st.tool {
                Tool::Pen | Tool::Highlighter => {
                    if let Some(g) = self.grab_instrument(st, p) {
                        Some(g)
                    } else {
                        let hl = st.tool == Tool::Highlighter;
                        let (color, wpt) = if hl {
                            (st.style.hl_color, st.style.hl_width)
                        } else {
                            (st.style.pen_color, st.style.pen_width)
                        };
                        let width = wpt * st.scale;
                        let snap = self.detect_snap(st, p, width);
                        let p0 = snap.map(|s| s.apply(p)).unwrap_or(p);
                        st.begin_change();
                        Some(Gesture::Ink {
                            item: Item::Ink { highlighter: hl, color, width, pts: vec![p0] },
                            snap,
                        })
                    }
                }
                Tool::Eraser => {
                    if let Some(g) = self.grab_instrument(st, p) {
                        Some(g)
                    } else {
                        st.begin_change();
                        let tol = erase_tol;
                        let before = st.items.len();
                        st.items.retain(|it| !it.hit(p, tol));
                        if st.items.len() != before {
                            st.dirty = true;
                        }
                        Some(Gesture::Erase)
                    }
                }
                Tool::Shape => {
                    let tol = shape_tol;
                    // 1) handle of the selected shape
                    let mut g = None;
                    if let Some(idx) = st.selected {
                        if let Some(Item::Shape { kind, a, b, .. }) = st.items.get(idx) {
                            for (hi, hp) in shape_handles(*kind, *a, *b).iter().enumerate() {
                                if dist(*hp, p) <= hpx {
                                    g = Some(Gesture::ResizeShape { idx, handle: hi, moved: false });
                                    break;
                                }
                            }
                        }
                    }
                    // 2) body of any shape (topmost first)
                    if g.is_none() {
                        let hit = st
                            .items
                            .iter()
                            .enumerate()
                            .rev()
                            .find(|(_, it)| matches!(it, Item::Shape { .. }) && it.hit(p, tol))
                            .map(|(i, _)| i);
                        if let Some(idx) = hit {
                            st.selected = Some(idx);
                            g = Some(Gesture::MoveShape { idx, last: p, moved: false });
                        }
                    }
                    // 3) instruments, then a brand-new shape
                    if g.is_none() {
                        g = self.grab_instrument(st, p);
                    }
                    if g.is_none() {
                        st.selected = None;
                        g = Some(Gesture::NewShape { start: p });
                    }
                    st.begin_change();
                    g
                }
                Tool::Crop => {
                    let cr = st.crop.unwrap_or(rect(0.0, 0.0, 0.0, 0.0));
                    let handles = crop_handles(cr);
                    let handle = handles
                        .iter()
                        .position(|h| dist(*h, p) <= hpx * 1.4)
                        .or_else(|| if rect_contains(cr, p) { Some(8) } else { None });
                    handle.map(|handle| Gesture::Crop { handle, start_rect: cr, start: p })
                }
                Tool::Text => {
                    if let Some(o) = st.ocr.as_mut() {
                        let hit = o.lines.iter().position(|l| rect_contains(l.rect, p));
                        match hit {
                            Some(i) => {
                                if shift || event.modifierFlags().contains(NSEventModifierFlags::Command) {
                                    o.selected[i] = !o.selected[i];
                                } else {
                                    let only = o.selected.iter().filter(|s| **s).count() == 1 && o.selected[i];
                                    o.selected.iter_mut().for_each(|s| *s = false);
                                    o.selected[i] = !only;
                                }
                            }
                            None => {
                                if !shift {
                                    o.selected.iter_mut().for_each(|s| *s = false);
                                }
                            }
                        }
                    }
                    Some(Gesture::TextSelect { start: p })
                }
                Tool::None => {
                    if let Some(g) = self.grab_instrument(st, p) {
                        Some(g)
                    } else {
                        NSCursor::closedHandCursor().set();
                        Some(Gesture::Pan { last: event.locationInWindow() })
                    }
                }
            };
            st.gesture = gesture;
            if matches!(st.gesture, Some(Gesture::Erase)) && st.dirty {
                needs_notify = true;
            }
        }
        self.setNeedsDisplay(true);
        if needs_notify {
            self.notify();
        }
    }

    fn on_mouse_dragged(&self, event: &NSEvent) {
        let p = self.event_px(event);
        let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
        let mut invalidate: Option<CGRect> = None;
        let mut full = false;
        let mut pan: Option<(f64, f64)> = None;
        {
            let mut guard = self.st_mut();
            let st = &mut *guard;
            st.hover = Some(p);
            let (img_w, img_h) = st.size_px();
            let scale = st.scale;
            let mut gesture = st.gesture.take();
            match gesture.as_mut() {
                Some(Gesture::Ink { item: Item::Ink { pts, width, .. }, snap }) => {
                    let q = snap.map(|s| s.apply(p)).unwrap_or(p);
                    match snap {
                        Some(Snap::Line { .. }) => {
                            let first = pts[0];
                            pts.truncate(1);
                            pts.push(q);
                            invalidate = Some(rect_from_points(first, q));
                            full = true;
                        }
                        _ => {
                            let last = *pts.last().unwrap();
                            if dist(last, q) >= 0.75 {
                                pts.push(q);
                                let prev = if pts.len() >= 3 { pts[pts.len() - 3] } else { last };
                                let r = rect_from_points(prev, q);
                                let r = crate::util::rect_union(r, rect_from_points(last, q));
                                let pad = *width;
                                invalidate = Some(rect(
                                    r.origin.x - pad,
                                    r.origin.y - pad,
                                    r.size.width + 2.0 * pad,
                                    r.size.height + 2.0 * pad,
                                ));
                            }
                        }
                    }
                }
                Some(Gesture::Erase) => {
                    let tol = 8.0 * scale / self.mag();
                    let before = st.items.len();
                    st.items.retain(|it| !it.hit(p, tol));
                    if st.items.len() != before {
                        st.dirty = true;
                    }
                    full = true;
                }
                Some(Gesture::NewShape { start }) => {
                    let start = *start;
                    let mut end = p;
                    let kind = st.style.shape_kind;
                    if shift {
                        end = constrain(kind, start, end);
                    }
                    let item = Item::Shape {
                        kind,
                        a: start,
                        b: end,
                        stroke: st.style.shape_stroke,
                        fill: st.style.shape_fill,
                        width: st.style.shape_width * scale,
                    };
                    match st.selected {
                        Some(idx) if idx < st.items.len() => st.items[idx] = item,
                        _ => {
                            st.items.push(item);
                            st.selected = Some(st.items.len() - 1);
                        }
                    }
                    full = true;
                }
                Some(Gesture::MoveShape { idx, last, moved }) => {
                    let (dx, dy) = (p.x - last.x, p.y - last.y);
                    if dx != 0.0 || dy != 0.0 {
                        if let Some(it) = st.items.get_mut(*idx) {
                            it.translate(dx, dy);
                        }
                        *last = p;
                        *moved = true;
                        full = true;
                    }
                }
                Some(Gesture::ResizeShape { idx, handle, moved }) => {
                    let before = st.items.get(*idx).map(|it| format!("{it:?}"));
                    if let Some(Item::Shape { kind, a, b, .. }) = st.items.get_mut(*idx) {
                        match kind {
                            ShapeKind::Line | ShapeKind::Arrow => {
                                if *handle == 0 {
                                    *a = if shift { constrain(*kind, *b, p) } else { p };
                                } else {
                                    *b = if shift { constrain(*kind, *a, p) } else { p };
                                }
                            }
                            _ => match handle {
                                0 => *a = p,
                                1 => {
                                    b.x = p.x;
                                    a.y = p.y;
                                }
                                2 => *b = p,
                                _ => {
                                    a.x = p.x;
                                    b.y = p.y;
                                }
                            },
                        }
                    }
                    if st.items.get(*idx).map(|it| format!("{it:?}")) != before {
                        *moved = true;
                    }
                    full = true;
                }
                Some(Gesture::MoveRuler { last }) => {
                    if let Some(r) = st.ruler.as_mut() {
                        r.c.x += p.x - last.x;
                        r.c.y += p.y - last.y;
                    }
                    *last = p;
                    full = true;
                }
                Some(Gesture::MoveProtractor { last }) => {
                    if let Some(pr) = st.protractor.as_mut() {
                        pr.c.x += p.x - last.x;
                        pr.c.y += p.y - last.y;
                    }
                    *last = p;
                    full = true;
                }
                Some(Gesture::Crop { handle, start_rect, start }) => {
                    let (dx, dy) = (p.x - start.x, p.y - start.y);
                    let r = *start_rect;
                    let (mut x0, mut y0) = (r.origin.x, r.origin.y);
                    let (mut x1, mut y1) = (x0 + r.size.width, y0 + r.size.height);
                    let min = 8.0;
                    match *handle {
                        0 => { x0 += dx; y0 += dy; }
                        1 => { x1 += dx; y0 += dy; }
                        2 => { x1 += dx; y1 += dy; }
                        3 => { x0 += dx; y1 += dy; }
                        4 => { y0 += dy; }
                        5 => { x1 += dx; }
                        6 => { y1 += dy; }
                        7 => { x0 += dx; }
                        _ => {
                            let mx = dx.clamp(-x0, img_w - x1);
                            let my = dy.clamp(-y0, img_h - y1);
                            x0 += mx; x1 += mx; y0 += my; y1 += my;
                        }
                    }
                    x0 = x0.clamp(0.0, img_w);
                    x1 = x1.clamp(0.0, img_w);
                    y0 = y0.clamp(0.0, img_h);
                    y1 = y1.clamp(0.0, img_h);
                    if x1 - x0 < min {
                        if matches!(*handle, 0 | 3 | 7) { x0 = x1 - min; } else { x1 = x0 + min; }
                    }
                    if y1 - y0 < min {
                        if matches!(*handle, 0 | 1 | 4) { y0 = y1 - min; } else { y1 = y0 + min; }
                    }
                    st.crop = Some(rect(x0, y0, x1 - x0, y1 - y0));
                    full = true;
                }
                Some(Gesture::TextSelect { start }) => {
                    let sel = rect_from_points(*start, p);
                    if sel.size.width > 3.0 || sel.size.height > 3.0 {
                        if let Some(o) = st.ocr.as_mut() {
                            for (i, l) in o.lines.iter().enumerate() {
                                if crate::util::rect_intersection(l.rect, sel).is_some() {
                                    o.selected[i] = true;
                                } else if !shift {
                                    o.selected[i] = false;
                                }
                            }
                        }
                        full = true;
                    }
                }
                Some(Gesture::Pan { last }) => {
                    let now = event.locationInWindow();
                    pan = Some((now.x - last.x, now.y - last.y));
                    *last = now;
                }
                _ => {}
            }
            st.gesture = gesture;
        }
        if let Some((dx, dy)) = pan {
            let mag = self.mag();
            let v = self.visibleRect();
            self.scrollPoint(NSPoint::new(v.origin.x - dx / mag, v.origin.y + dy / mag));
        }
        if full {
            self.setNeedsDisplay(true);
        } else if let Some(r) = invalidate {
            self.invalidate_px(r);
        }
    }

    fn on_mouse_up(&self, _event: &NSEvent) {
        let mut changed = false;
        {
            let mut guard = self.st_mut();
            let st = &mut *guard;
            match st.gesture.take() {
                Some(Gesture::Ink { item, .. }) => {
                    st.items.push(item);
                    st.commit_change();
                    changed = true;
                }
                Some(Gesture::Erase) => {
                    let erased = st
                        .pending
                        .as_ref()
                        .map(|s| s.items.len() != st.items.len())
                        .unwrap_or(false);
                    if erased {
                        st.commit_change();
                        changed = true;
                    } else {
                        st.pending = None;
                    }
                }
                Some(Gesture::NewShape { .. }) => {
                    let tiny = st
                        .selected
                        .and_then(|i| st.items.get(i))
                        .map(|it| match it {
                            Item::Shape { a, b, .. } => dist(*a, *b) < 3.0,
                            _ => false,
                        })
                        .unwrap_or(true);
                    if tiny {
                        if let Some(i) = st.selected.take() {
                            if i < st.items.len() {
                                st.items.remove(i);
                            }
                        }
                        st.pending = None;
                    } else {
                        st.commit_change();
                        changed = true;
                    }
                }
                Some(Gesture::MoveShape { moved, .. }) => {
                    if moved {
                        st.commit_change();
                        changed = true;
                    } else {
                        st.pending = None;
                    }
                }
                Some(Gesture::ResizeShape { moved, .. }) => {
                    if moved {
                        st.commit_change();
                        changed = true;
                    } else {
                        st.pending = None;
                    }
                }
                Some(Gesture::Pan { .. }) => {
                    NSCursor::openHandCursor().set();
                }
                _ => {}
            }
        }
        self.setNeedsDisplay(true);
        if changed {
            self.notify();
        }
    }

    /// Returns true if the event was consumed (rotating / resizing instruments).
    fn on_scroll(&self, event: &NSEvent) -> bool {
        let p = self.event_px(event);
        let dy = event.scrollingDeltaY();
        let precise = event.hasPreciseScrollingDeltas();
        let step = if precise { dy * 0.25 } else { dy * 3.0 };
        let consumed = {
            let mut st = self.st_mut();
            if let Some(r) = st.ruler.as_mut().filter(|r| r.contains(p)) {
                let deg = (r.angle.to_degrees() + step).rem_euclid(360.0);
                r.angle = deg.to_radians();
                true
            } else if let Some(pr) = st.protractor.as_mut().filter(|pr| dist(p, pr.c) <= pr.r) {
                pr.r = (pr.r * (1.0 + step * 0.01)).max(40.0);
                true
            } else {
                false
            }
        };
        if consumed {
            self.setNeedsDisplay(true);
        }
        consumed
    }

    fn on_key(&self, event: &NSEvent) -> bool {
        let code = event.keyCode();
        match code {
            51 | 117 => self.delete_selected(),
            53 => {
                let had_sel = self.st_mut().selected.take().is_some();
                self.setNeedsDisplay(true);
                if !had_sel {
                    if let Some(f) = self.ivars().hooks.borrow().key.as_ref() {
                        f(CanvasKey::Escape);
                    }
                }
                true
            }
            36 | 76 => {
                if let Some(f) = self.ivars().hooks.borrow().key.as_ref() {
                    f(CanvasKey::Return);
                }
                true
            }
            _ => false,
        }
    }
}

fn same_size(a: Option<&CGImage>, b: &CGImage) -> bool {
    match a {
        Some(a) => image_size(a) == image_size(b),
        None => false,
    }
}

fn constrain(kind: ShapeKind, a: CGPoint, b: CGPoint) -> CGPoint {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    match kind {
        ShapeKind::Rect | ShapeKind::Ellipse => {
            let m = dx.abs().max(dy.abs());
            pt(a.x + m * dx.signum(), a.y + m * dy.signum())
        }
        ShapeKind::Line | ShapeKind::Arrow => {
            let ang = dy.atan2(dx);
            let snapped = (ang / (PI / 4.0)).round() * (PI / 4.0);
            let l = (dx * dx + dy * dy).sqrt();
            pt(a.x + l * snapped.cos(), a.y + l * snapped.sin())
        }
    }
}

fn shape_handles(kind: ShapeKind, a: CGPoint, b: CGPoint) -> Vec<CGPoint> {
    match kind {
        ShapeKind::Line | ShapeKind::Arrow => vec![a, b],
        _ => vec![a, pt(b.x, a.y), b, pt(a.x, b.y)],
    }
}

fn crop_handles(r: CGRect) -> [CGPoint; 8] {
    let (x0, y0) = (r.origin.x, r.origin.y);
    let (x1, y1) = (x0 + r.size.width, y0 + r.size.height);
    let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    [pt(x0, y0), pt(x1, y0), pt(x1, y1), pt(x0, y1), pt(mx, y0), pt(x1, my), pt(mx, y1), pt(x0, my)]
}

fn draw_handle(ctx: &CGContext, p: CGPoint, r: f64, line: f64) {
    let c = Some(ctx);
    CGContext::set_rgb_fill_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::set_rgb_stroke_color(c, 0.0, 0.47, 0.84, 1.0);
    CGContext::set_line_width(c, 1.5 * line);
    let rr = rect(p.x - r, p.y - r, 2.0 * r, 2.0 * r);
    CGContext::fill_ellipse_in_rect(c, rr);
    CGContext::stroke_ellipse_in_rect(c, rr);
}

fn draw_crop(ctx: &CGContext, cr: CGRect, w: f64, h: f64, unit: f64) {
    let c = Some(ctx);
    CGContext::save_g_state(c);
    // Dim outside.
    CGContext::begin_path(c);
    CGContext::add_rect(c, rect(0.0, 0.0, w, h));
    CGContext::add_rect(c, cr);
    CGContext::set_rgb_fill_color(c, 0.0, 0.0, 0.0, 0.5);
    CGContext::eo_fill_path(c);
    // Thirds grid.
    CGContext::set_rgb_stroke_color(c, 1.0, 1.0, 1.0, 0.45);
    CGContext::set_line_width(c, 1.0 * unit);
    for i in 1..3 {
        let x = cr.origin.x + cr.size.width * i as f64 / 3.0;
        let y = cr.origin.y + cr.size.height * i as f64 / 3.0;
        CGContext::begin_path(c);
        CGContext::move_to_point(c, x, cr.origin.y);
        CGContext::add_line_to_point(c, x, cr.origin.y + cr.size.height);
        CGContext::move_to_point(c, cr.origin.x, y);
        CGContext::add_line_to_point(c, cr.origin.x + cr.size.width, y);
        CGContext::stroke_path(c);
    }
    // Border.
    CGContext::set_rgb_stroke_color(c, 1.0, 1.0, 1.0, 1.0);
    CGContext::set_line_width(c, 1.5 * unit);
    CGContext::stroke_rect(c, cr);
    // Corner brackets + edge bars (Windows style).
    let l = (18.0 * unit).min(cr.size.width / 2.0).min(cr.size.height / 2.0);
    CGContext::set_line_width(c, 4.0 * unit);
    CGContext::set_line_cap(c, CGLineCap::Square);
    let (x0, y0) = (cr.origin.x, cr.origin.y);
    let (x1, y1) = (x0 + cr.size.width, y0 + cr.size.height);
    let corners = [(x0, y0, 1.0, 1.0), (x1, y0, -1.0, 1.0), (x1, y1, -1.0, -1.0), (x0, y1, 1.0, -1.0)];
    CGContext::begin_path(c);
    for (x, y, sx, sy) in corners {
        CGContext::move_to_point(c, x + sx * l, y);
        CGContext::add_line_to_point(c, x, y);
        CGContext::add_line_to_point(c, x, y + sy * l);
    }
    let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let e = l * 0.6;
    CGContext::move_to_point(c, mx - e, y0);
    CGContext::add_line_to_point(c, mx + e, y0);
    CGContext::move_to_point(c, mx - e, y1);
    CGContext::add_line_to_point(c, mx + e, y1);
    CGContext::move_to_point(c, x0, my - e);
    CGContext::add_line_to_point(c, x0, my + e);
    CGContext::move_to_point(c, x1, my - e);
    CGContext::add_line_to_point(c, x1, my + e);
    CGContext::stroke_path(c);
    CGContext::restore_g_state(c);
}

fn draw_ocr(ctx: &CGContext, o: &OcrOverlay, w: f64, h: f64, unit: f64) {
    let c = Some(ctx);
    CGContext::save_g_state(c);
    // Dim everything except text.
    CGContext::begin_path(c);
    CGContext::add_rect(c, rect(0.0, 0.0, w, h));
    for l in &o.lines {
        CGContext::add_rect(c, crate::util::rect_inset(l.rect, -2.0 * unit));
    }
    CGContext::set_rgb_fill_color(c, 0.0, 0.0, 0.0, 0.35);
    CGContext::eo_fill_path(c);
    for (l, sel) in o.lines.iter().zip(o.selected.iter()) {
        let r = crate::util::rect_inset(l.rect, -2.0 * unit);
        if *sel {
            CGContext::set_rgb_fill_color(c, 0.0, 0.47, 0.84, 0.35);
            CGContext::fill_rect(c, r);
        }
        CGContext::set_rgb_stroke_color(c, 1.0, 1.0, 1.0, 0.9);
        CGContext::set_line_width(c, 1.0 * unit);
        CGContext::stroke_rect(c, r);
    }
    CGContext::restore_g_state(c);
}

fn text_attrs(size: f64, color: &NSColor) -> Retained<NSDictionary<NSString, AnyObject>> {
    let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, unsafe { NSFontWeightMedium });
    let keys: [&NSString; 2] = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let vals: [&AnyObject; 2] = [&font, color];
    NSDictionary::from_slices(&keys, &vals)
}

fn draw_text_centered(text: &str, at: CGPoint, size: f64, color: &NSColor) {
    let s = ns(text);
    let attrs = text_attrs(size, color);
    unsafe {
        let sz = s.sizeWithAttributes(Some(&attrs));
        s.drawAtPoint_withAttributes(
            NSPoint::new(at.x - sz.width / 2.0, at.y - sz.height / 2.0),
            Some(&attrs),
        );
    }
}

fn draw_ruler(r: &Ruler, ctx: &CGContext, s: f64) {
    let c = Some(ctx);
    CGContext::save_g_state(c);
    CGContext::translate_ctm(c, r.c.x, r.c.y);
    CGContext::rotate_ctm(c, r.angle);
    let body = rect(-r.len / 2.0, -r.thick / 2.0, r.len, r.thick);
    CGContext::set_shadow_with_color(
        c,
        objc2_core_foundation::CGSize::new(0.0, 2.0 * s),
        6.0 * s,
        Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 0.3)),
    );
    CGContext::set_rgb_fill_color(c, 0.98, 0.98, 0.98, 0.9);
    CGContext::fill_rect(c, body);
    CGContext::set_shadow_with_color(c, objc2_core_foundation::CGSize::new(0.0, 0.0), 0.0, None);
    CGContext::set_rgb_stroke_color(c, 0.45, 0.45, 0.45, 1.0);
    CGContext::set_line_width(c, 1.0 * s);
    CGContext::stroke_rect(c, body);
    let unit = 10.0 * s;
    let n = (r.len / unit) as i64;
    CGContext::set_rgb_stroke_color(c, 0.2, 0.2, 0.2, 1.0);
    CGContext::begin_path(c);
    for i in 0..=n {
        let x = -r.len / 2.0 + i as f64 * unit;
        let tl = if i % 10 == 0 {
            r.thick * 0.32
        } else if i % 5 == 0 {
            r.thick * 0.22
        } else {
            r.thick * 0.12
        };
        CGContext::move_to_point(c, x, -r.thick / 2.0);
        CGContext::add_line_to_point(c, x, -r.thick / 2.0 + tl);
        CGContext::move_to_point(c, x, r.thick / 2.0);
        CGContext::add_line_to_point(c, x, r.thick / 2.0 - tl);
    }
    CGContext::stroke_path(c);
    let dark = NSColor::colorWithSRGBRed_green_blue_alpha(0.15, 0.15, 0.15, 1.0);
    let mut i = 10;
    while i < n {
        let x = -r.len / 2.0 + i as f64 * unit;
        draw_text_centered(&format!("{}", i / 10), pt(x, -r.thick / 2.0 + r.thick * 0.48), 8.5 * s, &dark);
        i += 10;
    }
    let deg = (r.angle.to_degrees().round() as i64).rem_euclid(360);
    draw_text_centered(&format!("{deg}°"), pt(0.0, r.thick * 0.18), 12.0 * s, &dark);
    CGContext::restore_g_state(c);
}

fn draw_protractor(p: &Protractor, ctx: &CGContext, s: f64) {
    let c = Some(ctx);
    CGContext::save_g_state(c);
    let disc = rect(p.c.x - p.r, p.c.y - p.r, 2.0 * p.r, 2.0 * p.r);
    CGContext::set_shadow_with_color(
        c,
        objc2_core_foundation::CGSize::new(0.0, 2.0 * s),
        6.0 * s,
        Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 0.3)),
    );
    CGContext::set_rgb_fill_color(c, 0.98, 0.98, 0.98, 0.18);
    CGContext::fill_ellipse_in_rect(c, disc);
    // Opaque scale ring, see-through middle (like the Windows protractor).
    let band = (34.0 * s).min(p.r * 0.45);
    CGContext::begin_path(c);
    CGContext::add_ellipse_in_rect(c, disc);
    CGContext::add_ellipse_in_rect(c, rect(p.c.x - p.r + band, p.c.y - p.r + band, 2.0 * (p.r - band), 2.0 * (p.r - band)));
    CGContext::set_rgb_fill_color(c, 0.98, 0.98, 0.98, 0.88);
    CGContext::eo_fill_path(c);
    CGContext::set_shadow_with_color(c, objc2_core_foundation::CGSize::new(0.0, 0.0), 0.0, None);
    CGContext::set_rgb_stroke_color(c, 0.35, 0.35, 0.35, 1.0);
    CGContext::set_line_width(c, 1.0 * s);
    CGContext::stroke_ellipse_in_rect(c, disc);
    CGContext::begin_path(c);
    for d in 0..360 {
        let a = (d as f64).to_radians();
        let tl = if d % 10 == 0 { 14.0 } else if d % 5 == 0 { 9.0 } else { 5.0 } * s;
        let (sn, cs) = a.sin_cos();
        CGContext::move_to_point(c, p.c.x + cs * p.r, p.c.y - sn * p.r);
        CGContext::add_line_to_point(c, p.c.x + cs * (p.r - tl), p.c.y - sn * (p.r - tl));
    }
    // Center cross.
    let cr = 8.0 * s;
    CGContext::move_to_point(c, p.c.x - cr, p.c.y);
    CGContext::add_line_to_point(c, p.c.x + cr, p.c.y);
    CGContext::move_to_point(c, p.c.x, p.c.y - cr);
    CGContext::add_line_to_point(c, p.c.x, p.c.y + cr);
    CGContext::stroke_path(c);
    let dark = NSColor::colorWithSRGBRed_green_blue_alpha(0.15, 0.15, 0.15, 1.0);
    let lr = p.r - 24.0 * s;
    if lr > 20.0 * s {
        let mut d = 0;
        while d < 360 {
            let a = (d as f64).to_radians();
            draw_text_centered(
                &format!("{d}"),
                pt(p.c.x + a.cos() * lr, p.c.y - a.sin() * lr),
                8.0 * s,
                &dark,
            );
            d += 30;
        }
    }
    CGContext::restore_g_state(c);
}

/// Burn annotations into a new image.
pub fn render_flat(base: &CGImage, items: &[Item]) -> Option<CFRetained<CGImage>> {
    if items.is_empty() {
        return Some(crate::util::cf_retain(base));
    }
    let (w, h) = image_size(base);
    let ctx = bitmap_context(w, h, CGImage::color_space(Some(base)).as_deref())?;
    let c = Some(&*ctx);
    CGContext::translate_ctm(c, 0.0, h as f64);
    CGContext::scale_ctm(c, 1.0, -1.0);
    draw_image_flipped(&ctx, base, w as f64, h as f64);
    draw_items(&ctx, items);
    CGBitmapContextCreateImage(c)
}

#[allow(dead_code)]
pub fn any_obj<T: Message>(o: &T) -> &AnyObject {
    unsafe { &*(o as *const T as *const AnyObject) }
}

impl CanvasView {
    /// Add an annotation programmatically (undoable). Used by the self-test.
    pub fn push_item(&self, item: Item) {
        {
            let mut st = self.st_mut();
            st.checkpoint();
            st.items.push(item);
        }
        self.setNeedsDisplay(true);
        self.notify();
    }

    pub fn undo_depth(&self) -> usize {
        self.st().undo.len()
    }

    pub fn item_count(&self) -> usize {
        self.st().items.len()
    }

    /// Select an item by index (shape handles become visible).
    pub fn select(&self, idx: Option<usize>) {
        self.st_mut().selected = idx;
        self.setNeedsDisplay(true);
    }

    /// Set the crop rectangle (image pixels) while in crop mode.
    pub fn set_crop_rect(&self, r: CGRect) {
        self.st_mut().crop = Some(r);
        self.setNeedsDisplay(true);
    }
}
