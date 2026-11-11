//! Text recognition (Vision) and sensitive-data detection for Quick Redact.

use objc2::rc::Retained;
use objc2::{msg_send, AllocAnyThread};
use objc2_core_foundation::{CFRetained, CGRect};
use objc2_core_graphics::CGImage;
use objc2_foundation::{
    NSArray, NSDataDetector, NSDictionary, NSError, NSMatchingOptions, NSRange, NSString,
    NSTextCheckingType,
};
use objc2_vision::*;

use crate::util::{background, on_main, rect, SendBox};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensitive {
    Email,
    Phone,
}

#[derive(Clone, Debug)]
pub struct OcrLine {
    pub text: String,
    /// Image pixels, top-left origin.
    pub rect: CGRect,
    pub sensitive: Vec<(CGRect, Sensitive)>,
}

pub fn recognize(image: CFRetained<CGImage>, done: impl FnOnce(Vec<OcrLine>) + 'static) {
    let done = SendBox(done);
    background(move || {
        let lines = recognize_sync(&image);
        let lines = SendBox(lines);
        on_main(move || (done.into_inner())(lines.into_inner()));
    });
}

fn to_px(r: CGRect, w: f64, h: f64) -> CGRect {
    rect(
        r.origin.x * w,
        (1.0 - r.origin.y - r.size.height) * h,
        r.size.width * w,
        r.size.height * h,
    )
}

pub fn recognize_sync(image: &CGImage) -> Vec<OcrLine> {
    let w = CGImage::width(Some(image)) as f64;
    let h = CGImage::height(Some(image)) as f64;
    let mut out = Vec::new();
    unsafe {
        let request = VNRecognizeTextRequest::new();
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);
        request.setAutomaticallyDetectsLanguage(true);
        let handler = VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &NSDictionary::new(),
        );
        let req: &VNRequest = &request;
        if handler.performRequests_error(&NSArray::from_slice(&[req])).is_err() {
            return out;
        }
        let Some(results) = request.results() else {
            return out;
        };
        let detector = NSDataDetector::dataDetectorWithTypes_error(
            NSTextCheckingType::Link.0 | NSTextCheckingType::PhoneNumber.0,
        )
        .ok();
        for obs in results.iter() {
            let cands = obs.topCandidates(1);
            let Some(cand) = cands.iter().next() else { continue };
            let s: Retained<NSString> = cand.string();
            let line_rect = to_px(obs.boundingBox(), w, h);
            let mut sensitive = Vec::new();
            if let Some(det) = &detector {
                let len = s.length();
                let matches = det.matchesInString_options_range(
                    &s,
                    NSMatchingOptions::empty(),
                    NSRange::new(0, len),
                );
                for m in matches.iter() {
                    let kind = if m.resultType() == NSTextCheckingType::PhoneNumber {
                        Sensitive::Phone
                    } else if m
                        .URL()
                        .and_then(|u| u.scheme())
                        .map(|sc| sc.to_string() == "mailto")
                        .unwrap_or(false)
                    {
                        Sensitive::Email
                    } else {
                        continue;
                    };
                    let range = m.range();
                    let bb: Result<Retained<VNRectangleObservation>, Retained<NSError>> =
                        msg_send![&*cand, boundingBoxForRange: range, error: _];
                    let r = match bb {
                        Ok(o) => to_px(o.boundingBox(), w, h),
                        Err(_) => {
                            // Proportional fallback.
                            let f0 = range.location as f64 / len.max(1) as f64;
                            let f1 = (range.location + range.length) as f64 / len.max(1) as f64;
                            rect(
                                line_rect.origin.x + line_rect.size.width * f0,
                                line_rect.origin.y,
                                line_rect.size.width * (f1 - f0),
                                line_rect.size.height,
                            )
                        }
                    };
                    // Pad a little so glyph edges are fully covered.
                    let pad = (r.size.height * 0.12).max(1.0);
                    sensitive.push((
                        rect(r.origin.x - pad, r.origin.y - pad, r.size.width + 2.0 * pad, r.size.height + 2.0 * pad),
                        kind,
                    ));
                }
            }
            out.push(OcrLine { text: s.to_string(), rect: line_rect, sensitive });
        }
    }
    reading_order(out)
}

/// Sort OCR lines top-to-bottom, then left-to-right within a row.
pub fn reading_order(mut out: Vec<OcrLine>) -> Vec<OcrLine> {
    // Reading order: group lines into rows (by vertical center), then sort by
    // (row, x). Grouping first keeps the comparator a total order — a
    // tolerance-based comparator can make `sort_by` panic.
    out.sort_by(|a, b| center_y(a).total_cmp(&center_y(b)));
    let mut rows: Vec<usize> = Vec::with_capacity(out.len());
    let mut row = 0usize;
    let mut row_y = f64::NEG_INFINITY;
    let mut row_h = 0.0f64;
    for l in &out {
        let cy = center_y(l);
        if (cy - row_y).abs() > row_h.min(l.rect.size.height) * 0.5 {
            if row_y.is_finite() {
                row += 1;
            }
            row_y = cy;
            row_h = l.rect.size.height;
        }
        rows.push(row);
    }
    let mut keyed: Vec<(usize, OcrLine)> = rows.into_iter().zip(out).collect();
    keyed.sort_by(|(ra, a), (rb, b)| ra.cmp(rb).then(a.rect.origin.x.total_cmp(&b.rect.origin.x)));
    let out: Vec<OcrLine> = keyed.into_iter().map(|(_, l)| l).collect();
    out
}

fn center_y(l: &OcrLine) -> f64 {
    l.rect.origin.y + l.rect.size.height / 2.0
}

pub fn join_lines(lines: &[&OcrLine]) -> String {
    lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
}
