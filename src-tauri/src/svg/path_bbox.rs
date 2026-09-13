//! Port of the JS `pathBBox` parser from `src/XIX-VectorizeAI_v2.js`.
//!
//! svg.new emits RELATIVE path commands (lowercase `m`/`c`/`l`/`z`) where the
//! numbers are deltas from the pen position, not absolute coordinates. A naive
//! "all numbers are coordinate pairs" scan therefore computes a garbage bbox
//! for that data. This parser tracks the pen position and honors both absolute
//! and relative commands, so the bbox (used for full-bleed artboard fitting)
//! is correct for both svg.new and svgai.org output.
//!
//! Semantics match the JS exactly (verified by the same test vectors):
//! - relative coords add to the current pen, absolute coords replace it
//! - `M` sets the subpath start; extra coordinate pairs after `M` act as `L`
//! - `Z` returns the pen to the subpath start
//! - control points are included so the bbox never clips the artwork
//! - stray numbers before any command are skipped (not fatal)

use regex::Regex;
use std::sync::OnceLock;

pub struct BBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

const PATH_ARG_COUNT: &[(char, usize)] = &[
    ('M', 2),
    ('L', 2),
    ('H', 1),
    ('V', 1),
    ('C', 6),
    ('S', 4),
    ('Q', 4),
    ('T', 2),
    ('A', 7),
    ('Z', 0),
];

fn arg_count(cmd: char) -> Option<usize> {
    PATH_ARG_COUNT
        .iter()
        .find(|(c, _)| *c == cmd.to_ascii_uppercase())
        .map(|(_, n)| *n)
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-zA-Z]|-?\d*\.?\d+(?:[eE][-+]?\d+)?").unwrap())
}

struct Acc {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Acc {
    fn new() -> Self {
        Acc {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
        }
    }

    fn add(&mut self, x: f64, y: f64) {
        if x < self.min_x {
            self.min_x = x;
        }
        if x > self.max_x {
            self.max_x = x;
        }
        if y < self.min_y {
            self.min_y = y;
        }
        if y > self.max_y {
            self.max_y = y;
        }
    }
}

/// Compute the bounding box of a single `d` path string, resolving relative
/// commands against the running pen position. Returns `None` when the path
/// contains no coordinates.
pub fn path_bbox(d: &str) -> Option<BBox> {
    let toks: Vec<&str> = token_re().find_iter(d).map(|m| m.as_str()).collect();
    let mut acc = Acc::new();
    let (mut cx, mut cy) = (0.0f64, 0.0f64); // pen position
    let (mut sx, mut sy) = (0.0f64, 0.0f64); // subpath start
    let mut cmd: Option<char> = None;
    let mut i = 0usize;

    while i < toks.len() {
        let t = toks[i];
        let c = t.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            cmd = Some(c);
            i += 1;
            continue;
        }
        // Stray number before any command — skip (JS semantics), not fatal.
        let cmd_c = match cmd {
            Some(c) => c,
            None => {
                i += 1;
                continue;
            }
        };
        let rel = cmd_c.is_ascii_lowercase();
        let n = match arg_count(cmd_c) {
            Some(n) => n,
            None => {
                // Unknown command letter — skip the number, avoid a hang.
                i += 1;
                continue;
            }
        };
        if n == 0 {
            // Z: pen back to subpath start.
            cx = sx;
            cy = sy;
            cmd = None;
            continue;
        }
        // Gather up to n numbers (stop at the next command letter).
        let mut args: Vec<f64> = Vec::with_capacity(n);
        while args.len() < n && i < toks.len() {
            let tt = toks[i];
            if tt.chars().next().unwrap().is_ascii_alphabetic() {
                break;
            }
            match tt.parse::<f64>() {
                Ok(v) => args.push(v),
                Err(_) => break,
            }
            i += 1;
        }
        if args.len() < n {
            break; // truncated data — bail on this path
        }
        let x = |k: usize| args[k] + if rel { cx } else { 0.0 };
        let y = |k: usize| args[k + 1] + if rel { cy } else { 0.0 };
        match cmd_c.to_ascii_uppercase() {
            // Extra pairs after M act as lineto.
            'M' => {
                cx = x(0);
                cy = y(0);
                sx = cx;
                sy = cy;
                acc.add(cx, cy);
                cmd = Some(if rel { 'l' } else { 'L' });
            }
            'L' => {
                cx = x(0);
                cy = y(0);
                acc.add(cx, cy);
            }
            'H' => {
                cx = args[0] + if rel { cx } else { 0.0 };
                acc.add(cx, cy);
            }
            'V' => {
                cy = args[0] + if rel { cy } else { 0.0 };
                acc.add(cx, cy);
            }
            'C' => {
                acc.add(x(0), y(0));
                acc.add(x(2), y(2));
                cx = x(4);
                cy = y(4);
                acc.add(cx, cy);
            }
            'S' | 'Q' => {
                acc.add(x(0), y(0));
                cx = x(2);
                cy = y(2);
                acc.add(cx, cy);
            }
            'T' => {
                cx = x(0);
                cy = y(0);
                acc.add(cx, cy);
            }
            'A' => {
                acc.add(cx, cy);
                cx = args[5] + if rel { cx } else { 0.0 };
                cy = args[6] + if rel { cy } else { 0.0 };
                acc.add(cx, cy);
            }
            _ => {}
        }
    }

    if acc.min_x.is_finite() {
        Some(BBox {
            min_x: acc.min_x,
            min_y: acc.min_y,
            max_x: acc.max_x,
            max_y: acc.max_y,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bb(d: &str) -> (f64, f64, f64, f64) {
        let b = path_bbox(d).unwrap();
        (b.min_x, b.min_y, b.max_x, b.max_y)
    }

    #[test]
    fn resolves_relative_lineto() {
        assert_eq!(bb("M10 10 l5 5 z"), (10.0, 10.0, 15.0, 15.0));
    }

    #[test]
    fn handles_absolute_commands() {
        assert_eq!(bb("M10 10 L20 10 L20 20 Z"), (10.0, 10.0, 20.0, 20.0));
    }

    #[test]
    fn resolves_relative_cubic_control_points_included() {
        // m10 10 c 10 0 10 10 20 10 → endpoint (30,20), controls (20,10),(20,20)
        assert_eq!(bb("m10 10 c 10 0 10 10 20 10"), (10.0, 10.0, 30.0, 20.0));
    }

    #[test]
    fn tracks_subpath_start_through_z() {
        assert_eq!(bb("M0 0 l10 0 l0 10 z l-5 -5"), (0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn handles_hv_single_axis() {
        assert_eq!(bb("M0 0 H10 V10 H0 Z"), (0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn returns_none_when_no_coords() {
        assert!(path_bbox("Z Z Z").is_none());
    }
}
