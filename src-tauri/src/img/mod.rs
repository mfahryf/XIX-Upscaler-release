//! Pure-local image ops on top of the `image` crate — the sharp equivalents
//! of `src/XIX-Upscaler.js` (alpha detection, lanczos resize, alpha restore,
//! full-bleed crop, pad extend). Used by the upscale engines; no network.
//!
//! Everything works on decoded `DynamicImage`s so callers can chain ops, then
//! encode once.

use image::{DynamicImage, GenericImageView, GrayImage, ImageFormat, Rgba, RgbaImage};

/// True if PNG bytes carry transparency (color type 4/6, or palette + tRNS).
pub fn has_alpha(png: &[u8]) -> bool {
    if png.len() < 30 || !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    match png[25] {
        4 | 6 => true,
        3 => {
            let mut off = 8usize;
            while off + 8 <= png.len() {
                let len = u32::from_be_bytes(png[off..off + 4].try_into().unwrap()) as usize;
                let typ = &png[off + 4..off + 8];
                if typ == b"tRNS" {
                    return true;
                }
                if typ == b"IEND" {
                    break;
                }
                off += 12 + len;
            }
            false
        }
        _ => false,
    }
}

pub fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Encode as JPEG (RGB — alpha dropped) at `quality` (0–100).
pub fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    enc.encode_image(&rgb).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Rebuild the alpha channel of `output` from `input`'s own alpha (resized to
/// the output dims, threshold ≥128). PhotoRoom composites onto an opaque
/// background, so this is how PNG transparency survives an AI upscale.
pub fn restore_alpha(input: &DynamicImage, output: &DynamicImage) -> RgbaImage {
    let (ow, oh) = output.dimensions();
    let src = input.to_rgba8();
    let alpha_src = GrayImage::from_fn(src.width(), src.height(), |x, y| {
        image::Luma([src.get_pixel(x, y)[3]])
    });
    let alpha = image::imageops::resize(&alpha_src, ow, oh, image::imageops::FilterType::Lanczos3);
    let rgb = output.to_rgb8();
    let mut out = RgbaImage::new(ow, oh);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let r = rgb.get_pixel(x, y);
        let a = alpha.get_pixel(x, y);
        *px = Rgba([r[0], r[1], r[2], if a[0] >= 128 { 255 } else { 0 }]);
    }
    out
}

/// Crop to the bounding box of opaque pixels (full-bleed). `None` when the
/// image is fully transparent.
pub fn full_bleed(img: &DynamicImage) -> Option<RgbaImage> {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut found = false;
    for (x, y, px) in rgba.enumerate_pixels() {
        if px[3] > 0 {
            found = true;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    }
    if !found {
        return None;
    }
    Some(
        image::imageops::crop_imm(&rgba, min_x, min_y, max_x - min_x + 1, max_y - min_y + 1)
            .to_image(),
    )
}

/// Extend the canvas by `pct` of each dimension on every side (7% default).
/// Transparent padding for PNG (alpha preserved), white for opaque/JPG.
pub fn extend_pad(img: &DynamicImage, pct: f64, transparent: bool) -> RgbaImage {
    let (w, h) = img.dimensions();
    let pad_x = ((w as f64) * pct).round() as u32;
    let pad_y = ((h as f64) * pct).round() as u32;
    let bg = if transparent {
        Rgba([0, 0, 0, 0])
    } else {
        Rgba([255, 255, 255, 255])
    };
    let mut out = RgbaImage::from_pixel(w + 2 * pad_x, h + 2 * pad_y, bg);
    image::imageops::overlay(&mut out, &img.to_rgba8(), pad_x as i64, pad_y as i64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_rgba(pixels: [[u8; 4]; 4]) -> Vec<u8> {
        let img = RgbaImage::from_fn(2, 2, |x, y| Rgba(pixels[(y * 2 + x) as usize]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn has_alpha_detects_rgba_and_palette_trns() {
        // color type 6 (RGBA)
        let rgba = tiny_rgba([[255, 0, 0, 255]; 4]);
        assert!(has_alpha(&rgba));
        // color type 2 (RGB) → no alpha
        let rgb = image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(rgb)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        assert!(!has_alpha(&buf));
        assert!(!has_alpha(b"not a png"));
    }

    #[test]
    fn restore_alpha_thresholds_mask() {
        let input = DynamicImage::ImageRgba8(RgbaImage::from_fn(2, 2, |x, _| {
            Rgba([10, 20, 30, if x == 0 { 255 } else { 0 }])
        }));
        // output: opaque everywhere
        let output = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([9, 9, 9, 255])));
        let out = restore_alpha(&input, &output);
        assert_eq!(out.get_pixel(0, 0)[3], 255);
        assert_eq!(out.get_pixel(1, 0)[3], 0);
    }

    #[test]
    fn full_bleed_crops_to_opaque_bbox() {
        // 3x3, only top-left pixel opaque
        let mut img = RgbaImage::from_pixel(3, 3, Rgba([0, 0, 0, 0]));
        img.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let out = full_bleed(&DynamicImage::ImageRgba8(img)).unwrap();
        assert_eq!(out.dimensions(), (1, 1));
        assert_eq!(out.get_pixel(0, 0)[3], 255);
        // fully transparent → None
        let blank = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([0, 0, 0, 0])));
        assert!(full_bleed(&blank).is_none());
    }

    #[test]
    fn extend_pad_adds_symmetric_border() {
        let img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(100, 50, Rgba([1, 2, 3, 255])));
        let out = extend_pad(&img, 0.07, false);
        assert_eq!(out.dimensions(), (114, 58)); // +7% each side
                                                 // corners are the white pad
        assert_eq!(out.get_pixel(0, 0), &Rgba([255, 255, 255, 255]));
        // center is content
        assert_eq!(out.get_pixel(57, 29), &Rgba([1, 2, 3, 255]));
    }
}
