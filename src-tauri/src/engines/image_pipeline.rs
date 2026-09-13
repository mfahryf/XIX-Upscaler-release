use crate::engines::model_catalog::{model_spec, BackendPolicy, ModelId};
use crate::engines::onnx::{self, RgbImage};
use crate::engines::process_control::ProcessControl;
use crate::engines::tile::{tile_count, upscale_tiled, ProgressRange, TileConfig};
use crate::engines::EngineError;
use crate::net::http::ProgressSink;
use image::{DynamicImage, GrayImage, Rgba, RgbaImage};
use std::io::Cursor;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageOutputFormat {
    Jpeg,
    Png,
    WebP,
}

#[derive(Clone, Debug)]
pub struct ImageOutputSpec {
    pub extension: String,
    pub format: ImageOutputFormat,
}

pub fn resolve_image_output(file: &Path, format: Option<&str>) -> ImageOutputSpec {
    match format.unwrap_or("auto").to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => ImageOutputSpec {
            extension: "jpg".into(),
            format: ImageOutputFormat::Jpeg,
        },
        "png" => ImageOutputSpec {
            extension: "png".into(),
            format: ImageOutputFormat::Png,
        },
        "webp" => ImageOutputSpec {
            extension: "webp".into(),
            format: ImageOutputFormat::WebP,
        },
        _ => match file
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("jpg") => ImageOutputSpec {
                extension: "jpg".into(),
                format: ImageOutputFormat::Jpeg,
            },
            Some("jpeg") => ImageOutputSpec {
                extension: "jpeg".into(),
                format: ImageOutputFormat::Jpeg,
            },
            Some("webp") => ImageOutputSpec {
                extension: "webp".into(),
                format: ImageOutputFormat::WebP,
            },
            _ => ImageOutputSpec {
                extension: "png".into(),
                format: ImageOutputFormat::Png,
            },
        },
    }
}

pub struct ImageJob<'a> {
    pub input: &'a Path,
    pub out_dir: &'a Path,
    pub output: ImageOutputSpec,
    pub suffix: String,
    pub skip_existing: bool,
    pub progress: Option<&'a ProgressSink>,
    pub control: Option<&'a ProcessControl>,
}

struct PartGuard(Option<PathBuf>);

impl PartGuard {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn unique_stamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn replace_file(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    if !final_path.exists() {
        return std::fs::rename(part_path, final_path);
    }

    let file_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let backup_path = final_path.with_file_name(format!(
        ".{file_name}.{}.{}.bak",
        std::process::id(),
        unique_stamp()
    ));
    std::fs::rename(final_path, &backup_path)?;

    match std::fs::rename(part_path, final_path) {
        Ok(()) => {
            std::fs::remove_file(backup_path)?;
            Ok(())
        }
        Err(replace_error) => match std::fs::rename(&backup_path, final_path) {
            Ok(()) => Err(replace_error),
            Err(restore_error) => Err(std::io::Error::new(
                restore_error.kind(),
                format!(
                    "gagal mengganti output ({replace_error}) dan memulihkan output lama ({restore_error})"
                ),
            )),
        },
    }
}

pub fn process_image(
    job: ImageJob<'_>,
    model: ModelId,
    policy: BackendPolicy,
) -> Result<Vec<u8>, EngineError> {
    process_image_passes(job, model, policy, 1)
}

pub fn process_image_passes(
    job: ImageJob<'_>,
    model: ModelId,
    policy: BackendPolicy,
    passes: u8,
) -> Result<Vec<u8>, EngineError> {
    let spec = model_spec(model).map_err(EngineError::Other)?;
    let mut locked_policy = None;
    process_image_with_passes(job, spec.scale, passes, |tile| {
        let locked_policy = match locked_policy {
            Some(policy) => policy,
            None => {
                let backend = onnx::validate_model_backend(model, policy)
                    .map_err(EngineError::Other)?;
                let policy = onnx::policy_for_backend(backend);
                locked_policy = Some(policy);
                policy
            }
        };
        onnx::upscale_model_rgb(model, tile, locked_policy)
            .map(|result| result.image)
            .map_err(EngineError::Other)
    })
}

pub fn process_image_with<F>(
    job: ImageJob<'_>,
    scale: u32,
    infer: F,
) -> Result<Vec<u8>, EngineError>
where
    F: FnMut(&RgbImage) -> Result<RgbImage, EngineError>,
{
    process_image_with_passes(job, scale, 1, infer)
}

pub fn process_image_with_passes<F>(
    job: ImageJob<'_>,
    stage_scale: u32,
    passes: u8,
    mut infer: F,
) -> Result<Vec<u8>, EngineError>
where
    F: FnMut(&RgbImage) -> Result<RgbImage, EngineError>,
{
    if stage_scale == 0 || passes == 0 {
        return Err(EngineError::Other(
            "jumlah tahap dan skala gambar harus lebih dari nol".into(),
        ));
    }
    if let Some(control) = job.control {
        control.checkpoint_blocking()?;
    }
    let stem = job
        .input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("out");
    let final_path = job
        .out_dir
        .join(format!("{stem}{}.{}", job.suffix, job.output.extension));
    if job.skip_existing && final_path.is_file() {
        return std::fs::read(final_path).map_err(Into::into);
    }
    let source_bytes = std::fs::read(job.input)?;
    let source = image::load_from_memory(&source_bytes)
        .map_err(|e| EngineError::Other(format!("decode input: {e}")))?;
    if let Some(progress) = job.progress {
        let _ = progress.send(5);
    }
    let mut upscaled = onnx::rgb_from_dynamic(&source);
    let config = TileConfig::new(128, 16, stage_scale);
    let mut pass_counts = Vec::with_capacity(passes as usize);
    let mut pass_width = upscaled.width;
    let mut pass_height = upscaled.height;
    for _ in 0..passes {
        pass_counts.push(tile_count(pass_width, pass_height, config)?);
        pass_width = pass_width
            .checked_mul(stage_scale)
            .ok_or_else(|| EngineError::Other("lebar output terlalu besar".into()))?;
        pass_height = pass_height
            .checked_mul(stage_scale)
            .ok_or_else(|| EngineError::Other("tinggi output terlalu besar".into()))?;
    }
    let total_tiles = pass_counts.iter().try_fold(0usize, |total, count| {
        total
            .checked_add(*count)
            .ok_or_else(|| EngineError::Other("jumlah tile terlalu besar".into()))
    })?;
    let mut completed_tiles = 0usize;
    for (pass_index, pass_tiles) in pass_counts.into_iter().enumerate() {
        let range = job.progress.map(|sink| ProgressRange {
            sink,
            start: (5 + 90 * completed_tiles / total_tiles) as u8,
            end: (5 + 90 * (completed_tiles + pass_tiles) / total_tiles) as u8,
        });
        let current_pass = pass_index + 1;
        upscaled = upscale_tiled(&upscaled, config, job.control, range, |tile| {
            infer(tile).map_err(|error| {
                if passes > 1 {
                    let accumulated_scale = stage_scale
                        .checked_pow(current_pass as u32)
                        .map(|scale| format!("{scale}×"))
                        .unwrap_or_else(|| "skala akhir".into());
                    let ordinal = match current_pass {
                        1 => "pertama".into(),
                        2 => "kedua".into(),
                        value => format!("ke-{value}"),
                    };
                    EngineError::Other(format!(
                        "tahap {accumulated_scale} {ordinal} ({current_pass}/{passes}): {error}"
                    ))
                } else {
                    error
                }
            })
        })?;
        completed_tiles += pass_tiles;
    }
    let rgb_image = image::RgbImage::from_raw(upscaled.width, upscaled.height, upscaled.pixels)
        .ok_or_else(|| EngineError::Other("buffer RGB output tidak valid".into()))?;
    let mut output_image = DynamicImage::ImageRgb8(rgb_image);
    if matches!(
        job.output.format,
        ImageOutputFormat::Png | ImageOutputFormat::WebP
    ) {
        let rgba = source.to_rgba8();
        let alpha_source = GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            image::Luma([rgba.get_pixel(x, y)[3]])
        });
        let alpha = image::imageops::resize(
            &alpha_source,
            upscaled.width,
            upscaled.height,
            image::imageops::FilterType::Lanczos3,
        );
        let rgb = output_image.to_rgb8();
        let combined = RgbaImage::from_fn(upscaled.width, upscaled.height, |x, y| {
            let color = rgb.get_pixel(x, y);
            Rgba([color[0], color[1], color[2], alpha.get_pixel(x, y)[0]])
        });
        output_image = DynamicImage::ImageRgba8(combined);
    }
    if let Some(control) = job.control {
        control.checkpoint_blocking()?;
    }
    let bytes = match job.output.format {
        ImageOutputFormat::Jpeg => {
            crate::img::encode_jpeg(&output_image, 92).map_err(EngineError::Other)?
        }
        ImageOutputFormat::Png => {
            crate::img::encode_png(&output_image).map_err(EngineError::Other)?
        }
        ImageOutputFormat::WebP => {
            let mut bytes = Vec::new();
            output_image
                .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::WebP)
                .map_err(|e| EngineError::Other(format!("encode WebP: {e}")))?;
            bytes
        }
    };
    std::fs::create_dir_all(job.out_dir)?;
    let stamp = unique_stamp();
    let part_path = job
        .out_dir
        .join(format!(".{stem}.{}.{}.part", std::process::id(), stamp));
    let mut guard = PartGuard::new(part_path.clone());
    std::fs::write(&part_path, &bytes)?;
    if let Some(control) = job.control {
        control.checkpoint_blocking()?;
    }
    replace_file(&part_path, &final_path)?;
    guard.disarm();
    if let Some(progress) = job.progress {
        let _ = progress.send(100);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::onnx::RgbImage;
    use crate::engines::process_control::ProcessControl;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn nearest_2x(input: &RgbImage) -> Result<RgbImage, EngineError> {
        let mut pixels = vec![0; input.width as usize * input.height as usize * 12];
        for y in 0..input.height as usize {
            for x in 0..input.width as usize {
                for dy in 0..2 {
                    for dx in 0..2 {
                        let src = (y * input.width as usize + x) * 3;
                        let dst = ((y * 2 + dy) * input.width as usize * 2 + x * 2 + dx) * 3;
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

    fn temp_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "xix-image-pipeline-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn auto_output_preserves_supported_input_extension() {
        assert_eq!(
            resolve_image_output(std::path::Path::new("a.jpg"), None).extension,
            "jpg"
        );
        assert_eq!(
            resolve_image_output(std::path::Path::new("a.jpeg"), Some("auto")).extension,
            "jpeg"
        );
        assert_eq!(
            resolve_image_output(std::path::Path::new("a.png"), Some("auto")).extension,
            "png"
        );
        assert_eq!(
            resolve_image_output(std::path::Path::new("a.webp"), Some("auto")).extension,
            "webp"
        );
    }

    #[test]
    fn rgba_alpha_is_resized_and_preserved() {
        let root = temp_root("alpha");
        let input = root.join("tiny.png");
        let output = root.join("out");
        std::fs::create_dir_all(&root).unwrap();
        image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_raw(
                2,
                2,
                vec![
                    255, 0, 0, 0, 0, 255, 0, 64, 0, 0, 255, 128, 255, 255, 255, 255,
                ],
            )
            .unwrap(),
        )
        .save(&input)
        .unwrap();
        let bytes = process_image_with(
            ImageJob {
                input: &input,
                out_dir: &output,
                output: resolve_image_output(&input, Some("png")),
                suffix: "-x".into(),
                skip_existing: false,
                progress: None,
                control: None,
            },
            2,
            nearest_2x,
        )
        .unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (4, 4));
        assert_eq!(decoded.get_pixel(0, 0).0[3], 0);
        assert_eq!(decoded.get_pixel(3, 3).0[3], 255);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inference_error_keeps_existing_final_and_removes_part() {
        let root = temp_root("cleanup");
        let input = root.join("tiny.png");
        let output = root.join("out");
        std::fs::create_dir_all(&output).unwrap();
        image::DynamicImage::new_rgb8(2, 2).save(&input).unwrap();
        let final_path = output.join("tiny-x.png");
        std::fs::write(&final_path, b"existing").unwrap();
        let result = process_image_with(
            ImageJob {
                input: &input,
                out_dir: &output,
                output: resolve_image_output(&input, Some("png")),
                suffix: "-x".into(),
                skip_existing: false,
                progress: None,
                control: None,
            },
            2,
            |_| Err(EngineError::Other("boom".into())),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(std::fs::read_dir(&output).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".part")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_pass_pipeline_outputs_four_times_dimensions_alpha_and_progress() {
        let root = temp_root("two-pass");
        let input = root.join("large.png");
        let output = root.join("out");
        std::fs::create_dir_all(&root).unwrap();
        let source = image::RgbaImage::from_fn(97, 97, |x, y| {
            let alpha = if (x, y) == (0, 0) {
                0
            } else if (x, y) == (96, 96) {
                255
            } else {
                128
            };
            image::Rgba([x as u8, y as u8, 96, alpha])
        });
        image::DynamicImage::ImageRgba8(source)
            .save(&input)
            .unwrap();
        let (progress, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut calls = 0usize;

        let bytes = process_image_with_passes(
            ImageJob {
                input: &input,
                out_dir: &output,
                output: resolve_image_output(&input, Some("png")),
                suffix: "-x".into(),
                skip_existing: false,
                progress: Some(&progress),
                control: None,
            },
            2,
            2,
            |tile| {
                calls += 1;
                nearest_2x(tile)
            },
        )
        .unwrap();

        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (388, 388));
        assert_eq!(decoded.get_pixel(0, 0).0[3], 0);
        assert_eq!(decoded.get_pixel(387, 387).0[3], 255);
        assert_eq!(calls, 13);
        let mut observed = Vec::new();
        while let Ok(value) = progress_rx.try_recv() {
            observed.push(value);
        }
        assert!(observed.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(observed.contains(&32));
        assert_eq!(observed.last(), Some(&100));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn second_pass_failure_keeps_existing_output_and_removes_part() {
        let root = temp_root("second-pass-failure");
        let input = root.join("tiny.png");
        let output = root.join("out");
        std::fs::create_dir_all(&output).unwrap();
        image::DynamicImage::new_rgb8(2, 2).save(&input).unwrap();
        let final_path = output.join("tiny-x.png");
        std::fs::write(&final_path, b"existing").unwrap();
        let mut calls = 0usize;

        let result = process_image_with_passes(
            ImageJob {
                input: &input,
                out_dir: &output,
                output: resolve_image_output(&input, Some("png")),
                suffix: "-x".into(),
                skip_existing: false,
                progress: None,
                control: None,
            },
            2,
            2,
            |tile| {
                calls += 1;
                if calls == 2 {
                    Err(EngineError::Other("second pass failed".into()))
                } else {
                    nearest_2x(tile)
                }
            },
        );

        assert!(result
            .unwrap_err()
            .to_string()
            .contains("tahap 4× kedua"));
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(std::fs::read_dir(&output).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            !name.to_string_lossy().contains(".part")
                && !name.to_string_lossy().contains(".bak")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_between_passes_keeps_existing_output_and_removes_part() {
        let root = temp_root("cancel-between-passes");
        let input = root.join("tiny.png");
        let output = root.join("out");
        std::fs::create_dir_all(&output).unwrap();
        image::DynamicImage::new_rgb8(2, 2).save(&input).unwrap();
        let final_path = output.join("tiny-x.png");
        std::fs::write(&final_path, b"existing").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let control = ProcessControl::new(cancel.clone(), Arc::new(AtomicBool::new(false)));
        let mut calls = 0usize;

        let result = process_image_with_passes(
            ImageJob {
                input: &input,
                out_dir: &output,
                output: resolve_image_output(&input, Some("png")),
                suffix: "-x".into(),
                skip_existing: false,
                progress: None,
                control: Some(&control),
            },
            2,
            2,
            |tile| {
                calls += 1;
                cancel.store(true, Ordering::Relaxed);
                nearest_2x(tile)
            },
        );

        assert!(matches!(result, Err(EngineError::Cancelled)));
        assert_eq!(calls, 1);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(std::fs::read_dir(&output).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".part")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pause_between_passes_waits_then_resumes() {
        let root = temp_root("pause-between-passes");
        let input = root.join("tiny.png");
        let output = root.join("out");
        std::fs::create_dir_all(&root).unwrap();
        image::DynamicImage::new_rgb8(2, 2).save(&input).unwrap();
        let pause = Arc::new(AtomicBool::new(false));
        let control = ProcessControl::new(Arc::new(AtomicBool::new(false)), pause.clone());
        let resume = pause.clone();
        let mut calls = 0usize;
        let started = std::time::Instant::now();

        let bytes = process_image_with_passes(
            ImageJob {
                input: &input,
                out_dir: &output,
                output: resolve_image_output(&input, Some("png")),
                suffix: "-x".into(),
                skip_existing: false,
                progress: None,
                control: Some(&control),
            },
            2,
            2,
            |tile| {
                calls += 1;
                if calls == 1 {
                    pause.store(true, Ordering::Relaxed);
                    let resume = resume.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        resume.store(false, Ordering::Relaxed);
                    });
                }
                nearest_2x(tile)
            },
        )
        .unwrap();

        assert_eq!(calls, 2);
        assert!(started.elapsed() >= std::time::Duration::from_millis(100));
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 8);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replacement_failure_restores_existing_final() {
        let root = temp_root("replace");
        std::fs::create_dir_all(&root).unwrap();
        let final_path = root.join("result.png");
        std::fs::write(&final_path, b"existing").unwrap();

        let result = replace_file(&root.join("missing.part"), &final_path);

        assert!(result.is_err());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".bak")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
