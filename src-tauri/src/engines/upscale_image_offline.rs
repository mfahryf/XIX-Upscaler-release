//! Local image upscaling with ESRGAN Slim 2x and 4x models.

use crate::engines::image_pipeline::{process_image, resolve_image_output, ImageJob, ImageOutputSpec};
use crate::engines::model_catalog::{BackendPolicy, ModelId};
use crate::engines::process_control::ProcessControl;
use crate::engines::{Engine, EngineError, EngineOptions, OptionDef, OptionKind};
use std::path::Path;

pub struct UpscaleImageOfflineEngine;

impl UpscaleImageOfflineEngine {
    pub fn new() -> Self {
        Self
    }

    fn scale(opts: &EngineOptions) -> u32 {
        match opts.get("scale").and_then(|v| v.as_str()) {
            Some("4") => 4,
            _ => 2,
        }
    }

    fn suffix(opts: &EngineOptions) -> String {
        opts.get("suffix")
            .and_then(|v| v.as_str())
            .unwrap_or("-esrgan")
            .to_string()
    }

    fn output_spec(file: &Path, opts: &EngineOptions) -> ImageOutputSpec {
        resolve_image_output(file, opts.get("format").and_then(|value| value.as_str()))
    }

    fn model(opts: &EngineOptions) -> ModelId {
        match Self::scale(opts) {
            4 => ModelId::EsrganSlim4x,
            _ => ModelId::EsrganSlim2x,
        }
    }
}

impl Engine for UpscaleImageOfflineEngine {
    fn id(&self) -> &str {
        "upscale-esrgan"
    }

    fn name(&self) -> &str {
        "Image (Offline)"
    }

    fn options_schema(&self) -> Vec<OptionDef> {
        vec![
            OptionDef {
                id: "format".into(),
                label: "Format".into(),
                kind: OptionKind::Select(vec![
                    ("auto".into(), "Auto".into()),
                    ("jpg".into(), "JPG".into()),
                    ("png".into(), "PNG".into()),
                ]),
                default: serde_json::json!("auto"),
            },
            OptionDef {
                id: "scale".into(),
                label: "Scale".into(),
                kind: OptionKind::Select(vec![
                    ("2".into(), "2×".into()),
                    ("4".into(), "4×".into()),
                ]),
                default: serde_json::json!("2"),
            },
            OptionDef {
                id: "skip_existing".into(),
                label: "Skip existing".into(),
                kind: OptionKind::Bool,
                default: serde_json::json!(false),
            },
            OptionDef {
                id: "suffix".into(),
                label: "Suffix output".into(),
                kind: OptionKind::Text,
                default: serde_json::json!("-esrgan"),
            },
        ]
    }

    fn output_name(&self, file: &Path, opts: &EngineOptions) -> String {
        let base = file.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        let spec = Self::output_spec(file, opts);
        format!("{base}{}.{}", Self::suffix(opts), spec.extension)
    }

    fn max_batch_concurrency(&self) -> usize { 1 }

    fn process<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        progress: Option<&'a crate::net::http::ProgressSink>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        process_async(file, out_dir, opts, progress, None, BackendPolicy::PreferDirectMl)
    }

    fn process_controlled<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        progress: Option<&'a crate::net::http::ProgressSink>,
        control: Option<&'a ProcessControl>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        process_async(
            file,
            out_dir,
            opts,
            progress,
            control,
            BackendPolicy::PreferDirectMl,
        )
    }
}

fn process_async<'a>(
    file: &'a Path,
    out_dir: &'a Path,
    opts: &'a EngineOptions,
    progress: Option<&'a crate::net::http::ProgressSink>,
    control: Option<&'a ProcessControl>,
    policy: BackendPolicy,
) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
    let file = file.to_path_buf();
    let out_dir = out_dir.to_path_buf();
    let opts = opts.clone();
    let progress = progress.cloned();
    let control = control.cloned();
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            process_with_policy(
                &file,
                &out_dir,
                &opts,
                progress.as_ref(),
                control.as_ref(),
                policy,
            )
        })
        .await
        .map_err(|e| EngineError::Other(format!("worker gambar gagal: {e}")))?
    })
}

fn process_with_policy(
    file: &Path,
    out_dir: &Path,
    opts: &EngineOptions,
    progress: Option<&crate::net::http::ProgressSink>,
    control: Option<&ProcessControl>,
    policy: BackendPolicy,
) -> Result<Vec<u8>, EngineError> {
    process_image(
        ImageJob {
            input: file,
            out_dir,
            output: UpscaleImageOfflineEngine::output_spec(file, opts),
            suffix: UpscaleImageOfflineEngine::suffix(opts),
            skip_existing: opts
                .get("skip_existing")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            progress,
            control,
        },
        UpscaleImageOfflineEngine::model(opts),
        policy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(format: &str) -> EngineOptions {
        [("format".to_string(), serde_json::json!(format))]
            .into_iter()
            .collect()
    }

    #[test]
    fn schema_offers_auto_jpg_png_and_auto_follows_input_extension() {
        let engine = UpscaleImageOfflineEngine::new();
        assert_eq!(engine.name(), "Image (Offline)");
        assert_eq!(engine.input_exts(), ["jpg", "jpeg", "png", "webp"]);
        assert_eq!(
            engine.output_name(Path::new("photo.jpg"), &EngineOptions::new()),
            "photo-esrgan.jpg"
        );
        assert_eq!(
            engine.output_name(Path::new("photo.jpeg"), &EngineOptions::new()),
            "photo-esrgan.jpeg"
        );
        assert_eq!(
            engine.output_name(Path::new("photo.png"), &EngineOptions::new()),
            "photo-esrgan.png"
        );
        assert_eq!(
            engine.output_name(Path::new("photo.webp"), &EngineOptions::new()),
            "photo-esrgan.webp"
        );
        assert_eq!(
            engine.output_name(Path::new("photo.png"), &opts("jpg")),
            "photo-esrgan.jpg"
        );
        assert_eq!(
            engine.output_name(Path::new("photo.jpg"), &opts("png")),
            "photo-esrgan.png"
        );
        let saved_suffix = [("suffix".to_string(), serde_json::json!("-offline"))]
            .into_iter()
            .collect();
        assert_eq!(
            engine.output_name(Path::new("photo.png"), &saved_suffix),
            "photo-offline.png"
        );
        let format = engine
            .options_schema()
            .into_iter()
            .find(|x| x.id == "format")
            .unwrap();
        assert_eq!(format.default, serde_json::json!("auto"));
        assert!(
            matches!(format.kind, OptionKind::Select(options) if options == vec![
                ("auto".into(), "Auto".into()),
                ("jpg".into(), "JPG".into()),
                ("png".into(), "PNG".into()),
            ])
        );
        let scale = engine
            .options_schema()
            .into_iter()
            .find(|x| x.id == "scale")
            .unwrap();
        assert!(matches!(scale.kind, OptionKind::Select(options) if options.len() == 2));
    }

    #[test]
    fn esrgan_slim_cpu_upscales_rgba_at_two_and_four_times() {
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(format!("xix-esrgan-slim-{stamp}"));
        let input_path = root.join("input.png");
        let output_dir = root.join("output");
        std::fs::create_dir_all(&root).unwrap();
        let input = image::RgbaImage::from_fn(17, 13, |x, y| {
            image::Rgba([(x * 13) as u8, (y * 17) as u8, 120, ((x + y) * 8) as u8])
        });
        image::DynamicImage::ImageRgba8(input).save(&input_path).unwrap();

        for scale in [2, 4] {
            let options = [
                ("format".to_string(), serde_json::json!("png")),
                ("scale".to_string(), serde_json::json!(scale.to_string())),
            ]
            .into_iter()
            .collect();
            let bytes = process_with_policy(
                &input_path,
                &output_dir,
                &options,
                None,
                None,
                crate::engines::model_catalog::BackendPolicy::CpuOnly,
            )
            .unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
            assert_eq!(decoded.dimensions(), (17 * scale, 13 * scale));
            assert_eq!(decoded.get_pixel(0, 0)[3], 0);
            assert!(decoded.get_pixel(16 * scale, 12 * scale)[3] > 0);
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn explicit_jpg_encodes_jpeg_and_uses_jpg_extension() {
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(format!("xix-upscaler-jpg-{stamp}"));
        let input_path = root.join("input.png");
        let output_dir = root.join("output");
        std::fs::create_dir_all(&root).unwrap();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(2, 2, vec![180; 12]).unwrap())
            .save(&input_path)
            .unwrap();

        let engine = UpscaleImageOfflineEngine::new();
        let bytes = engine
            .process(&input_path, &output_dir, &opts("jpg"), None)
            .await
            .unwrap();
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert!(output_dir.join("input-esrgan.jpg").is_file());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn auto_webp_encodes_webp_and_keeps_webp_extension() {
        let stamp = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(format!("xix-upscaler-webp-{stamp}"));
        let input_path = root.join("input.webp");
        let output_dir = root.join("output");
        std::fs::create_dir_all(&root).unwrap();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_raw(2, 2, vec![120; 12]).unwrap())
            .save(&input_path)
            .unwrap();

        let engine = UpscaleImageOfflineEngine::new();
        let bytes = engine
            .process(&input_path, &output_dir, &opts("auto"), None)
            .await
            .unwrap();
        assert_eq!(
            image::guess_format(&bytes).unwrap(),
            image::ImageFormat::WebP
        );
        assert!(output_dir.join("input-esrgan.webp").is_file());

        std::fs::remove_dir_all(root).unwrap();
    }
}
