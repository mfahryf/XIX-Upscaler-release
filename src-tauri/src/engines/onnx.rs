//! Shared ONNX Runtime helpers for the local upscalers.

use crate::engines::model_catalog::{model_spec, BackendPolicy, InferenceBackend, ModelId};
use ort::{session::Session, value::Tensor};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

#[derive(Debug)]
pub struct RgbImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

static ORT_INIT: OnceLock<()> = OnceLock::new();
static MODEL_SESSIONS: OnceLock<Mutex<HashMap<(ModelId, InferenceBackend), Arc<Mutex<Session>>>>> = OnceLock::new();

#[derive(Debug)]
pub struct InferenceResult {
    pub image: RgbImage,
    pub backend: InferenceBackend,
}

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn create_model_session(model: ModelId, backend: InferenceBackend) -> Result<Session, String> {
    ORT_INIT.get_or_init(|| { ort::init().with_name("XIX-Upscaler").commit(); });
    let spec = model_spec(model)?;
    let builder = Session::builder().map_err(|e| format!("ONNX Runtime gagal dibuat: {e}"))?;
    let mut builder = if backend == InferenceBackend::DirectMl {
        builder
            .with_execution_providers([ort::ep::DirectML::default().build().error_on_failure()])
            .map_err(|e| format!("DirectML gagal didaftarkan: {e}"))?
    } else { builder };
    builder.commit_from_file(&spec.path).map_err(|e| format!("model {} gagal dimuat: {e}", spec.path.display()))
}

fn model_session(model: ModelId, backend: InferenceBackend) -> Result<Arc<Mutex<Session>>, String> {
    let cache = MODEL_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(session) = cache.lock().get(&(model, backend)).cloned() { return Ok(session); }
    let session = Arc::new(Mutex::new(create_model_session(model, backend)?));
    cache.lock().insert((model, backend), session.clone());
    Ok(session)
}

fn session_for_policy(model: ModelId, policy: BackendPolicy) -> Result<(Arc<Mutex<Session>>, InferenceBackend), String> {
    match policy {
        BackendPolicy::CpuOnly => Ok((model_session(model, InferenceBackend::Cpu)?, InferenceBackend::Cpu)),
        BackendPolicy::RequireDirectMl => model_session(model, InferenceBackend::DirectMl)
            .map(|session| (session, InferenceBackend::DirectMl))
            .map_err(|e| format!("GPU DirectX 12 dengan DirectML wajib untuk engine ini: {e}")),
        BackendPolicy::PreferDirectMl => match model_session(model, InferenceBackend::DirectMl) {
            Ok(session) => Ok((session, InferenceBackend::DirectMl)),
            Err(_) => Ok((model_session(model, InferenceBackend::Cpu)?, InferenceBackend::Cpu)),
        },
    }
}

pub fn validate_model_backend(
    model: ModelId,
    policy: BackendPolicy,
) -> Result<InferenceBackend, String> {
    session_for_policy(model, policy).map(|(_, backend)| backend)
}

pub(crate) fn policy_for_backend(backend: InferenceBackend) -> BackendPolicy {
    match backend {
        InferenceBackend::DirectMl => BackendPolicy::RequireDirectMl,
        InferenceBackend::Cpu => BackendPolicy::CpuOnly,
    }
}

/// Run one fixed 128×128 model tile through the requested backend policy.
pub fn upscale_model_rgb(model: ModelId, input: &RgbImage, policy: BackendPolicy) -> Result<InferenceResult, String> {
    if input.width != 128 || input.height != 128 { return Err("model offline memerlukan tile RGB 128x128".into()); }
    if input.pixels.len() != 128 * 128 * 3 { return Err("ukuran buffer gambar tidak sesuai dimensinya".into()); }
    let plane = 128 * 128;
    let mut chw = vec![0.0f32; plane * 3];
    for i in 0..plane {
        chw[i] = input.pixels[i * 3] as f32 / 255.0;
        chw[plane + i] = input.pixels[i * 3 + 1] as f32 / 255.0;
        chw[plane * 2 + i] = input.pixels[i * 3 + 2] as f32 / 255.0;
    }
    let tensor = Tensor::<f32>::from_array(([1usize, 3, 128, 128], chw)).map_err(|e| format!("tensor input gagal dibuat: {e}"))?;
    let (session, backend) = session_for_policy(model, policy)?;
    let mut guard = session.lock();
    let outputs = guard.run(ort::inputs![tensor]).map_err(|e| format!("inferensi gagal: {e}"))?;
    let (shape, values) = outputs[0].try_extract_tensor::<f32>().map_err(|e| format!("tensor output tidak valid: {e}"))?;
    let spec = model_spec(model)?;
    let expected = 128usize * spec.scale as usize;
    if shape.as_ref() != [1, 3, expected as i64, expected as i64] { return Err(format!("bentuk output model tidak sesuai: {shape:?}")); }
    let out_plane = expected * expected;
    let mut pixels = vec![0u8; out_plane * 3];
    for i in 0..out_plane { pixels[i * 3] = to_byte(values[i]); pixels[i * 3 + 1] = to_byte(values[out_plane + i]); pixels[i * 3 + 2] = to_byte(values[out_plane * 2 + i]); }
    Ok(InferenceResult { image: RgbImage { width: expected as u32, height: expected as u32, pixels }, backend })
}

pub fn rgb_from_dynamic(image: &image::DynamicImage) -> RgbImage {
    let rgb = image.to_rgb8();
    RgbImage {
        width: rgb.width(),
        height: rgb.height(),
        pixels: rgb.into_raw(),
    }
}

pub fn dynamic_from_rgb(image: &RgbImage) -> image::DynamicImage {
    image::DynamicImage::ImageRgb8(
        image::RgbImage::from_raw(image.width, image.height, image.pixels.clone())
            .expect("RGB buffer dimensions are validated by RgbImage"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_conversion_preserves_dimensions_and_channel_order() {
        let input = image::DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(1, 1, vec![10, 20, 30]).unwrap(),
        );
        let rgb = rgb_from_dynamic(&input);
        assert_eq!(
            (rgb.width, rgb.height, rgb.pixels),
            (1, 1, vec![10, 20, 30])
        );
    }

    #[test]
    fn fixed_model_runtime_rejects_non_tile_input() {
        let input = RgbImage { width: 1, height: 1, pixels: vec![0, 0, 0] };
        assert!(upscale_model_rgb(ModelId::EsrganSlim2x, &input, BackendPolicy::CpuOnly)
            .unwrap_err().contains("128x128"));
    }

    #[test]
    fn resolved_backend_is_converted_to_a_non_fallback_policy() {
        assert_eq!(
            policy_for_backend(InferenceBackend::DirectMl),
            BackendPolicy::RequireDirectMl
        );
        assert_eq!(
            policy_for_backend(InferenceBackend::Cpu),
            BackendPolicy::CpuOnly
        );
    }

    #[test]
    fn bundled_esrgan_runs_on_explicit_cpu_backend() {
        let input = RgbImage { width: 128, height: 128, pixels: vec![0; 128 * 128 * 3] };
        let result = upscale_model_rgb(
            ModelId::EsrganSlim2x,
            &input,
            BackendPolicy::CpuOnly,
        ).unwrap();
        assert_eq!((result.image.width, result.image.height), (256, 256));
        assert_eq!(result.backend, InferenceBackend::Cpu);
    }
}
