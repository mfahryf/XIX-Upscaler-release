//! Persisted app settings — last used folders, playlist splitter position and
//! per-engine option values, so the app reopens where the user left off.
//! Stored as JSON next to the app data dir; a missing/corrupt file falls back
//! to `Default` (never crashes the app).

use crate::engines::EngineOptions;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub last_output: Option<String>,
    /// Playlist panel height in pixels (0 = default 3/5 of screen). Kept for
    /// backward compat with old configs; the splitter UI is gone, and the
    /// frontend no longer sends this field (serde default keeps saves alive).
    #[serde(default)]
    pub splitter_pos: u32,
    /// Background effect mode ("none", "aurora", "bokeh", "grain", "star",
    /// "embers", "scanline", "rain", "snow", "fireflies", "ripples",
    /// "grid", "orbit"). New field → default for configs saved before it.
    #[serde(default = "default_bg_fx")]
    pub bg_fx: String,
    /// Color palette id ("glass", "neon", "sunset", "emerald", "ocean",
    /// "gold"). New field → default for configs saved before it.
    #[serde(default = "default_palette")]
    pub palette: String,
    pub engine_options: EngineOptions,
    /// v1 proxy mode: "direct", "user" (use proxy_list), "tor" or "free"
    /// (aggregate HProxy + ProxyScrape candidates).
    pub proxy_mode: String,
    /// User-supplied proxies for v1 (one per line, `socks5://…` / `http://…`).
    pub proxy_list: Vec<String>,
    /// API key for HProxy free-proxy listing (optional; empty = anonymous).
    /// New field → empty string for configs saved before it.
    #[serde(default)]
    pub hproxy_api_key: String,
}

fn default_bg_fx() -> String {
    "none".into()
}

fn default_palette() -> String {
    "neon".into()
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            last_output: None,
            splitter_pos: 0,
            bg_fx: "none".into(),
            palette: "neon".into(),
            engine_options: EngineOptions::new(),
            proxy_mode: "direct".into(),
            proxy_list: Vec::new(),
            hproxy_api_key: String::new(),
        }
    }
}

/// Load config from `path`; missing or unparseable file → `Default`.
pub fn load(path: &Path) -> AppConfig {
    let Ok(raw) = std::fs::read(path) else {
        return AppConfig::default();
    };
    serde_json::from_slice(&raw).unwrap_or_default()
}

pub fn save(path: &Path, cfg: &AppConfig) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_missing_returns_default() {
        let cfg = load(Path::new("C:/definitely/not/exists/xix.json"));
        assert_eq!(cfg.splitter_pos, 0);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = std::env::temp_dir().join("xix-config-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p: PathBuf = dir.join("config.json");
        let cfg = AppConfig {
            last_output: Some("C:/out".into()),
            splitter_pos: 240,
            bg_fx: "star".into(),
            palette: "neon".into(),
            engine_options: EngineOptions::new(),
            proxy_mode: "user".into(),
            proxy_list: vec!["socks5://1.2.3.4:1080".into()],
            hproxy_api_key: String::new(),
        };
        // config lama tanpa field baru tetap bisa di-load (serde default)
        let old_json = r#"{"last_output":"C:/out","splitter_pos":240,"engine_options":{},"proxy_mode":"user","proxy_list":["socks5://1.2.3.4:1080"]}"#;
        std::fs::write(&p, old_json).unwrap();
        let old = load(&p);
        assert_eq!(old.bg_fx, "none");
        assert_eq!(old.palette, "neon");
        std::fs::write(&p, serde_json::to_vec(&cfg).unwrap()).unwrap();
        save(&p, &cfg).unwrap();
        let loaded = load(&p);
        assert_eq!(loaded.splitter_pos, 240);
        assert_eq!(loaded.proxy_mode, "user");
        assert_eq!(loaded.proxy_list, vec!["socks5://1.2.3.4:1080".to_string()]);
    }

    #[test]
    fn hproxy_api_key_defaults_and_round_trips() {
        let dir = std::env::temp_dir().join("xix-config-test-hproxy");
        std::fs::create_dir_all(&dir).unwrap();
        let p: PathBuf = dir.join("config.json");
        // config lama tanpa field baru tetap bisa di-load (serde default "")
        let old_json = r#"{"last_output":"C:/out","splitter_pos":0,"engine_options":{},"bg_fx":"none","palette":"glass","proxy_mode":"direct","proxy_list":[]}"#;
        std::fs::write(&p, old_json).unwrap();
        let old = load(&p);
        assert_eq!(old.hproxy_api_key, "");
        // save + load round-trip kunci non-empty
        let cfg = AppConfig {
            last_output: None,
            splitter_pos: 0,
            bg_fx: "none".into(),
            palette: "glass".into(),
            engine_options: EngineOptions::new(),
            proxy_mode: "free".into(),
            proxy_list: Vec::new(),
            hproxy_api_key: "hpx_test_123".into(),
        };
        save(&p, &cfg).unwrap();
        let loaded = load(&p);
        assert_eq!(loaded.proxy_mode, "free");
        assert_eq!(loaded.hproxy_api_key, "hpx_test_123");
    }

    #[test]
    fn load_without_splitter_pos_matches_frontend_payload() {
        // Frontend `save_config` payload no longer includes splitter_pos
        // (splitter UI removed) — must still deserialize (defaults to 0).
        let dir = std::env::temp_dir().join("xix-config-test3");
        std::fs::create_dir_all(&dir).unwrap();
        let p: PathBuf = dir.join("config.json");
        let json = r#"{"last_output":"C:/out","engine_options":{},"bg_fx":"orbit","palette":"gold","proxy_mode":"direct","proxy_list":[]}"#;
        std::fs::write(&p, json).unwrap();
        let cfg = load(&p);
        assert_eq!(cfg.splitter_pos, 0);
        assert_eq!(cfg.bg_fx, "orbit");
        assert_eq!(cfg.palette, "gold");
    }

    #[test]
    fn corrupt_file_falls_back_to_default() {
        let dir = std::env::temp_dir().join("xix-config-test2");
        std::fs::create_dir_all(&dir).unwrap();
        let p: PathBuf = dir.join("config.json");
        std::fs::write(&p, b"{not json").unwrap();
        let cfg = load(&p);
        assert_eq!(cfg.splitter_pos, 0);
    }
}
