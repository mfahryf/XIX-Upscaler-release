//! Port of `fitToBounds` + `syncDimensions` from `src/XIX-VectorizeAI_v2.js`.
//!
//! - `fit_to_bounds` crops the artboard to the artwork's bounding box, then
//!   normalizes the offset to `(0,0)` per the Adobe Stock spec (artboard
//!   offset upper-left corner). The artwork is shifted into the origin via a
//!   `<g transform="translate(...)">` — a pure translation, so it is lossless
//!   and strokes are never scaled.
//! - `sync_dimensions` rewrites the root `<svg>` width/height to ~`target_mp`
//!   megapixels preserving the viewBox aspect ratio, capped at 65MP (Adobe
//!   Stock upper bound).

use crate::svg::path_bbox::path_bbox;
use regex::Regex;

pub const PAD_PCT: f64 = 0.07;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FitMode {
    /// Keep the original viewBox — no cropping.
    None,
    /// Full-bleed: crop to the artwork bbox with a 1px anti-clip margin.
    Fit,
    /// Full-bleed + 7% proportional margin per side (breathing room).
    Pad,
}

/// Some external SVG providers (FreeConvert, Adobe Sensei) emit only
/// `width`/`height` and no `viewBox`. The fit/sync helpers key off viewBox,
/// so synthesize one from the dimensions first.
pub fn ensure_viewbox(svg: &str) -> String {
    if svg.contains("viewBox=") {
        return svg.to_string();
    }
    let w_re = Regex::new(r#"width="([\d.]+)""#).unwrap();
    let h_re = Regex::new(r#"height="([\d.]+)""#).unwrap();
    let w = w_re
        .captures(svg)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "0".into());
    let h = h_re
        .captures(svg)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "0".into());
    let open = Regex::new(r"<svg\b([^>]*)>").unwrap();
    open.replace(svg, |c: &regex::Captures| {
        let attrs = c.get(1).map(|m| m.as_str()).unwrap_or("");
        format!(r#"<svg{attrs} viewBox="0 0 {w} {h}">"#)
    })
    .into_owned()
}

/// Canvas dimensions from viewBox (or width/height fallback) — used to detect
/// full-canvas background paths that vectorizers sometimes add over transparent
/// input (e.g. Adobe Sensei fills the alpha with a solid color rect).
fn canvas_dims(svg: &str) -> Option<(f64, f64)> {
    let vb = Regex::new(r#"viewBox="([-\d.]+)\s+([-\d.]+)\s+([\d.]+)\s+([\d.]+)""#)
        .unwrap()
        .captures(svg)
        .map(|c| {
            (
                c[3].parse::<f64>().unwrap_or(0.0),
                c[4].parse::<f64>().unwrap_or(0.0),
            )
        });
    if let Some((w, h)) = vb {
        if w > 0.0 && h > 0.0 {
            return Some((w, h));
        }
    }
    let w = Regex::new(r#"width="([\d.]+)""#)
        .unwrap()
        .captures(svg)
        .map(|c| c[1].parse::<f64>().unwrap_or(0.0))
        .unwrap_or(0.0);
    let h = Regex::new(r#"height="([\d.]+)""#)
        .unwrap()
        .captures(svg)
        .map(|c| c[1].parse::<f64>().unwrap_or(0.0))
        .unwrap_or(0.0);
    if w > 0.0 && h > 0.0 {
        Some((w, h))
    } else {
        None
    }
}

struct PathGeom {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    fill: Option<String>,
}

/// Parse every `<path ...>` tag, extracting `d` and `transform="translate(...)"`
/// from anywhere in the tag (FreeConvert puts transform *after* `d=`; the old
/// regex only looked before `d=` and silently ignored those transforms).
fn parse_paths(svg: &str) -> Vec<PathGeom> {
    let tag_re = Regex::new(r"<path\b([^>]*)>").unwrap();
    let d_re = Regex::new(r##"\bd="([^"]*)""##).unwrap();
    let tr_re =
        Regex::new(r##"transform="translate\(\s*(-?[\d.]+)[\s,]+(-?[\d.]+)\s*\)"##).unwrap();
    let fill_re = Regex::new(r##"\bfill="([^"]*)""##).unwrap();
    let mut out = Vec::new();
    for cap in tag_re.captures_iter(svg) {
        let tag = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let Some(d) = d_re.captures(tag).map(|c| c[1].to_string()) else {
            continue;
        };
        let (mut tx, mut ty) = (0.0, 0.0);
        if let Some(tr) = tr_re.captures(tag) {
            tx = tr[1].parse().unwrap_or(0.0);
            ty = tr[2].parse().unwrap_or(0.0);
        }
        let fill = fill_re.captures(tag).map(|c| c[1].to_string());
        if let Some(bb) = path_bbox(&d) {
            out.push(PathGeom {
                min_x: bb.min_x + tx,
                min_y: bb.min_y + ty,
                max_x: bb.max_x + tx,
                max_y: bb.max_y + ty,
                fill,
            });
        }
    }
    out
}

/// Fit the artboard to the artwork bounds, then shift the artwork into the
/// origin so the artboard offset is `(0,0)` upper-left. Returns the input
/// unchanged for `FitMode::None` or when no path coordinates are found.
///
/// Full-canvas background paths are excluded from the bbox: Adobe Sensei (and
/// friends) composite transparent input over a solid rect, and that rect would
/// make the bbox equal the whole canvas (no crop). A path counts as background
/// when its bbox covers ≥ 95% of the canvas AND touches all four viewBox
/// corners (within 2% of the longer side) — artwork that merely fills the
/// frame has margins on at least one side and is kept.
pub fn fit_to_bounds(svg: &str, mode: FitMode) -> String {
    if mode == FitMode::None {
        return svg.to_string();
    }
    let paths = parse_paths(svg);
    if paths.is_empty() {
        return svg.to_string();
    }

    let canvas = canvas_dims(svg);
    let mut geoms: Vec<&PathGeom> = paths.iter().collect();
    if let Some((cw, ch)) = canvas {
        let canvas_area = cw * ch;
        let tol = cw.max(ch) * 0.02;
        let bg_fills: Vec<String> = geoms
            .iter()
            .filter(|p| {
                let w = p.max_x - p.min_x;
                let h = p.max_y - p.min_y;
                w * h >= canvas_area * 0.95
                    && p.min_x <= tol
                    && p.min_y <= tol
                    && (cw - p.max_x).abs() <= tol
                    && (ch - p.max_y).abs() <= tol
            })
            .filter_map(|p| p.fill.clone())
            .collect();
        if !bg_fills.is_empty() {
            // Drop the full-canvas rects AND their anti-aliased edge fragments
            // (same fill, scattered at the borders) so stray corner pixels don't
            // re-inflate the bbox to the whole canvas.
            geoms.retain(|p| {
                p.fill
                    .as_ref()
                    .map(|f| !bg_fills.contains(f))
                    .unwrap_or(true)
            });
        }
    }
    if geoms.is_empty() {
        return svg.to_string();
    }

    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in &geoms {
        if p.min_x < min_x {
            min_x = p.min_x;
        }
        if p.min_y < min_y {
            min_y = p.min_y;
        }
        if p.max_x > max_x {
            max_x = p.max_x;
        }
        if p.max_y > max_y {
            max_y = p.max_y;
        }
    }
    if !min_x.is_finite() {
        return svg.to_string(); // no coordinates found — leave untouched
    }

    let art_w = max_x - min_x;
    let art_h = max_y - min_y;
    let (pad_x, pad_y) = if mode == FitMode::Pad {
        (art_w * PAD_PCT, art_h * PAD_PCT)
    } else {
        (0.0, 0.0)
    };
    let x = (min_x - pad_x).floor() as i64 - 1;
    let y = (min_y - pad_y).floor() as i64 - 1;
    let w = (art_w + pad_x * 2.0).ceil() as i64 + 2;
    let h = (art_h + pad_y * 2.0).ceil() as i64 + 2;
    if w <= 0 || h <= 0 {
        return svg.to_string();
    }

    // Adobe Stock spec: artboard offset must be (0,0) upper-left. A raw
    // viewBox="x y w h" with x/y != 0 violates that, so shift the artwork into
    // the origin with a <g translate> (pure translation = lossless, strokes
    // unscaled) and reset viewBox to 0 0 w h.
    let vb_re = Regex::new(r#"viewBox="[^"]*""#).unwrap();
    let mut out = vb_re
        .replacen(svg, 1, format!(r#"viewBox="0 0 {w} {h}""#))
        .into_owned();
    let svg_open = Regex::new(r"<svg\b([^>]*)>").unwrap();
    if let Some(m) = svg_open.find(&out) {
        let end = m.end();
        out = format!(
            "{}\n<g transform=\"translate({} {})\">{}",
            &out[..end],
            -x,
            -y,
            &out[end..]
        );
    }
    if let Some(pos) = out.find("</svg>") {
        out = format!(
            "{}</g>\n</svg>{}",
            &out[..pos],
            &out[pos + "</svg>".len()..]
        );
    }
    out
}

/// Rewrite the root `<svg>` width/height to ~`target_mp` megapixels while
/// preserving the viewBox aspect ratio. Capped at 65MP (Adobe Stock max).
pub fn sync_dimensions(svg: &str, target_mp: f64) -> String {
    let vb_re = Regex::new(r#"viewBox="([-\d.]+)\s+([-\d.]+)\s+([\d.]+)\s+([\d.]+)""#).unwrap();
    let Some(caps) = vb_re.captures(svg) else {
        return svg.to_string();
    };
    let vw: f64 = caps[3].parse().unwrap_or(0.0);
    let vh: f64 = caps[4].parse().unwrap_or(0.0);
    if vw == 0.0 || vh == 0.0 {
        return svg.to_string();
    }
    let ratio = vw / vh;
    let mut w = (target_mp * 1e6 * ratio).sqrt().round() as u64;
    let mut h = (w as f64 / ratio).round() as u64;
    if (w as f64 * h as f64) / 1e6 > 65.0 {
        let k = (65e6 / (w as f64 * h as f64)).sqrt();
        w = (w as f64 * k).round() as u64;
        h = (h as f64 * k).round() as u64;
    }
    let svg_open = Regex::new(r"<svg\b([^>]*)>").unwrap();
    svg_open
        .replace(svg, |c: &regex::Captures| {
            let attrs = c.get(1).map(|m| m.as_str()).unwrap_or("");
            // Match width/height whatever their value ("100%", px, missing)
            // and replace or insert them — Adobe Sensei emits width="100%"
            // with no height attribute at all.
            let w_re = Regex::new(r#"\swidth="[^"]*""#).unwrap();
            let a = if w_re.is_match(attrs) {
                w_re.replacen(attrs, 1, format!(r#" width="{w}""#))
                    .into_owned()
            } else {
                format!("{attrs} width=\"{w}\"")
            };
            let h_re = Regex::new(r#"\sheight="[^"]*""#).unwrap();
            let a = if h_re.is_match(&a) {
                h_re.replacen(&a, 1, format!(r#" height="{h}""#))
                    .into_owned()
            } else {
                format!("{a} height=\"{h}\"")
            };
            format!("<svg{a}>")
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELATIVE_SVG: &str = r##"<svg width="1200" height="896" viewBox="0 0 4800 3584"><path fill="#000" d="M2343.5 681.25 c-29.58 .52 -51.14 1.28 -51.33 1.82 l-10 20 z"/><path fill="#000" d="M917.61 421.33 c10 5 20 5 30 0 l2936 5 l0 2000 l-2966 0 z"/></svg>"##;

    fn vb(svg: &str) -> (f64, f64, f64, f64) {
        let re = Regex::new(r#"viewBox="([-\d.]+) ([\d.]+) ([\d.]+) ([\d.]+)""#).unwrap();
        let c = re.captures(svg).expect("viewBox present");
        (
            c[1].parse().unwrap(),
            c[2].parse().unwrap(),
            c[3].parse().unwrap(),
            c[4].parse().unwrap(),
        )
    }

    #[test]
    fn fit_bleed_crops_and_normalizes_origin() {
        let out = fit_to_bounds(RELATIVE_SVG, FitMode::Fit);
        let (x, y, w, h) = vb(&out);
        assert_eq!((x, y), (0.0, 0.0)); // Adobe Stock (0,0) offset
        assert_eq!((w, h), (2968.0, 2007.0)); // artwork bbox + 1px anti-clip each side
        assert!(out.contains(r#"<g transform="translate(-916 -420)">"#));
        assert!(out.contains("</g>\n</svg>"));
    }

    #[test]
    fn fit_pad_adds_7_percent_margin() {
        // artW=2966, artH=2005 → pad 207.62/140.35 → viewBox 0 0 3384 2288
        let out = fit_to_bounds(RELATIVE_SVG, FitMode::Pad);
        let (x, y, w, h) = vb(&out);
        assert_eq!((x, y), (0.0, 0.0));
        assert_eq!((w, h), (3384.0, 2288.0));
        assert!(out.contains(r#"<g transform="translate(-708 -279)">"#));
    }

    #[test]
    fn fit_none_keeps_original_viewbox() {
        assert!(fit_to_bounds(RELATIVE_SVG, FitMode::None).contains(r#"viewBox="0 0 4800 3584""#));
    }

    #[test]
    fn sync_dimensions_targets_25mp_preserving_aspect() {
        let svg = r#"<svg width="1200" height="896" viewBox="0 0 3384 3079"><path d="M0 0 l10 0 l0 10 z"/></svg>"#;
        let out = sync_dimensions(svg, 25.0);
        let re = Regex::new(r#"width="(\d+)" height="(\d+)""#).unwrap();
        let c = re.captures(&out).expect("width/height rewritten");
        let w: f64 = c[1].parse().unwrap();
        let h: f64 = c[2].parse().unwrap();
        let mp = w * h / 1e6;
        assert!(
            (15.0..=65.0).contains(&mp),
            "25MP target within 15-65MP, got {mp}"
        );
        assert!((w / h - 3384.0 / 3079.0).abs() < 0.01, "aspect preserved");
    }

    #[test]
    fn sync_dimensions_caps_at_65mp() {
        // extreme aspect ratio would blow past the 65MP cap without the clamp
        let svg = r#"<svg width="1200" height="896" viewBox="0 0 5000 500"><path d="M0 0 l10 0 l0 10 z"/></svg>"#;
        let out = sync_dimensions(svg, 25.0);
        let re = Regex::new(r#"width="(\d+)" height="(\d+)""#).unwrap();
        let c = re.captures(&out).expect("width/height rewritten");
        let w: f64 = c[1].parse().unwrap();
        let h: f64 = c[2].parse().unwrap();
        let mp = w * h / 1e6;
        assert!(mp <= 65.0, "capped at 65MP, got {mp}");
    }

    #[test]
    fn fit_honors_transform_after_d_attr() {
        // FreeConvert emits transform AFTER d= — the old regex only looked
        // before d= and computed a garbage bbox (artwork at negative coords).
        // ensure_viewbox runs first in the real pipeline, so the input here
        // already has the synthesized viewBox.
        let svg = r##"<svg width="1200" height="896" viewBox="0 0 1200 896"><path fill="#2A2C31" d="M0 0 l100 0 l0 80 l-100 0 z" transform="translate(200,150)"/><path fill="#F8DC76" d="M0 0 l60 0 l0 40 l-60 0 z" transform="translate(700,600)"/></svg>"##;
        let out = fit_to_bounds(svg, FitMode::Fit);
        let (x, y, w, h) = vb(&out);
        assert_eq!((x, y), (0.0, 0.0));
        assert_eq!((w, h), (562.0, 492.0)); // bbox (200,150)-(760,640) + 1px margin
        assert!(out.contains(r#"<g transform="translate(-199 -149)">"#));
    }

    #[test]
    fn fit_skips_full_canvas_background_path() {
        // Adobe composites transparent input over a solid rect; that rect must
        // not inflate the bbox to the whole canvas (no crop).
        let svg = r##"<svg width="100%" viewBox="0 0 1202 898"><path fill="#7E94DD" d="M1 1 l1200 0 l0 896 l-1200 0 z"/><path fill="#2A2C31" d="M300 200 l400 0 l0 300 l-400 0 z"/></svg>"##;
        let out = fit_to_bounds(svg, FitMode::Fit);
        let (x, y, w, h) = vb(&out);
        assert_eq!((x, y), (0.0, 0.0));
        assert_eq!((w, h), (402.0, 302.0)); // crop ke artwork (300,200)-(700,500)
        assert!(out.contains(r#"<g transform="translate(-299 -199)">"#));
    }

    #[test]
    fn fit_keeps_artwork_that_fills_frame_with_margins() {
        // PngToSvg output: artwork touches most of the canvas but NOT the
        // corners (margins all around) — must NOT be treated as background.
        let svg = r##"<svg width="758" height="713" viewBox="0 0 758 713"><path fill="#a4b7f1" d="M219 91 l756 0 l0 711 l-756 0 z"/></svg>"##;
        let out = fit_to_bounds(svg, FitMode::Fit);
        let (x, y, w, h) = vb(&out);
        assert_eq!((x, y), (0.0, 0.0));
        assert_eq!((w, h), (758.0, 713.0)); // masih crop ke bbox artwork yang sama
        assert!(out.contains(r#"<g transform="translate(-218 -90)">"#));
    }

    #[test]
    fn sync_dimensions_handles_width_100pct_and_missing_height() {
        // Adobe raw: width="100%", no height attr — must be rewritten + inserted.
        let svg = r#"<svg version="1.1" width="100%" viewBox="0 0 1202 898" enable-background="new 0 0 1200 896"><path d="M1 1 l10 0 l0 10 z"/></svg>"#;
        let out = sync_dimensions(svg, 25.0);
        let w_re = Regex::new(r#"width="(\d+)""#).unwrap();
        let h_re = Regex::new(r#"height="(\d+)""#).unwrap();
        let cw = w_re.captures(&out).expect("width numeric after sync");
        let ch = h_re.captures(&out).expect("height inserted after sync");
        let w: f64 = cw[1].parse().unwrap();
        let h: f64 = ch[1].parse().unwrap();
        let mp = w * h / 1e6;
        assert!((15.0..=65.0).contains(&mp), "25MP within 15-65MP, got {mp}");
        assert!(!out.contains("100%"), "width rewritten, no leftover 100%");
    }
}
