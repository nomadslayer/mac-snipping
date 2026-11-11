//! Annotation model (ink, shapes, redactions) and rendering.
//!
//! All coordinates are image pixels with a top-left origin. Rendering
//! functions expect a CGContext whose CTM already maps that space.

use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::*;

use crate::util::{rect, rect_contains, rect_from_points, Rgba};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeKind {
    Rect,
    Ellipse,
    Line,
    Arrow,
}

impl ShapeKind {
    pub const ALL: [ShapeKind; 4] = [ShapeKind::Rect, ShapeKind::Ellipse, ShapeKind::Line, ShapeKind::Arrow];
    pub fn from_index(i: u32) -> ShapeKind {
        *Self::ALL.get(i as usize).unwrap_or(&ShapeKind::Rect)
    }
    pub fn index(self) -> u32 {
        Self::ALL.iter().position(|k| *k == self).unwrap_or(0) as u32
    }
    pub fn title(self) -> &'static str {
        match self {
            ShapeKind::Rect => "Rectangle",
            ShapeKind::Ellipse => "Oval",
            ShapeKind::Line => "Line",
            ShapeKind::Arrow => "Arrow",
        }
    }
    pub fn symbol(self) -> &'static str {
        match self {
            ShapeKind::Rect => "rectangle",
            ShapeKind::Ellipse => "circle",
            ShapeKind::Line => "line.diagonal",
            ShapeKind::Arrow => "arrow.up.right",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Item {
    Ink {
        highlighter: bool,
        color: Rgba,
        width: f64,
        pts: Vec<CGPoint>,
    },
    Shape {
        kind: ShapeKind,
        a: CGPoint,
        b: CGPoint,
        stroke: Rgba,
        fill: Option<Rgba>,
        width: f64,
    },
    Redact {
        r: CGRect,
    },
}

pub fn pt(x: f64, y: f64) -> CGPoint {
    CGPoint::new(x, y)
}

fn sub(a: CGPoint, b: CGPoint) -> CGPoint {
    pt(a.x - b.x, a.y - b.y)
}

fn len(a: CGPoint) -> f64 {
    (a.x * a.x + a.y * a.y).sqrt()
}

pub fn dist(a: CGPoint, b: CGPoint) -> f64 {
    len(sub(a, b))
}

pub fn dist_to_segment(p: CGPoint, a: CGPoint, b: CGPoint) -> f64 {
    let ab = sub(b, a);
    let l2 = ab.x * ab.x + ab.y * ab.y;
    if l2 < 1e-9 {
        return dist(p, a);
    }
    let t = (((p.x - a.x) * ab.x + (p.y - a.y) * ab.y) / l2).clamp(0.0, 1.0);
    dist(p, pt(a.x + ab.x * t, a.y + ab.y * t))
}

impl Item {
    #[allow(dead_code)]
    pub fn bounds(&self) -> CGRect {
        match self {
            Item::Ink { width, pts, .. } => {
                let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for p in pts {
                    x0 = x0.min(p.x);
                    y0 = y0.min(p.y);
                    x1 = x1.max(p.x);
                    y1 = y1.max(p.y);
                }
                if pts.is_empty() {
                    return rect(0.0, 0.0, 0.0, 0.0);
                }
                let w = width / 2.0 + 2.0;
                rect(x0 - w, y0 - w, x1 - x0 + 2.0 * w, y1 - y0 + 2.0 * w)
            }
            Item::Shape { kind, a, b, width, .. } => {
                let r = rect_from_points(*a, *b);
                let pad = match kind {
                    ShapeKind::Arrow => arrow_head_len(*width) + width,
                    _ => width / 2.0 + 2.0,
                };
                rect(r.origin.x - pad, r.origin.y - pad, r.size.width + 2.0 * pad, r.size.height + 2.0 * pad)
            }
            Item::Redact { r } => *r,
        }
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        match self {
            Item::Ink { pts, .. } => {
                for p in pts.iter_mut() {
                    p.x += dx;
                    p.y += dy;
                }
            }
            Item::Shape { a, b, .. } => {
                a.x += dx;
                a.y += dy;
                b.x += dx;
                b.y += dy;
            }
            Item::Redact { r } => {
                r.origin.x += dx;
                r.origin.y += dy;
            }
        }
    }

    /// Does `p` touch this item, within `tol` pixels?
    pub fn hit(&self, p: CGPoint, tol: f64) -> bool {
        match self {
            Item::Ink { width, pts, .. } => {
                let t = tol + width / 2.0;
                if pts.len() == 1 {
                    return dist(p, pts[0]) <= t;
                }
                pts.windows(2).any(|w| dist_to_segment(p, w[0], w[1]) <= t)
            }
            Item::Shape { kind, a, b, fill, width, .. } => {
                let t = tol + width / 2.0;
                match kind {
                    ShapeKind::Line | ShapeKind::Arrow => dist_to_segment(p, *a, *b) <= t,
                    ShapeKind::Rect => {
                        let r = rect_from_points(*a, *b);
                        let (x0, y0) = (r.origin.x, r.origin.y);
                        let (x1, y1) = (x0 + r.size.width, y0 + r.size.height);
                        if fill.is_some() && rect_contains(r, p) {
                            return true;
                        }
                        dist_to_segment(p, pt(x0, y0), pt(x1, y0)) <= t
                            || dist_to_segment(p, pt(x1, y0), pt(x1, y1)) <= t
                            || dist_to_segment(p, pt(x1, y1), pt(x0, y1)) <= t
                            || dist_to_segment(p, pt(x0, y1), pt(x0, y0)) <= t
                    }
                    ShapeKind::Ellipse => {
                        let r = rect_from_points(*a, *b);
                        let rx = (r.size.width / 2.0).max(1.0);
                        let ry = (r.size.height / 2.0).max(1.0);
                        let cx = r.origin.x + rx;
                        let cy = r.origin.y + ry;
                        let d = (((p.x - cx) / rx).powi(2) + ((p.y - cy) / ry).powi(2)).sqrt();
                        if fill.is_some() && d <= 1.0 {
                            return true;
                        }
                        (d - 1.0).abs() * rx.min(ry) <= t
                    }
                }
            }
            Item::Redact { r } => rect_contains(*r, p),
        }
    }
}

pub fn arrow_head_len(width: f64) -> f64 {
    (width * 3.2).max(12.0)
}

/// Build a smoothed path through the stroke points (quadratic midpoints).
fn add_smooth_path(ctx: &CGContext, pts: &[CGPoint]) {
    let c = Some(ctx);
    CGContext::begin_path(c);
    CGContext::move_to_point(c, pts[0].x, pts[0].y);
    if pts.len() == 2 {
        CGContext::add_line_to_point(c, pts[1].x, pts[1].y);
        return;
    }
    for i in 1..pts.len() - 1 {
        let p = pts[i];
        let n = pts[i + 1];
        let mid = pt((p.x + n.x) / 2.0, (p.y + n.y) / 2.0);
        CGContext::add_quad_curve_to_point(c, p.x, p.y, mid.x, mid.y);
    }
    let last = pts[pts.len() - 1];
    CGContext::add_line_to_point(c, last.x, last.y);
}

pub fn draw_item(ctx: &CGContext, item: &Item) {
    let c = Some(ctx);
    CGContext::save_g_state(c);
    CGContext::set_line_cap(c, CGLineCap::Round);
    CGContext::set_line_join(c, CGLineJoin::Round);
    match item {
        Item::Ink { highlighter, color, width, pts } => {
            if pts.is_empty() {
                CGContext::restore_g_state(c);
                return;
            }
            if *highlighter {
                CGContext::set_blend_mode(c, CGBlendMode::Multiply);
            }
            let cg = color.cg();
            if pts.len() == 1 {
                CGContext::set_fill_color_with_color(c, Some(&cg));
                let r = width / 2.0;
                CGContext::fill_ellipse_in_rect(c, rect(pts[0].x - r, pts[0].y - r, 2.0 * r, 2.0 * r));
            } else {
                CGContext::set_stroke_color_with_color(c, Some(&cg));
                CGContext::set_line_width(c, *width);
                if *highlighter {
                    CGContext::set_line_cap(c, CGLineCap::Square);
                }
                add_smooth_path(ctx, pts);
                CGContext::stroke_path(c);
            }
        }
        Item::Shape { kind, a, b, stroke, fill, width } => {
            let sc = stroke.cg();
            CGContext::set_stroke_color_with_color(c, Some(&sc));
            CGContext::set_line_width(c, *width);
            let r = rect_from_points(*a, *b);
            match kind {
                ShapeKind::Rect => {
                    CGContext::set_line_join(c, CGLineJoin::Miter);
                    if let Some(f) = fill {
                        CGContext::set_fill_color_with_color(c, Some(&f.cg()));
                        CGContext::fill_rect(c, r);
                    }
                    CGContext::stroke_rect(c, r);
                }
                ShapeKind::Ellipse => {
                    if let Some(f) = fill {
                        CGContext::set_fill_color_with_color(c, Some(&f.cg()));
                        CGContext::fill_ellipse_in_rect(c, r);
                    }
                    CGContext::stroke_ellipse_in_rect(c, r);
                }
                ShapeKind::Line => {
                    CGContext::begin_path(c);
                    CGContext::move_to_point(c, a.x, a.y);
                    CGContext::add_line_to_point(c, b.x, b.y);
                    CGContext::stroke_path(c);
                }
                ShapeKind::Arrow => {
                    let d = sub(*b, *a);
                    let l = len(d);
                    if l < 0.5 {
                        CGContext::restore_g_state(c);
                        return;
                    }
                    let dir = pt(d.x / l, d.y / l);
                    let head = arrow_head_len(*width).min(l);
                    let base = pt(b.x - dir.x * head, b.y - dir.y * head);
                    let perp = pt(-dir.y, dir.x);
                    let hw = head * 0.55;
                    CGContext::begin_path(c);
                    CGContext::move_to_point(c, a.x, a.y);
                    CGContext::add_line_to_point(c, base.x + dir.x * 1.0, base.y + dir.y * 1.0);
                    CGContext::stroke_path(c);
                    CGContext::set_fill_color_with_color(c, Some(&sc));
                    CGContext::begin_path(c);
                    CGContext::move_to_point(c, b.x, b.y);
                    CGContext::add_line_to_point(c, base.x + perp.x * hw, base.y + perp.y * hw);
                    CGContext::add_line_to_point(c, base.x - perp.x * hw, base.y - perp.y * hw);
                    CGContext::close_path(c);
                    CGContext::fill_path(c);
                }
            }
        }
        Item::Redact { r } => {
            CGContext::set_rgb_fill_color(c, 0.0, 0.0, 0.0, 1.0);
            CGContext::fill_rect(c, *r);
        }
    }
    CGContext::restore_g_state(c);
}

pub fn draw_items(ctx: &CGContext, items: &[Item]) {
    for it in items {
        draw_item(ctx, it);
    }
}

/// Draw `image` filling `(0,0,w,h)` in a top-left-origin (flipped) context.
pub fn draw_image_flipped(ctx: &CGContext, image: &CGImage, w: f64, h: f64) {
    let c = Some(ctx);
    CGContext::save_g_state(c);
    CGContext::translate_ctm(c, 0.0, h);
    CGContext::scale_ctm(c, 1.0, -1.0);
    CGContext::draw_image(c, rect(0.0, 0.0, w, h), Some(image));
    CGContext::restore_g_state(c);
}
