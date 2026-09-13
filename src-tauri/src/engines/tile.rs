use crate::engines::onnx::RgbImage;
use crate::engines::process_control::ProcessControl;
use crate::engines::EngineError;
use crate::net::http::ProgressSink;

#[derive(Clone, Copy)]
pub struct TileConfig {
    pub tile_size: u32,
    pub overlap: u32,
    pub scale: u32,
}
impl TileConfig {
    pub fn new(tile_size: u32, overlap: u32, scale: u32) -> Self {
        Self {
            tile_size,
            overlap,
            scale,
        }
    }
}

pub struct ProgressRange<'a> {
    pub sink: &'a ProgressSink,
    pub start: u8,
    pub end: u8,
}

fn reflected(value: i64, length: u32) -> usize {
    if length <= 1 {
        return 0;
    }
    let period = (length as i64 - 1) * 2;
    let folded = value.rem_euclid(period);
    if folded < length as i64 {
        folded as usize
    } else {
        (period - folded) as usize
    }
}

fn tile_layout(
    width: u32,
    height: u32,
    config: TileConfig,
) -> Result<(u32, u32, usize), EngineError> {
    if config.tile_size <= config.overlap * 2
        || config.scale == 0
        || width == 0
        || height == 0
    {
        return Err(EngineError::Other("konfigurasi tile tidak valid".into()));
    }
    let core = config.tile_size - config.overlap * 2;
    let cols = width.div_ceil(core) as usize;
    let rows = height.div_ceil(core) as usize;
    let total = cols
        .checked_mul(rows)
        .ok_or_else(|| EngineError::Other("jumlah tile terlalu besar".into()))?;
    Ok((cols as u32, rows as u32, total))
}

pub fn tile_count(
    width: u32,
    height: u32,
    config: TileConfig,
) -> Result<usize, EngineError> {
    tile_layout(width, height, config).map(|(_, _, total)| total)
}

pub fn upscale_tiled<F>(
    input: &RgbImage,
    config: TileConfig,
    control: Option<&ProcessControl>,
    progress: Option<ProgressRange<'_>>,
    mut infer: F,
) -> Result<RgbImage, EngineError>
where
    F: FnMut(&RgbImage) -> Result<RgbImage, EngineError>,
{
    let (cols, rows, total) = tile_layout(input.width, input.height, config)?;
    if input.pixels.len() != input.width as usize * input.height as usize * 3 {
        return Err(EngineError::Other("buffer RGB tidak valid".into()));
    }
    let core = config.tile_size - config.overlap * 2;
    let out_width = input.width * config.scale;
    let out_height = input.height * config.scale;
    let mut output = RgbImage {
        width: out_width,
        height: out_height,
        pixels: vec![0; out_width as usize * out_height as usize * 3],
    };
    let mut done = 0usize;
    for row in 0..rows {
        for col in 0..cols {
            if let Some(control) = control {
                control.checkpoint_blocking()?;
            }
            let origin_x = col * core;
            let origin_y = row * core;
            let copy_w = core.min(input.width - origin_x);
            let copy_h = core.min(input.height - origin_y);
            let mut tile = RgbImage {
                width: config.tile_size,
                height: config.tile_size,
                pixels: vec![0; config.tile_size as usize * config.tile_size as usize * 3],
            };
            for ty in 0..config.tile_size {
                for tx in 0..config.tile_size {
                    let sx = reflected(
                        origin_x as i64 + tx as i64 - config.overlap as i64,
                        input.width,
                    );
                    let sy = reflected(
                        origin_y as i64 + ty as i64 - config.overlap as i64,
                        input.height,
                    );
                    let src = (sy * input.width as usize + sx) * 3;
                    let dst = (ty as usize * config.tile_size as usize + tx as usize) * 3;
                    tile.pixels[dst..dst + 3].copy_from_slice(&input.pixels[src..src + 3]);
                }
            }
            let inferred = infer(&tile)?;
            if inferred.width != config.tile_size * config.scale
                || inferred.height != config.tile_size * config.scale
            {
                return Err(EngineError::Other("skala output tile tidak sesuai".into()));
            }
            for y in 0..copy_h * config.scale {
                let src_x = config.overlap * config.scale;
                let src_y = config.overlap * config.scale + y;
                let dst_x = origin_x * config.scale;
                let dst_y = origin_y * config.scale + y;
                let count = copy_w as usize * config.scale as usize * 3;
                let src = (src_y as usize * inferred.width as usize + src_x as usize) * 3;
                let dst = (dst_y as usize * out_width as usize + dst_x as usize) * 3;
                output.pixels[dst..dst + count].copy_from_slice(&inferred.pixels[src..src + count]);
            }
            done += 1;
            if let Some(range) = &progress {
                let pct = range.start as usize
                    + (range.end.saturating_sub(range.start) as usize * done / total);
                let _ = range.sink.send(pct as u8);
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nearest_2x(input: &RgbImage) -> Result<RgbImage, crate::engines::EngineError> {
        let mut pixels = vec![0; input.width as usize * input.height as usize * 12];
        let width = input.width as usize;
        for y in 0..input.height as usize {
            for x in 0..width {
                for dy in 0..2 {
                    for dx in 0..2 {
                        let src = (y * width + x) * 3;
                        let dst = (((y * 2 + dy) * width * 2 + x * 2 + dx) * 3) as usize;
                        pixels[dst..dst + 3].copy_from_slice(&input.pixels[src..src + 3]);
                    }
                }
            }
        }
        Ok(RgbImage {
            width: input.width * 2,
            height: input.height * 2,
            pixels,
        })
    }

    #[test]
    fn odd_sized_tiling_matches_whole_image_nearest() {
        let input = RgbImage {
            width: 301,
            height: 227,
            pixels: (0..301 * 227 * 3).map(|v| (v % 251) as u8).collect(),
        };
        let expected = nearest_2x(&input).unwrap();
        let actual =
            upscale_tiled(&input, TileConfig::new(128, 16, 2), None, None, nearest_2x).unwrap();
        assert_eq!(actual.width, 602);
        assert_eq!(actual.height, 454);
        assert_eq!(actual.pixels, expected.pixels);
    }

    #[test]
    fn tile_count_matches_inference_calls_for_each_pass_size() {
        let config = TileConfig::new(128, 16, 2);
        assert_eq!(tile_count(97, 97, config).unwrap(), 4);
        assert_eq!(tile_count(194, 194, config).unwrap(), 9);
    }
}
