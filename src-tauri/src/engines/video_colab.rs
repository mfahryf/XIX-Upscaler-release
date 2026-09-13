//! Remote video-upscale profile executed by the XIX Google Colab worker.

use crate::engines::{Engine, EngineError, EngineOptions, OptionDef, OptionKind};
use std::path::Path;

pub struct VideoColabEngine;

impl VideoColabEngine {
    pub fn new() -> Self {
        Self
    }

    fn suffix(opts: &EngineOptions) -> &str {
        opts.get("suffix")
            .and_then(|value| value.as_str())
            .unwrap_or("-colab")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_colab_keep_drive_files_is_an_opt_in_boolean() {
        let schema = VideoColabEngine::new().options_schema();
        let option = schema
            .iter()
            .find(|option| option.id == "keep_drive_files")
            .expect("Colab must expose Drive retention");
        assert!(matches!(option.kind, OptionKind::Bool));
        assert_eq!(option.default, serde_json::json!(false));
    }
}

impl Engine for VideoColabEngine {
    fn id(&self) -> &str {
        "video-colab"
    }

    fn name(&self) -> &str {
        "Video (Colab)"
    }

    fn options_schema(&self) -> Vec<OptionDef> {
        vec![
            OptionDef {
                id: "format".into(),
                label: "Format".into(),
                kind: OptionKind::Select(vec![("mp4".into(), "MP4".into())]),
                default: serde_json::json!("mp4"),
            },
            OptionDef {
                id: "scale".into(),
                label: "Scale".into(),
                kind: OptionKind::Select(vec![
                    ("2".into(), "2×".into()),
                    ("4".into(), "4×".into()),
                ]),
                default: serde_json::json!("4"),
            },
            OptionDef {
                id: "interpolation".into(),
                label: "Interpolation".into(),
                kind: OptionKind::Select(vec![
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
                ]),
                default: serde_json::json!("off"),
            },
            OptionDef {
                id: "mute_audio".into(),
                label: "Mute".into(),
                kind: OptionKind::Bool,
                default: serde_json::json!(false),
            },
            OptionDef {
                id: "suffix".into(),
                label: "Suffix output".into(),
                kind: OptionKind::Text,
                default: serde_json::json!("-colab"),
            },
            OptionDef {
                id: "keep_drive_files".into(),
                label: "Simpan file kerja di Drive".into(),
                kind: OptionKind::Bool,
                default: serde_json::json!(false),
            },
        ]
    }

    fn input_exts(&self) -> &'static [&'static str] {
        &["mp4", "mov", "webm", "mkv", "m4v", "ts"]
    }

    fn output_name(&self, file: &Path, opts: &EngineOptions) -> String {
        let base = file
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("out");
        format!("{base}{}.mp4", Self::suffix(opts))
    }

    fn max_batch_concurrency(&self) -> usize {
        1
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn process<'a>(
        &'a self,
        _file: &'a Path,
        _out_dir: &'a Path,
        _opts: &'a EngineOptions,
        _progress: Option<&'a crate::net::http::ProgressSink>,
    ) -> crate::net::http::BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        Box::pin(async {
            Err(EngineError::Other(
                "Video Colab harus dimulai melalui integrasi Google Drive dan notebook".into(),
            ))
        })
    }
}
