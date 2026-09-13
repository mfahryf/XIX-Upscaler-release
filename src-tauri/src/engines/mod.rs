//! Engine abstraction — every batch tool (vectorize v2, upscaler, remove-bg,
//! …) is a pluggable `Engine`. The desktop UI renders the options schema
//! dynamically, so adding an engine never touches the frontend: register a new
//! struct implementing `Engine` in `registry()`.

pub mod image_pipeline;
pub mod model_catalog;
pub mod onnx;
pub mod process_control;
pub mod proxy;
pub mod tile;
pub mod upscale_image_offline;
pub mod upscale_v1;
pub mod video_colab;

use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

#[derive(Debug)]
pub enum EngineError {
    Network(String),
    Parse(String),
    RateLimit,
    /// Server-side quota exhausted (svgai HTTP 402) — rotate proxy / stop.
    Quota(String),
    /// Authentication / token rejected (photoroom remove-bg 401) — retry
    /// with a freshly minted anonymous token.
    Auth(String),
    SslPin(String),
    Io(std::io::Error),
    Other(String),
    Cancelled,
}

impl Clone for EngineError {
    fn clone(&self) -> Self {
        match self {
            EngineError::Network(m) => EngineError::Network(m.clone()),
            EngineError::Parse(m) => EngineError::Parse(m.clone()),
            EngineError::RateLimit => EngineError::RateLimit,
            EngineError::Quota(m) => EngineError::Quota(m.clone()),
            EngineError::Auth(m) => EngineError::Auth(m.clone()),
            EngineError::SslPin(m) => EngineError::SslPin(m.clone()),
            EngineError::Io(e) => EngineError::Io(std::io::Error::new(e.kind(), e.to_string())),
            EngineError::Other(m) => EngineError::Other(m.clone()),
            EngineError::Cancelled => EngineError::Cancelled,
        }
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Network(m) => write!(f, "network error: {m}"),
            EngineError::Parse(m) => write!(f, "parse error: {m}"),
            EngineError::RateLimit => write!(f, "rate limit reached (429)"),
            EngineError::Quota(m) => write!(f, "quota exhausted: {m}"),
            EngineError::Auth(m) => write!(f, "auth error: {m}"),
            EngineError::SslPin(m) => write!(f, "certificate pin mismatch: {m}"),
            EngineError::Io(e) => write!(f, "io error: {e}"),
            EngineError::Other(m) => write!(f, "{m}"),
            EngineError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for EngineError {}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        EngineError::Io(e)
    }
}

/// Shape of one UI control in the options panel.
#[derive(Clone, Serialize)]
pub enum OptionKind {
    /// Dropdown: (value, label) pairs.
    Select(Vec<(String, String)>),
    Number {
        min: f64,
        max: f64,
        step: f64,
    },
    Bool,
    Text,
}

#[derive(Clone, Serialize)]
pub struct OptionDef {
    pub id: String,
    pub label: String,
    pub kind: OptionKind,
    pub default: serde_json::Value,
}

pub type EngineOptions = HashMap<String, serde_json::Value>;

/// Opsi mitigasi batch yang berlaku untuk semua engine (dipasang oleh
/// `list_engines` di lib.rs sehingga muncul di panel ADV tiap engine):
/// - `batch_delay`: jeda antar file (detik) — menghormati rate limit svg.new
///   / photoroom yang mem-flag IP setelah N request beruntun (403).
/// - `retry_403`: retry sekali dengan backoff saat server menolak 403.
/// - `concurrency`: jumlah worker paralel (1..=8, default 3); gate pacing
///   global tetap membatasi satu START per `batch_delay` detik.
pub fn common_batch_options(max_concurrency: usize) -> Vec<OptionDef> {
    let max_concurrency = max_concurrency.clamp(1, 8);
    vec![
        OptionDef {
            id: "batch_delay".into(),
            label: "Delay".into(),
            kind: OptionKind::Number {
                min: 0.0,
                max: 60.0,
                step: 1.0,
            },
            default: serde_json::json!(3),
        },
        OptionDef {
            id: "retry_403".into(),
            label: "Retry saat 403".into(),
            kind: OptionKind::Bool,
            default: serde_json::json!(true),
        },
        OptionDef {
            id: "concurrency".into(),
            label: if max_concurrency == 1 {
                "Concurrency (GPU max 1)".into()
            } else {
                "Concurrency".into()
            },
            kind: OptionKind::Number {
                min: 1.0,
                max: max_concurrency as f64,
                step: 1.0,
            },
            default: serde_json::json!(3usize.min(max_concurrency)),
        },
    ]
}

/// True jika error adalah HTTP 403 dari server (format Network kita selalu
/// `HTTP {status}: ...`) — dipakai batch loop untuk retry dengan backoff.
pub fn is_http_403(e: &EngineError) -> bool {
    matches!(e, EngineError::Network(m) if m.contains("HTTP 403"))
}

/// True jika error adalah parse "response bukan SVG" — svgai/svg.new balas
/// 200 tapi body bukan SVG. Lewat proxy publik ini biasanya block page yang
/// disisipkan proxy itu sendiri, jadi sinyal kuat proxy buruk (dipakai untuk
/// blacklist + rotate).
pub fn is_not_svg_parse_error(e: &EngineError) -> bool {
    matches!(e, EngineError::Parse(m) if m.contains("response bukan SVG"))
}

pub trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn options_schema(&self) -> Vec<OptionDef>;
    /// File extensions this engine accepts as input (used by the UI file
    /// picker / folder scan). Default: raster images; SVG converter overrides.
    fn input_exts(&self) -> &'static [&'static str] {
        &["jpg", "jpeg", "png", "webp"]
    }
    /// Output file name for one input (used by the batch loop for progress
    /// events — the engine owns the naming scheme, e.g. `{stem}-v2.svg`).
    fn output_name(&self, file: &Path, opts: &EngineOptions) -> String;
    fn max_batch_concurrency(&self) -> usize {
        8
    }
    fn is_remote(&self) -> bool {
        false
    }
    /// Process one input file into output bytes. Async because engines make
    /// network calls; the batch loop runs inside a Tauri async command.
    /// `progress` (v2 only) receives live percent ticks from the SSE stream;
    /// other engines ignore it.
    fn process<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        progress: Option<&'a crate::net::http::ProgressSink>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>>;

    fn process_controlled<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        progress: Option<&'a crate::net::http::ProgressSink>,
        control: Option<&'a process_control::ProcessControl>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        if let Some(control) = control {
            if let Err(error) = control.checkpoint_blocking() {
                return Box::pin(async move { Err(error) });
            }
        }
        self.process(file, out_dir, opts, progress)
    }
}

/// All registered engines. The existing online engine remains the UI default.
pub fn registry() -> Vec<Box<dyn Engine>> {
    vec![
        Box::new(crate::engines::upscale_v1::UpscaleV1Engine::new()),
        Box::new(crate::engines::upscale_image_offline::UpscaleImageOfflineEngine::new()),
        Box::new(crate::engines::video_colab::VideoColabEngine::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_error_display_readable() {
        let e = EngineError::RateLimit;
        assert!(e.to_string().contains("rate"));
    }

    #[test]
    fn option_def_serializes() {
        let def = OptionDef {
            id: "format".into(),
            label: "Format".into(),
            kind: OptionKind::Select(vec![("svg".into(), "SVG".into())]),
            default: serde_json::json!("svg"),
        };
        let v = serde_json::to_value(def).unwrap();
        assert_eq!(v["id"], "format");
    }

    #[test]
    fn registry_exposes_supported_engines() {
        let ids: Vec<_> = registry()
            .into_iter()
            .map(|engine| engine.id().to_string())
            .collect();
        assert_eq!(ids, ["upscale-v1", "upscale-esrgan", "video-colab"]);
    }

    #[test]
    fn registry_uses_final_user_facing_engine_names() {
        let names: Vec<_> = registry()
            .into_iter()
            .map(|engine| engine.name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "Image (Online)",
                "Image (Offline)",
                "Video (Colab)",
            ]
        );
    }

    #[test]
    fn registered_engines_accept_their_supported_media() {
        let engines = registry();
        assert_eq!(engines.len(), 3);
        assert!(engines
            .iter()
            .take(2)
            .all(|engine| engine.input_exts() == ["jpg", "jpeg", "png", "webp"]));
        assert_eq!(engines[1].max_batch_concurrency(), 1);
        assert_eq!(
            engines[2].input_exts(),
            ["mp4", "mov", "webm", "mkv", "m4v", "ts"]
        );
        assert!(engines[2].is_remote());
    }

    #[test]
    fn colab_engine_exposes_fixed_video_profile_and_optional_mute() {
        let engine = registry()
            .into_iter()
            .find(|engine| engine.id() == "video-colab")
            .expect("video Colab engine");
        let schema = engine.options_schema();
        let option = |id: &str| schema.iter().find(|item| item.id == id).unwrap();

        assert_eq!(option("format").default, serde_json::json!("mp4"));
        assert_eq!(option("scale").default, serde_json::json!("4"));
        assert_eq!(option("interpolation").default, serde_json::json!("off"));
        assert_eq!(option("mute_audio").default, serde_json::json!(false));
        assert_eq!(option("suffix").default, serde_json::json!("-colab"));
        assert!(matches!(
            &option("scale").kind,
            OptionKind::Select(values) if values == &vec![
                ("2".into(), "2×".into()),
                ("4".into(), "4×".into()),
            ]
        ));
        assert!(schema.iter().all(|item| item.id != "model"));
        assert!(schema.iter().all(|item| item.id != "target_fps"));
        assert!(schema.iter().all(|item| item.id != "custom_fps"));

        assert!(matches!(
            &option("interpolation").kind,
            OptionKind::Select(values) if values == &vec![
                ("off".into(), "Off".into()),
                ("23.976".into(), "23.976".into()),
                ("24".into(), "24".into()),
                ("25".into(), "25".into()),
                ("29.97".into(), "29.97".into()),
                ("30".into(), "30".into()),
                ("48".into(), "48".into()),
                ("50".into(), "50".into()),
                ("59.94".into(), "59.94".into()),
                ("60".into(), "60".into()),
            ]
        ));
    }

    #[test]
    fn gpu_batch_options_expose_a_single_worker() {
        let option = common_batch_options(1)
            .into_iter()
            .find(|item| item.id == "concurrency")
            .unwrap();
        assert_eq!(option.label, "Concurrency (GPU max 1)");
        assert!(matches!(
            option.kind,
            OptionKind::Number {
                min: 1.0,
                max: 1.0,
                step: 1.0
            }
        ));
        assert_eq!(option.default, serde_json::json!(1));
    }
}
