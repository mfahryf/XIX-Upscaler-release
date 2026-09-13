//! Local preflight for jobs consumed by the Colab worker.

pub mod auth;
pub mod coordinator;
mod debug;
pub mod drive;
pub mod manifest;
pub mod media;
pub mod output;
pub mod registry;
pub mod remote_status;
pub mod runtime;
pub mod vault;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColabError(pub(super) &'static str);

impl std::fmt::Display for ColabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ColabError {}

#[cfg(test)]
pub(super) mod test_support {
    use std::{fs, path::PathBuf};

    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("xix-colab-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    pub fn probe_json() -> serde_json::Value {
        serde_json::json!({
            "streams": [
                {"index": 0, "codec_type": "audio", "codec_name": "aac"},
                {"index": 1, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080, "duration": "60.000000",
                 "avg_frame_rate": "24000/1001", "time_base": "1/24000",
                 "disposition": {"default": 1, "attached_pic": 0},
                 "tags": {"rotate": "90"},
                 "side_data_list": [{"side_data_type": "Display Matrix", "rotation": -90}]}
            ],
            "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "60.000000"}
        })
    }
}
