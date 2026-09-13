//! The `upscale-v1` engine — photoroom AI upscale, ported to Rust from
//! `src/XIX-Upscaler.js` (with the newer request shape from the web backend:
//! `imageFile` + `outputFormat`, pr-app-version 2026.31.01).
//!
//! Per file: reuse/mint an anonymous Firebase token → POST /v3/upscale
//! (always 4× — photoroom ignores any scale param) → restore PNG
//! transparency from the input alpha → optional full-bleed crop / 7% padding
//! → encode. Tokens rotate pre-emptively every `TOKEN_BUDGET` uses and on
//! 401/403/429.

use crate::engines::proxy::ProxyRotator;
use crate::engines::{Engine, EngineError, EngineOptions, OptionDef, OptionKind};
use crate::img;
use crate::net::http::{BoxFuture, HttpClient, PinnedClient};
use image::DynamicImage;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// Mint a fresh token before this many uses (mirrors the CLI's TOKEN_BUDGET).
const TOKEN_BUDGET: usize = 20;
/// Max attempts per file when the token is rejected / rate limited.
const MAX_TOKEN_RETRIES: usize = 3;
/// Max proxy rotates per file before giving up.
const MAX_PROXY_ROTATES: usize = 12;
/// JPEG output quality (matches the web backend).
const JPEG_QUALITY: u8 = 92;

/// Token cache keyed by proxy (None = direct): rotating IPs mint their own
/// token, because Firebase signUp is rate limited per IP.
#[derive(Default)]
struct TokenState {
    token: Option<String>,
    proxy: Option<String>,
    uses: usize,
}

pub struct UpscaleV1Engine {
    client: Box<dyn HttpClient>,
    token: Mutex<TokenState>,
    rotation: ProxyRotator,
}

impl UpscaleV1Engine {
    pub fn new() -> Self {
        UpscaleV1Engine {
            client: Box::new(PinnedClient::new().expect("failed to init pinned client")),
            token: Mutex::new(TokenState::default()),
            rotation: ProxyRotator::new(),
        }
    }

    pub fn new_with(client: Box<dyn HttpClient>) -> Self {
        UpscaleV1Engine {
            client,
            token: Mutex::new(TokenState::default()),
            rotation: ProxyRotator::new(),
        }
    }

    fn suffix(&self, opts: &EngineOptions) -> String {
        opts.get("suffix")
            .and_then(|v| v.as_str())
            .unwrap_or("-4K")
            .to_string()
    }

    fn skip_existing(&self, opts: &EngineOptions) -> bool {
        opts.get("skip_existing")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn use_proxy(&self, opts: &EngineOptions) -> bool {
        opts.get("use_proxy")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
    }

    fn proxy_mode(&self, opts: &EngineOptions) -> String {
        opts.get("proxy_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("direct")
            .to_string()
    }

    fn proxy_list(&self, opts: &EngineOptions) -> Vec<String> {
        opts.get("proxy_list")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn fit_mode(&self, opts: &EngineOptions) -> String {
        opts.get("fit")
            .and_then(|v| v.as_str())
            .unwrap_or("none")
            .to_string()
    }

    /// Format decision: auto → png when the input PNG carries alpha (so
    /// transparency survives), else jpg. Explicit jpg/png win.
    fn format(&self, opts: &EngineOptions, input: &[u8]) -> &'static str {
        match opts.get("format").and_then(|v| v.as_str()) {
            Some("jpg") => "jpg",
            Some("png") => "png",
            _ => {
                if input.starts_with(b"\x89PNG") && img::has_alpha(input) {
                    "png"
                } else {
                    "jpg"
                }
            }
        }
    }

    /// Reuse the cached token until the budget is spent, then mint a fresh
    /// one (Firebase signUp is rate limited per IP). Tokens are per-proxy:
    /// rotating to a new IP mints through that proxy.
    async fn ensure_token(&self, proxy: Option<&str>) -> Result<String, EngineError> {
        {
            let mut st = self.token.lock().unwrap();
            if st.uses < TOKEN_BUDGET && st.token.is_some() && st.proxy.as_deref() == proxy {
                st.uses += 1;
                return Ok(st.token.clone().unwrap());
            }
        }
        let t = self.client.mint_token(proxy).await?;
        let mut st = self.token.lock().unwrap();
        st.token = Some(t.clone());
        st.proxy = proxy.map(|s| s.to_string());
        st.uses = 1;
        Ok(t)
    }

    fn invalidate_token(&self) {
        let mut st = self.token.lock().unwrap();
        st.token = None;
        st.uses = 0;
    }
}

impl Engine for UpscaleV1Engine {
    fn id(&self) -> &str {
        "upscale-v1"
    }

    fn name(&self) -> &str {
        "Image (Online)"
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
                id: "fit".into(),
                label: "Fit".into(),
                kind: OptionKind::Select(vec![
                    ("fit".into(), "Full-bleed".into()),
                    ("pad".into(), "+7% padding".into()),
                    ("none".into(), "Keep".into()),
                ]),
                default: serde_json::json!("none"),
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
                default: serde_json::json!("-4K"),
            },
            OptionDef {
                id: "use_proxy".into(),
                label: "Pakai proxy".into(),
                kind: OptionKind::Bool,
                default: serde_json::json!(true),
            },
        ]
    }

    fn output_name(&self, file: &Path, opts: &EngineOptions) -> String {
        let base = file.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
        format!("{base}{}.jpg", self.suffix(opts))
    }

    fn process<'a>(
        &'a self,
        file: &'a Path,
        out_dir: &'a Path,
        opts: &'a EngineOptions,
        _progress: Option<&'a crate::net::http::ProgressSink>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        Box::pin(async move {
            let base = file.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
            let img_bytes = std::fs::read(file)?;
            let format = self.format(opts, &img_bytes);
            let out_path = out_dir.join(format!("{base}{}.{}", self.suffix(opts), format));
            if self.skip_existing(opts) && out_path.exists() {
                return std::fs::read(&out_path).map_err(Into::into);
            }
            let input = image::load_from_memory(&img_bytes)
                .map_err(|e| EngineError::Other(format!("decode input: {e}")))?;

            let proxies: Vec<String> = if self.use_proxy(opts) && self.proxy_mode(opts) == "user" {
                self.proxy_list(opts)
                    .iter()
                    .filter_map(|p| ProxyRotator::normalize(p))
                    .collect()
            } else {
                Vec::new()
            };
            for attempt in 0..MAX_TOKEN_RETRIES {
                // Live proxy (reused until it fails), token minted per-IP.
                let proxy = if !proxies.is_empty() {
                    self.rotation
                        .next_live(&proxies, |_| std::future::ready(true))
                        .await
                } else {
                    None
                };
                let token = self.ensure_token(proxy.as_deref()).await?;
                match self
                    .client
                    .upscale_photoroom(&img_bytes, &token, format, proxy.as_deref())
                    .await
                {
                    Ok(bytes) => {
                        // PhotoRoom always returns 4× — output as-is.
                        let mut out_img = image::load_from_memory(&bytes)
                            .map_err(|e| EngineError::Other(format!("decode result: {e}")))?;
                        let fit = self.fit_mode(opts);
                        if format == "png" {
                            // PhotoRoom composites onto opaque background —
                            // rebuild alpha from the input.
                            out_img =
                                DynamicImage::ImageRgba8(img::restore_alpha(&input, &out_img));
                            if fit == "fit" {
                                if let Some(cropped) = img::full_bleed(&out_img) {
                                    out_img = DynamicImage::ImageRgba8(cropped);
                                }
                            }
                        }
                        if fit == "pad" {
                            out_img = DynamicImage::ImageRgba8(img::extend_pad(
                                &out_img,
                                0.07,
                                format == "png",
                            ));
                        }
                        let out = if format == "png" {
                            img::encode_png(&out_img).map_err(EngineError::Other)?
                        } else {
                            img::encode_jpeg(&out_img, JPEG_QUALITY).map_err(EngineError::Other)?
                        };
                        std::fs::write(&out_path, &out)?;
                        return Ok(out);
                    }
                    Err(EngineError::RateLimit) | Err(EngineError::Auth(_)) => {
                        self.invalidate_token();
                        // Rate limit per-IP → rotate ke proxy berikutnya kalau
                        // ada, kalau tidak backoff dan coba lagi.
                        if let Some(p) = &proxy {
                            self.rotation.mark_dead(p);
                        }
                        if attempt + 1 < MAX_TOKEN_RETRIES {
                            tokio::time::sleep(Duration::from_secs(2 * (attempt as u64 + 1))).await;
                            continue;
                        }
                        return Err(EngineError::RateLimit);
                    }
                    Err(e)
                        if proxy.is_some()
                            && ProxyRotator::is_transport_error(&e)
                            && attempt + 1 < MAX_PROXY_ROTATES =>
                    {
                        if let Some(p) = &proxy {
                            self.rotation.mark_dead(p);
                        }
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(EngineError::RateLimit)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::EngineOptions;
    use crate::net::http::tests::MockHttp;
    use image::GenericImageView;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn opts(pairs: &[(&str, serde_json::Value)]) -> EngineOptions {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("xix-up1-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tiny_rgba_png(pixels: [[u8; 4]; 4]) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(2, 2, |x, y| image::Rgba(pixels[(y * 2 + x) as usize]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    /// 4×4 opaque red PNG — simulates the photoroom "4× result".
    fn fake_4x_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 30, 30, 255]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn input_alpha_png() -> Vec<u8> {
        // 2×2: left column opaque, right column transparent
        tiny_rgba_png([
            [255, 0, 0, 255],
            [255, 0, 0, 0],
            [255, 0, 0, 255],
            [255, 0, 0, 0],
        ])
    }

    #[tokio::test]
    async fn photoroom_4x_keeps_dimensions() {
        let mock = MockHttp {
            upscale_out: fake_4x_png(),
            ..MockHttp::default()
        };
        let eng = UpscaleV1Engine::new_with(Box::new(mock));
        let dir = tempdir("scale4");
        let file = dir.join("in.png");
        std::fs::write(&file, input_alpha_png()).unwrap();
        let out = eng.process(&file, &dir, &opts(&[]), None).await.unwrap();
        assert!(out.starts_with(b"\x89PNG"));
        let dec = image::load_from_memory(&out).unwrap();
        assert_eq!(dec.dimensions(), (4, 4), "default scale 4 keeps 4× dims");
        assert!(dir.join("in-4K.png").exists());
    }

    #[tokio::test]
    async fn png_alpha_restored_from_input() {
        let mock = MockHttp {
            upscale_out: fake_4x_png(),
            ..MockHttp::default()
        };
        let eng = UpscaleV1Engine::new_with(Box::new(mock));
        let dir = tempdir("alpha");
        let file = dir.join("in.png");
        std::fs::write(&file, input_alpha_png()).unwrap();
        let out = eng.process(&file, &dir, &opts(&[]), None).await.unwrap();
        let dec = image::load_from_memory(&out).unwrap().to_rgba8();
        // 4× output, alpha resized from 2× input: left half opaque, right transparent
        assert_eq!(dec.get_pixel(0, 0)[3], 255);
        assert_eq!(dec.get_pixel(3, 0)[3], 0);
        assert_eq!(dec.get_pixel(1, 1)[3], 255);
        assert_eq!(dec.get_pixel(2, 1)[3], 0);
    }

    #[tokio::test]
    async fn full_bleed_crops_transparent_margins() {
        let mock = MockHttp {
            upscale_out: fake_4x_png(),
            ..MockHttp::default()
        };
        let eng = UpscaleV1Engine::new_with(Box::new(mock));
        let dir = tempdir("fit");
        let file = dir.join("in.png");
        std::fs::write(&file, input_alpha_png()).unwrap();
        let o = opts(&[("fit", serde_json::json!("fit"))]);
        let out = eng.process(&file, &dir, &o, None).await.unwrap();
        let dec = image::load_from_memory(&out).unwrap();
        // alpha restored (left half) then cropped → only left half remains
        assert_eq!(dec.dimensions(), (2, 4), "crop to opaque bbox");
    }

    #[tokio::test]
    async fn auth_error_rotates_token_and_retries() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mock = MockHttp {
            upscale_out: fake_4x_png(),
            upscale_auth_once: AtomicBool::new(true),
            log: log.clone(),
            ..MockHttp::default()
        };
        let eng = UpscaleV1Engine::new_with(Box::new(mock));
        let dir = tempdir("auth");
        let file = dir.join("in.png");
        std::fs::write(&file, input_alpha_png()).unwrap();
        let out = eng.process(&file, &dir, &opts(&[]), None).await.unwrap();
        assert!(!out.is_empty());
        let calls = log.lock().unwrap().clone();
        assert_eq!(
            calls.iter().filter(|c| c.starts_with("mint_token")).count(),
            2,
            "re-mint after 401: {calls:?}"
        );
        assert_eq!(calls.iter().filter(|c| c.starts_with("upscale")).count(), 2);
    }

    #[tokio::test]
    async fn proxy_mode_rotates_off_dead_proxy_to_success() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mock = MockHttp {
            upscale_out: fake_4x_png(),
            // proxy pertama mati (transport error) → harus pindah ke kedua
            transport_proxies: vec!["http://p1:8080".into()],
            log: log.clone(),
            ..MockHttp::default()
        };
        let eng = UpscaleV1Engine::new_with(Box::new(mock));
        let dir = tempdir("proxy");
        let file = dir.join("in.png");
        std::fs::write(&file, input_alpha_png()).unwrap();
        let o = opts(&[
            ("use_proxy", serde_json::json!(true)),
            ("proxy_mode", serde_json::json!("user")),
            (
                "proxy_list",
                serde_json::json!(["http://p1:8080", "http://p2:8080"]),
            ),
        ]);
        let out = eng.process(&file, &dir, &o, None).await.unwrap();
        assert!(!out.is_empty());
        let calls = log.lock().unwrap().clone();
        let ups: Vec<String> = calls
            .iter()
            .filter(|c| c.starts_with("upscale:"))
            .cloned()
            .collect();
        assert_eq!(
            ups,
            vec!["upscale:http://p1:8080", "upscale:http://p2:8080"],
            "p1 transport error → rotate ke p2: {ups:?}"
        );
        let mints: Vec<String> = calls
            .iter()
            .filter(|c| c.starts_with("mint_token:"))
            .cloned()
            .collect();
        assert_eq!(mints.len(), 2, "mint per proxy: {mints:?}");
    }

    #[tokio::test]
    async fn jpg_format_writes_jpeg() {
        let mock = MockHttp {
            upscale_out: fake_4x_png(),
            ..MockHttp::default()
        };
        let eng = UpscaleV1Engine::new_with(Box::new(mock));
        let dir = tempdir("jpg");
        let file = dir.join("in.png");
        std::fs::write(&file, input_alpha_png()).unwrap();
        let o = opts(&[("format", serde_json::json!("jpg"))]);
        let out = eng.process(&file, &dir, &o, None).await.unwrap();
        assert!(out.starts_with(b"\xFF\xD8"), "JPEG magic");
        assert!(dir.join("in-4K.jpg").exists());
    }

    #[test]
    fn output_name_uses_suffix() {
        let eng = UpscaleV1Engine::new_with(Box::new(MockHttp::default()));
        assert_eq!(eng.output_name(Path::new("a.png"), &opts(&[])), "a-4K.jpg");
        let o = opts(&[("suffix", serde_json::json!("-big"))]);
        assert_eq!(eng.output_name(Path::new("a.jpg"), &o), "a-big.jpg");
    }
}
