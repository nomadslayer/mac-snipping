//! Tool option flyouts (pen / highlighter / shapes / eraser), like the
//! Windows Snipping Tool's color + size popovers.

use std::rc::Rc;

use objc2::rc::Retained;
use objc2_app_kit::*;
use objc2_foundation::*;

use crate::annot::ShapeKind;
use crate::canvas::CanvasView;
use crate::util::{self, fixed_size, label, mtm, ns, on_action, on_click, rect, symbol, vstack, Rgba};

pub const PEN_COLORS: [u32; 18] = [
    0x000000, 0xFFFFFF, 0x7A7A7A, 0xE81123, 0xFF8C00, 0xFFD700,
    0x16C60C, 0x00B7C3, 0x0078D4, 0x5C2D91, 0xE3008C, 0x8E562E,
    0xF7630C, 0xC239B3, 0x00CC6A, 0x4F6BED, 0xB4009E, 0x2D7D9A,
];

pub const HIGHLIGHTER_COLORS: [u32; 6] = [0xFFEB3B, 0x7CFC00, 0x40E0D0, 0xFF69B4, 0xFFA500, 0xB388FF];

fn same(a: Rgba, b: Rgba) -> bool {
    (a.r - b.r).abs() < 0.01 && (a.g - b.g).abs() < 0.01 && (a.b - b.b).abs() < 0.01
}

fn style_swatch(b: &NSButton, color: Option<Rgba>, selected: bool) {
    if let Some(layer) = b.layer() {
        match color {
            Some(c) => layer.setBackgroundColor(Some(&c.cg())),
            None => layer.setBackgroundColor(Some(&Rgba::hex(0xFFFFFF).cg())),
        }
        layer.setCornerRadius(12.0);
        if selected {
            layer.setBorderWidth(3.0);
            layer.setBorderColor(Some(&Rgba::hex(0x0A84FF).cg()));
        } else {
            layer.setBorderWidth(1.0);
            layer.setBorderColor(Some(&Rgba::new(0.5, 0.5, 0.5, 0.6).cg()));
        }
    }
}

/// A grid of color swatches. `on_pick` gets the chosen color (None = no fill).
fn swatch_grid(
    colors: &[Option<Rgba>],
    current: Option<Rgba>,
    per_row: usize,
    on_pick: impl Fn(Option<Rgba>) + 'static,
) -> Retained<NSStackView> {
    let on_pick = Rc::new(on_pick);
    let buttons: Rc<std::cell::RefCell<Vec<(Option<Rgba>, Retained<NSButton>)>>> = Rc::default();
    let mut rows: Vec<Retained<NSStackView>> = Vec::new();
    let mut row: Vec<Retained<NSView>> = Vec::new();
    for (i, c) in colors.iter().enumerate() {
        let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(""), None, None, mtm()) };
        b.setBordered(false);
        b.setWantsLayer(true);
        fixed_size(&b, 24.0, 24.0);
        if c.is_none() {
            b.setImage(Some(&symbol("nosign", 14.0)));
            b.setContentTintColor(Some(&NSColor::systemRedColor()));
        }
        let selected = match (c, current) {
            (Some(a), Some(b)) => same(*a, b),
            (None, None) => true,
            _ => false,
        };
        style_swatch(&b, *c, selected);
        b.setToolTip(Some(&ns(&c.map(|c| c.to_hex()).unwrap_or_else(|| "No fill".into()))));
        let color = *c;
        let on_pick2 = on_pick.clone();
        let buttons2 = Rc::downgrade(&buttons);
        on_click(&b, move || {
            if let Some(all) = buttons2.upgrade() {
                for (bc, bb) in all.borrow().iter() {
                    let sel = match (bc, color) {
                        (Some(a), Some(b)) => same(*a, b),
                        (None, None) => true,
                        _ => false,
                    };
                    style_swatch(bb, *bc, sel);
                }
            }
            on_pick2(color);
        });
        buttons.borrow_mut().push((color, b.clone()));
        row.push(Retained::into_super(Retained::into_super(b)));
        if row.len() == per_row || i == colors.len() - 1 {
            let refs: Vec<&NSView> = row.iter().map(|v| &**v).collect();
            rows.push(util::hstack(&refs, 8.0));
            row.clear();
        }
    }
    // Keep the shared button list alive as long as the grid lives.
    let refs: Vec<&NSView> = rows.iter().map(|v| &***v).collect();
    let grid = vstack(&refs, 8.0);
    let holder = util::ActionTarget::new(move |_| {
        let _ = &buttons;
    });
    static KEY: u8 = 0;
    util::retain_on(&grid, &holder, &KEY);
    grid
}

fn size_slider(
    title: &str,
    value: f64,
    min: f64,
    max: f64,
    on_change: impl Fn(f64) + 'static,
) -> Retained<NSStackView> {
    let value_label = label(&format!("{} pt", value.round() as i64), 12.0, false);
    value_label.setTextColor(Some(&NSColor::secondaryLabelColor()));
    fixed_size(&value_label, 44.0, 0.0);
    let slider = unsafe { NSSlider::sliderWithValue_minValue_maxValue_target_action(value, min, max, None, None, mtm()) };
    slider.setContinuous(true);
    fixed_size(&slider, 170.0, 0.0);
    let vl = value_label.clone();
    on_action(&slider, move |sender| {
        if let Some(s) = sender.and_then(|s| s.downcast_ref::<NSSlider>()) {
            let v = s.doubleValue().round();
            vl.setStringValue(&ns(&format!("{} pt", v as i64)));
            on_change(v);
        }
    });
    let row = util::hstack(&[&slider, &value_label], 8.0);
    vstack(&[&label(title, 12.0, true), &row], 6.0)
}

fn popover_with(content: &NSView) -> Retained<NSPopover> {
    let m = mtm();
    let pop = NSPopover::new(m);
    pop.setBehavior(NSPopoverBehavior::Transient);
    let vc = NSViewController::new(m);
    let size = content.fittingSize();
    content.setFrameSize(size);
    vc.setView(content);
    pop.setContentViewController(Some(&vc));
    pop
}

fn padded(stack: &NSStackView) {
    stack.setEdgeInsets(NSEdgeInsets { top: 14.0, left: 14.0, bottom: 14.0, right: 14.0 });
}

pub fn pen_popover(canvas: Retained<CanvasView>, highlighter: bool) -> Retained<NSPopover> {
    let style = canvas.style();
    let (colors, current, width, min, max): (Vec<Option<Rgba>>, Rgba, f64, f64, f64) = if highlighter {
        (
            HIGHLIGHTER_COLORS.iter().map(|c| Some(Rgba::hex(*c))).collect(),
            style.hl_color,
            style.hl_width,
            6.0,
            48.0,
        )
    } else {
        (PEN_COLORS.iter().map(|c| Some(Rgba::hex(*c))).collect(), style.pen_color, style.pen_width, 1.0, 24.0)
    };
    let c1 = canvas.clone();
    let grid = swatch_grid(&colors, Some(current), 6, move |c| {
        if let Some(c) = c {
            c1.update_style(|s| if highlighter { s.hl_color = c } else { s.pen_color = c });
        }
    });
    let c2 = canvas.clone();
    let slider = size_slider("Size", width, min, max, move |v| {
        c2.update_style(|s| if highlighter { s.hl_width = v } else { s.pen_width = v });
    });
    let custom = {
        let c3 = canvas.clone();
        util::text_icon_button("More colors…", "paintpalette", move || {
            let panel = NSColorPanel::sharedColorPanel(mtm());
            // Weak: the shared panel must not keep a closed editor alive.
            let c4 = objc2::rc::Weak::from_retained(&c3);
            on_color_panel(&panel, move |c| {
                if let Some(canvas) = c4.load() {
                    canvas.update_style(|s| if highlighter { s.hl_color = c } else { s.pen_color = c });
                }
            });
            panel.orderFront(None);
        })
    };
    let title = if highlighter { "Highlighter" } else { "Ballpoint pen" };
    let stack = vstack(&[&label(title, 13.0, true), &grid, &custom, &slider], 12.0);
    padded(&stack);
    popover_with(&stack)
}

thread_local! {
    /// Our current target on the shared color panel (the panel doesn't retain it).
    static PANEL_TARGET: std::cell::RefCell<Option<Retained<util::ActionTarget>>> = const { std::cell::RefCell::new(None) };
}

fn on_color_panel(panel: &NSColorPanel, f: impl Fn(Rgba) + 'static) {
    let t = util::ActionTarget::new(move |sender| {
        if let Some(p) = sender.and_then(|s| s.downcast_ref::<NSColorPanel>()) {
            f(Rgba::from_ns(&p.color()));
        }
    });
    unsafe {
        panel.setTarget(Some(&t));
        panel.setAction(Some(objc2::sel!(invoke:)));
    }
    PANEL_TARGET.with(|p| *p.borrow_mut() = Some(t));
}

/// Stop the color panel from editing a tool that is no longer active.
pub fn detach_color_panel() {
    let had = PANEL_TARGET.with(|p| p.borrow_mut().take()).is_some();
    if !had || !NSColorPanel::sharedColorPanelExists(mtm()) {
        return;
    }
    let panel = NSColorPanel::sharedColorPanel(mtm());
    unsafe {
        panel.setTarget(None);
        panel.setAction(None);
    }
    panel.orderOut(None);
}

pub fn shape_popover(canvas: Retained<CanvasView>, on_kind: impl Fn(ShapeKind) + 'static) -> Retained<NSPopover> {
    let style = canvas.style();
    let m = mtm();
    let seg = unsafe {
        NSSegmentedControl::segmentedControlWithImages_trackingMode_target_action(
            &NSArray::from_retained_slice(&ShapeKind::ALL.map(|k| symbol(k.symbol(), 14.0))),
            NSSegmentSwitchTracking::SelectOne,
            None,
            None,
            m,
        )
    };
    for (i, k) in ShapeKind::ALL.iter().enumerate() {
        seg.setToolTip_forSegment(Some(&ns(k.title())), i as isize);
        seg.setWidth_forSegment(40.0, i as isize);
    }
    seg.setSelectedSegment(style.shape_kind.index() as isize);
    let c0 = canvas.clone();
    on_action(&seg, move |sender| {
        if let Some(s) = sender.and_then(|s| s.downcast_ref::<NSSegmentedControl>()) {
            let k = ShapeKind::from_index(s.selectedSegment().max(0) as u32);
            c0.update_style(|st| st.shape_kind = k);
            on_kind(k);
        }
    });
    let colors: Vec<Option<Rgba>> = PEN_COLORS.iter().map(|c| Some(Rgba::hex(*c))).collect();
    let c1 = canvas.clone();
    let outline = swatch_grid(&colors, Some(style.shape_stroke), 6, move |c| {
        if let Some(c) = c {
            c1.update_style(|s| s.shape_stroke = c);
        }
    });
    let mut fills: Vec<Option<Rgba>> = vec![None];
    fills.extend(PEN_COLORS.iter().take(11).map(|c| Some(Rgba::hex(*c))));
    let c2 = canvas.clone();
    let fill = swatch_grid(&fills, style.shape_fill, 6, move |c| {
        c2.update_style(|s| s.shape_fill = c);
    });
    let c3 = canvas.clone();
    let slider = size_slider("Thickness", style.shape_width, 1.0, 20.0, move |v| {
        c3.update_style(|s| s.shape_width = v);
    });
    let hint = util::secondary_label("Hold ⇧ for squares, circles and 45° lines.", 11.0);
    let stack = vstack(
        &[
            &label("Shapes", 13.0, true),
            &seg,
            &label("Outline", 12.0, true),
            &outline,
            &label("Fill", 12.0, true),
            &fill,
            &slider,
            &hint,
        ],
        10.0,
    );
    padded(&stack);
    popover_with(&stack)
}

pub fn eraser_popover(canvas: Retained<CanvasView>, after_clear: impl Fn() + 'static) -> Retained<NSPopover> {
    let note = util::secondary_label("Drag over ink or shapes to erase them.", 12.0);
    let button = util::text_icon_button("Erase all ink", "trash", move || {
        canvas.clear_ink();
        after_clear();
    });
    let stack = vstack(&[&label("Eraser", 13.0, true), &note, &button], 10.0);
    padded(&stack);
    let _ = rect(0.0, 0.0, 0.0, 0.0);
    popover_with(&stack)
}
