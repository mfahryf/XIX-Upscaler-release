//! L2+L3: HTTPS client for svg.new.
//!
//! L3 — every connection is pinned to svg.new's current SPKI (sha256, base64).
//! A custom `ServerCertVerifier` first does the full webpki chain validation
//! (via `WebPkiServerVerifier`) and then compares the end-entity certificate's
//! subjectPublicKeyInfo hash against the pin. mitmproxy-style interception
//! (a locally-installed CA) therefore fails the handshake without patching the
//! binary. If svg.new rotates its certificate, update `SVG_NEW_SPKI_B64`.
//!
//! L2 — all URLs / headers are embedded via `xstr!` (compile-time XOR), never
//! as plaintext, so `strings` on the binary reveals nothing.

use crate::engines::EngineError;
use crate::net::sse::{SseCollector, SseError};
use crate::secure::strings::{decrypt, XKEY};
use base64::Engine as _;
use futures_util::StreamExt;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use std::collections::HashMap;
use std::error::Error as _;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// svg.new SPKI sha256 (base64), captured 2026-08-14 (cert valid to Nov 2026).
pub const SVG_NEW_SPKI_B64: &str = "QrFgzzJtrp90N8bIgKBxe6QBCUxsaISk2mq0t7aeN3Y=";

/// Max idle gap (seconds) between SSE body chunks before the vectorize stream
/// is treated as a stalled/dead connection. Bounds `bytes_stream` reads, which
/// reqwest's own timeouts do not cover.
const SSE_IDLE_TIMEOUT_SECS: u64 = 30;

/// Max cached proxy clients per posture. Static proxy lists stay well under
/// this; Tor mode mints a fresh `user:pass` per request, so the cache is
/// cleared once it grows past the cap to avoid unbounded client/socket growth
/// across a large batch. Reuse within the cap keeps static lists fast.
const MAX_PROXY_CLIENTS: usize = 64;

/// Base64 of the sha256 hash of a DER SubjectPublicKeyInfo.
pub fn spki_sha256_base64(spki_der: &[u8]) -> String {
    use sha2::Digest;
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(spki_der))
}

/// Make sure the ring crypto provider is installed so reqwest can build its
/// default (unpinned) client for svgai. Safe to call repeatedly.
pub fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Format a reqwest error including its source chain. `to_string()` alone
/// yields the generic "error sending request for url (...)" even when the
/// real cause is a dead proxy / connection refused / timeout / TLS failure;
/// the detail lives in `source()`. Including it lets the UI show the true
/// cause and lets [`ProxyRotator::is_transport_error`] recognize these
/// transport failures so rotation kicks in.
fn reqwest_err(e: reqwest::Error) -> EngineError {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(&format!(": {s}"));
        src = s.source();
    }
    EngineError::Network(msg)
}

/// Build the pinned rustls `ClientConfig` (webpki chain + SPKI pin). Shared by
/// the direct client and every pinned proxy client (CONNECT tunnel keeps TLS
/// end-to-end, so the pin still applies through an HTTP/SOCKS proxy).
fn pinned_tls_config() -> Result<rustls::ClientConfig, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| e.to_string())?;
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            inner,
            pin: SVG_NEW_SPKI_B64.to_string(),
        }))
        .with_no_client_auth())
}

/// Custom verifier: webpki chain validation + SPKI pin check.
#[derive(Debug)]
struct PinnedVerifier {
    inner: Arc<WebPkiServerVerifier>,
    pin: String,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        let (_, cert) = x509_parser::parse_x509_certificate(end_entity.as_ref())
            .map_err(|e| rustls::Error::General(format!("cert parse failed: {e}").into()))?;
        let got = spki_sha256_base64(cert.tbs_certificate.subject_pki.raw);
        if got != self.pin {
            return Err(rustls::Error::General(
                format!("certificate pin mismatch (server SPKI {got})").into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// A boxed future — keeps the `HttpClient` trait object-safe (no async_trait
/// dependency needed).
pub type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Sender for live per-file progress percent (0..=100) from the v2 SSE stream.
/// The batch loop forwards these to the UI; `None` when no listener wants them.
pub type ProgressSink = tokio::sync::mpsc::UnboundedSender<u8>;

/// Abstraction over the third-party calls (svg.new for v2, svgai for v1) so
/// engines can be tested with a mock.
pub trait HttpClient: Send + Sync {
    /// v2: svg.new vectorize (SSE) → SVG string. `proxy` routes the request
    /// through a pinned proxy client (CONNECT keeps the SPKI pin valid).
    /// `progress` receives each `progress` percent parsed from the stream.
    fn vectorize<'a>(
        &'a self,
        image: &'a [u8],
        proxy: Option<&'a str>,
        progress: Option<&'a ProgressSink>,
    ) -> BoxFuture<'a, Result<String, EngineError>>;
    /// v2: svg.new edit/convert → format bytes (ai/dxf), same proxy routing.
    fn edit_convert<'a>(
        &'a self,
        svg: &'a str,
        format: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>>;
    /// v1: svgai multipart convert → SVG bytes; `proxy` routes the request
    /// (socks5://… or http://user:pass@…). 402 → `EngineError::Quota`,
    /// 429 → `EngineError::RateLimit`.
    fn svgai_convert<'a>(
        &'a self,
        image: &'a [u8],
        mime: &'a str,
        filename: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>>;
    /// v1: cheap paywall GET through `proxy` — is the proxy usable for svgai?
    /// `proxy` and `self` lifetimes are separate so callers can pass a
    /// short-lived candidate string into a borrowed-`self` future.
    fn svgai_proxy_alive<'a>(&'a self, proxy: &str) -> BoxFuture<'a, bool>;
    /// v2: cheap probe through `proxy` — is the proxy usable for svg.new?
    /// Uses a pinned proxy client (CONNECT keeps the SPKI pin valid) with a
    /// short timeout so dead/slow proxies are skipped instead of hanging the
    /// full vectorize request. Separate `proxy`/`self` lifetimes let callers
    /// pass a short-lived candidate string into a borrowed-`self` future.
    fn vectorize_proxy_alive<'a>(&'a self, proxy: &str) -> BoxFuture<'a, bool>;
    /// remove-bg: mint an anonymous Firebase idToken (photoroom). 429 (signUp
    /// per-IP rate limit) → retried with backoff, then `EngineError::RateLimit`.
    /// `proxy` routes both the signUp and the upload through the same IP.
    fn mint_token<'a>(
        &'a self,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, EngineError>>;
    /// remove-bg: POST image → photoroom segmentation luminance-mask PNG
    /// bytes (white = foreground). 429 → `RateLimit`, 401 → `Auth`.
    fn remove_bg<'a>(
        &'a self,
        image: &'a [u8],
        token: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>>;
    /// upscale-v1: photoroom /v3/upscale → upscaled image bytes (always 4×;
    /// the engine halves it for 2×). `format` = "jpg" | "png" (multipart
    /// outputFormat). 429 → `RateLimit`, 401/403 → `Auth` (re-mint token).
    fn upscale_photoroom<'a>(
        &'a self,
        image: &'a [u8],
        token: &'a str,
        format: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>>;
}

pub struct PinnedClient {
    client: reqwest::Client,
    /// Unpinned client for svgai (v1): accepts invalid certs because free
    /// proxies MITM TLS — same posture as the CLI. Payload is a public image
    /// + public SVG, no secrets transit.
    svgai: reqwest::Client,
    /// Photoroom (remove-bg): normal chain-verified TLS (no pin — public CA
    /// certs), long timeout for the segmentation upload.
    photoroom: reqwest::Client,
    /// One dedicated client per proxy URL (reqwest has no per-request proxy).
    proxy_clients: Mutex<HashMap<String, Arc<reqwest::Client>>>,
    /// Pinned TLS + proxy clients for svg.new (v2): CONNECT tunnel keeps the
    /// TLS pin verifiable end-to-end, so vectorize works from rotated IPs
    /// without dropping the L3 pin.
    pinned_proxy_clients: Mutex<HashMap<String, Arc<reqwest::Client>>>,
}

impl PinnedClient {
    pub fn new() -> Result<Self, String> {
        ensure_crypto_provider();
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(pinned_tls_config()?)
            .timeout(std::time::Duration::from_secs(90))
            .connect_timeout(std::time::Duration::from_secs(10))
            .user_agent(decrypt(
                crate::xstr!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"),
                XKEY,
            ))
            .build()
            .map_err(|e| e.to_string())?;
        let svgai = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(std::time::Duration::from_secs(60))
            .user_agent(decrypt(
                crate::xstr!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36"),
                XKEY,
            ))
            .build()
            .map_err(|e| e.to_string())?;
        let photoroom = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .user_agent(decrypt(
                crate::xstr!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"),
                XKEY,
            ))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(PinnedClient {
            client,
            svgai,
            photoroom,
            proxy_clients: Mutex::new(HashMap::new()),
            pinned_proxy_clients: Mutex::new(HashMap::new()),
        })
    }

    /// Get (or build) a client routed through `proxy` with the svgai posture.
    fn proxy_client(&self, proxy: &str) -> Result<Arc<reqwest::Client>, EngineError> {
        if let Some(c) = self.proxy_clients.lock().unwrap().get(proxy) {
            return Ok(c.clone());
        }
        let proxy_cfg =
            reqwest::Proxy::all(proxy).map_err(|e| EngineError::Other(e.to_string()))?;
        let c = Arc::new(
            reqwest::Client::builder()
                .proxy(proxy_cfg)
                .danger_accept_invalid_certs(true)
                .timeout(std::time::Duration::from_secs(60))
                .user_agent(decrypt(
                    crate::xstr!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36"),
                    XKEY,
                ))
                .build()
                .map_err(reqwest_err)?,
        );
        {
            let mut cache = self.proxy_clients.lock().unwrap();
            if cache.len() >= MAX_PROXY_CLIENTS {
                cache.clear();
            }
            cache.insert(proxy.to_string(), c.clone());
        }
        Ok(c)
    }

    /// Get (or build) a **pinned** client routed through `proxy` — used by v2
    /// (svg.new). CONNECT tunnels keep TLS end-to-end, so the SPKI pin from
    /// [`pinned_tls_config`] still applies while the source IP rotates.
    fn pinned_proxy_client(&self, proxy: &str) -> Result<Arc<reqwest::Client>, EngineError> {
        if let Some(c) = self.pinned_proxy_clients.lock().unwrap().get(proxy) {
            return Ok(c.clone());
        }
        let proxy_cfg =
            reqwest::Proxy::all(proxy).map_err(|e| EngineError::Other(e.to_string()))?;
        let tls = pinned_tls_config().map_err(EngineError::SslPin)?;
        let c = Arc::new(
            reqwest::Client::builder()
                .proxy(proxy_cfg)
                .use_preconfigured_tls(tls)
                .timeout(std::time::Duration::from_secs(90))
                .connect_timeout(std::time::Duration::from_secs(10))
                .user_agent(decrypt(
                    crate::xstr!("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36"),
                    XKEY,
                ))
                .build()
                .map_err(reqwest_err)?,
        );
        {
            let mut cache = self.pinned_proxy_clients.lock().unwrap();
            if cache.len() >= MAX_PROXY_CLIENTS {
                cache.clear();
            }
            cache.insert(proxy.to_string(), c.clone());
        }
        Ok(c)
    }
}

fn err_from_sse(e: SseError) -> EngineError {
    match e {
        SseError::Http(m) => EngineError::Network(m),
        SseError::Server(m) => EngineError::Other(m),
        SseError::NoEvent(m) => EngineError::Parse(m),
        SseError::Utf8 => EngineError::Parse("stream is not valid utf-8".into()),
    }
}

async fn vectorize_with(
    client: &reqwest::Client,
    image: &[u8],
    progress: Option<&ProgressSink>,
) -> Result<String, EngineError> {
    let url = decrypt(crate::xstr!("https://svg.new/api/image/vectorize"), XKEY);
    let data_uri = format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(image)
    );
    let body = serde_json::json!({ "image": data_uri });
    let resp = client
        .post(&url)
        .header(
            "content-type",
            decrypt(crate::xstr!("application/json"), XKEY),
        )
        .header("accept", decrypt(crate::xstr!("*/*"), XKEY))
        .header("origin", decrypt(crate::xstr!("https://svg.new"), XKEY))
        .header("referer", decrypt(crate::xstr!("https://svg.new/"), XKEY))
        .json(&body)
        .send()
        .await
        .map_err(reqwest_err)?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        let txt = resp.text().await.unwrap_or_default();
        let snippet: String = txt.chars().take(200).collect();
        return Err(EngineError::Network(format!("HTTP {status}: {snippet}")));
    }
    let mut collector = SseCollector::new("svg");
    let mut stream = resp.bytes_stream();
    // reqwest's request/connect timeout does NOT cover manual `bytes_stream`
    // body reads. svg.new returns 200 headers fast, then streams the SVG; a
    // flaky proxy can drop the tunnel mid-body, stalling `next()` forever with
    // no feedback. Bound each read with an idle timeout so a dead stream turns
    // into a transport error and the engine rotates to the next proxy.
    let idle = std::time::Duration::from_secs(SSE_IDLE_TIMEOUT_SECS);
    loop {
        match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(chunk)) => {
                collector.push(&chunk.map_err(reqwest_err)?);
                if let (Some(sink), Some(pct)) = (progress, collector.take_progress()) {
                    let _ = sink.send(pct); // receiver gone → ignore
                }
            }
            Ok(None) => break,
            Err(_) => {
                return Err(EngineError::Network(format!(
                    "error sending request for url ({url}): stream idle > {SSE_IDLE_TIMEOUT_SECS}s (proxy stalled mid-response)"
                )))
            }
        }
    }
    collector.finish().map_err(err_from_sse)
}

async fn svgai_convert_with(
    client: &reqwest::Client,
    image: &[u8],
    mime: &str,
    filename: &str,
) -> Result<Vec<u8>, EngineError> {
    let url = decrypt(
        crate::xstr!("https://www.svgai.org/api/convert/image-to-svg"),
        XKEY,
    );
    let flow_id = uuid::Uuid::new_v4().to_string();
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let acq_id = uuid::Uuid::new_v4().to_string();
    let size = image.len().to_string();
    let form = reqwest::multipart::Form::new()
        .text("converter_flow_id", flow_id.clone())
        .text("converter_attempt_id", attempt_id.clone())
        .text("acquisition_flow_id", acq_id)
        .text("source_page", "/convert/image-to-svg")
        .text("referrer", "https://www.google.com/")
        .text("client_preprocess_status", "not_needed")
        .text("client_original_size_bytes", size.clone())
        .text("client_processed_size_bytes", size)
        .text("client_original_mime_type", mime.to_string())
        .text("client_processed_mime_type", mime.to_string())
        .text("client_original_filename", filename.to_string())
        .part(
            "file",
            reqwest::multipart::Part::bytes(image.to_vec())
                .file_name(filename.to_string())
                .mime_str(mime)
                .map_err(|e| EngineError::Parse(e.to_string()))?,
        );
    let resp = client
        .post(&url)
        .header("accept", decrypt(crate::xstr!("*/*"), XKEY))
        .header(
            "referer",
            decrypt(
                crate::xstr!("https://www.svgai.org/convert/image-to-svg"),
                XKEY,
            ),
        )
        .header("x-svgai-converter-attempt-id", attempt_id)
        .header("x-svgai-converter-flow-id", flow_id)
        .multipart(form)
        .send()
        .await
        .map_err(reqwest_err)?;
    let status = resp.status();
    if status == reqwest::StatusCode::PAYMENT_REQUIRED {
        return Err(EngineError::Quota("svgai quota exhausted (402)".into()));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(EngineError::RateLimit);
    }
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let snippet: String = txt.chars().take(160).collect();
        return Err(EngineError::Network(format!("HTTP {status}: {snippet}")));
    }
    let bytes = resp.bytes().await.map_err(reqwest_err)?;
    if !bytes.windows(4).any(|w| w == b"<svg") {
        return Err(EngineError::Parse("response bukan SVG".into()));
    }
    Ok(bytes.to_vec())
}

async fn svgai_proxy_alive_with(client: &reqwest::Client) -> bool {
    let url = decrypt(
        crate::xstr!("https://www.svgai.org/api/convert/image-to-svg/paywall/status"),
        XKEY,
    );
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(6))
        .send()
        .await;
    matches!(resp, Ok(r) if r.status() == reqwest::StatusCode::OK)
}

async fn vectorize_proxy_alive_with(client: &reqwest::Client) -> bool {
    // Cheap reachability probe: any HTTP response through the proxy means the
    // CONNECT tunnel + TLS pin succeeded, so the proxy is usable for vectorize.
    // A short timeout keeps dead/slow proxies from stalling the batch.
    let url = decrypt(crate::xstr!("https://svg.new/"), XKEY);
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    resp.is_ok()
}

async fn edit_convert_with(
    client: &reqwest::Client,
    svg: &str,
    format: &str,
) -> Result<Vec<u8>, EngineError> {
    let url = decrypt(crate::xstr!("https://svg.new/api/agent/edit/convert"), XKEY);
    let body = serde_json::json!({ "svg": svg, "format": format });
    let resp = client
        .post(&url)
        .header(
            "content-type",
            decrypt(crate::xstr!("application/json"), XKEY),
        )
        .header("accept", decrypt(crate::xstr!("*/*"), XKEY))
        .header("origin", decrypt(crate::xstr!("https://svg.new"), XKEY))
        .header(
            "referer",
            decrypt(crate::xstr!("https://svg.new/edit"), XKEY),
        )
        .json(&body)
        .send()
        .await
        .map_err(reqwest_err)?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        let txt = resp.text().await.unwrap_or_default();
        let snippet: String = txt.chars().take(200).collect();
        return Err(EngineError::Network(format!("HTTP {status}: {snippet}")));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let bytes = resp.bytes().await.map_err(reqwest_err)?;
    // dxf comes JSON-wrapped: {"content":"..."} — unwrap it; everything else is raw binary.
    if ct.contains("json") {
        if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(content) = parsed.get("content").and_then(|c| c.as_str()) {
                return Ok(content.as_bytes().to_vec());
            }
        }
    }
    Ok(bytes.to_vec())
}

/// Photoroom (remove-bg): anonymous Firebase signUp → `idToken`.
/// Firebase rate-limits signUp per IP (429 TOO_MANY_ATTEMPTS_TRY_LATER), so
/// retry with exponential backoff; exhausted retries surface as
/// `EngineError::RateLimit` (stops the batch, like svgai 429).
async fn mint_token_with(client: &reqwest::Client) -> Result<String, EngineError> {
    let url = decrypt(
        crate::xstr!("https://identitytoolkit.googleapis.com/v1/accounts:signUp?key=AIzaSyBEEHOLo47ucxzV2T1NLRoCAflGxTWb_sw"),
        XKEY,
    );
    let body = serde_json::json!({ "returnSecureToken": true });
    let mut saw_rate_limit = false;
    for attempt in 0..5u32 {
        match client
            .post(&url)
            .header(
                "content-type",
                decrypt(crate::xstr!("application/json"), XKEY),
            )
            .json(&body)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = r
                    .json()
                    .await
                    .map_err(|e| EngineError::Parse(e.to_string()))?;
                return v
                    .get("idToken")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .ok_or_else(|| EngineError::Parse("no idToken in signUp response".into()));
            }
            Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                saw_rate_limit = true;
            }
            Ok(r) => {
                let status = r.status();
                let txt = r.text().await.unwrap_or_default();
                let snippet: String = txt.chars().take(160).collect();
                return Err(EngineError::Network(format!("HTTP {status}: {snippet}")));
            }
            Err(_) => {
                saw_rate_limit = false;
                // transient transport error — backoff and retry
                tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
                continue;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
    }
    if saw_rate_limit {
        Err(EngineError::RateLimit)
    } else {
        Err(EngineError::Network(
            "token mint: transport retries exhausted".into(),
        ))
    }
}

/// Photoroom (remove-bg): multipart POST → segmentation response JSON with
/// `b64_mask` (base64 PNG, luminance map: white = foreground). Returns the
/// decoded mask PNG bytes.
async fn remove_bg_with(
    client: &reqwest::Client,
    image: &[u8],
    token: &str,
) -> Result<Vec<u8>, EngineError> {
    let url = decrypt(
        crate::xstr!("https://segmentation-inference.photoroom.com/v1/upload"),
        XKEY,
    );
    let fname = decrypt(crate::xstr!("blob"), XKEY);
    let mime = decrypt(crate::xstr!("image/jpeg"), XKEY);
    let form = reqwest::multipart::Form::new().part(
        "sourceImage",
        reqwest::multipart::Part::bytes(image.to_vec())
            .file_name(fname)
            .mime_str(&mime)
            .map_err(|e| EngineError::Parse(e.to_string()))?,
    );
    let resp = client
        .post(&url)
        .header("accept", decrypt(crate::xstr!("*/*"), XKEY))
        .header("authorization", token)
        .header(
            "pr-app-version",
            decrypt(crate::xstr!("2026.27.01 (33bc9b4)"), XKEY),
        )
        .header("pr-platform", decrypt(crate::xstr!("web"), XKEY))
        .header("pr-user-bcp-language", decrypt(crate::xstr!("en-US"), XKEY))
        .header(
            "pr-user-timezone",
            decrypt(crate::xstr!("Asia/Jakarta"), XKEY),
        )
        .multipart(form)
        .send()
        .await
        .map_err(reqwest_err)?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(EngineError::RateLimit);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(EngineError::Auth("token expired/invalid (401)".into()));
    }
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let snippet: String = txt.chars().take(200).collect();
        return Err(EngineError::Network(format!("HTTP {status}: {snippet}")));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| EngineError::Parse(e.to_string()))?;
    let b64 = v
        .get("b64_mask")
        .and_then(|m| m.as_str())
        .ok_or_else(|| EngineError::Parse("no b64_mask in segmentation response".into()))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| EngineError::Parse(e.to_string()))
}

/// Photoroom (upscale-v1): multipart POST → upscaled image bytes. PhotoRoom
/// ignores any scale param and always returns 4× the input dimensions; the
/// engine halves the result for 2× (neural detail preserved).
async fn upscale_photoroom_with(
    client: &reqwest::Client,
    image: &[u8],
    token: &str,
    format: &str,
) -> Result<Vec<u8>, EngineError> {
    let url = decrypt(
        crate::xstr!("https://serverless-api.photoroom.com/v3/upscale"),
        XKEY,
    );
    let fname = decrypt(crate::xstr!("blob"), XKEY);
    let mime = decrypt(crate::xstr!("image/png"), XKEY);
    let form = reqwest::multipart::Form::new()
        .part(
            "imageFile",
            reqwest::multipart::Part::bytes(image.to_vec())
                .file_name(fname)
                .mime_str(&mime)
                .map_err(|e| EngineError::Parse(e.to_string()))?,
        )
        .text("outputFormat", format.to_string());
    let resp = client
        .post(&url)
        .header("accept", decrypt(crate::xstr!("*/*"), XKEY))
        .header("authorization", token)
        .header(
            "pr-app-version",
            decrypt(crate::xstr!("2026.31.01 (ffefead)"), XKEY),
        )
        .header(
            "pr-current-space-entitlement",
            decrypt(crate::xstr!("unknown"), XKEY),
        )
        .header("pr-main-subject-id", decrypt(crate::xstr!("not_set"), XKEY))
        .header("pr-platform", decrypt(crate::xstr!("web"), XKEY))
        .header("pr-telemetry-enabled", decrypt(crate::xstr!("false"), XKEY))
        .header("pr-user-bcp-language", decrypt(crate::xstr!("en-US"), XKEY))
        .header(
            "pr-user-timezone",
            decrypt(crate::xstr!("Asia/Jakarta"), XKEY),
        )
        .multipart(form)
        .send()
        .await
        .map_err(reqwest_err)?;
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(EngineError::RateLimit);
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(EngineError::Auth("token expired/invalid (401/403)".into()));
    }
    if !status.is_success() {
        let txt = resp.text().await.unwrap_or_default();
        let snippet: String = txt.chars().take(200).collect();
        return Err(EngineError::Network(format!("HTTP {status}: {snippet}")));
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !ct.contains("image") {
        let txt = resp.text().await.unwrap_or_default();
        let snippet: String = txt.chars().take(200).collect();
        return Err(EngineError::Network(format!(
            "response bukan gambar ({ct}): {snippet}"
        )));
    }
    resp.bytes().await.map(|b| b.to_vec()).map_err(reqwest_err)
}

impl HttpClient for PinnedClient {
    fn vectorize<'a>(
        &'a self,
        image: &'a [u8],
        proxy: Option<&'a str>,
        progress: Option<&'a ProgressSink>,
    ) -> BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(async move {
            let client = match proxy {
                Some(p) => self.pinned_proxy_client(p)?,
                None => Arc::new(self.client.clone()),
            };
            vectorize_with(&client, image, progress).await
        })
    }

    fn edit_convert<'a>(
        &'a self,
        svg: &'a str,
        format: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        Box::pin(async move {
            let client = match proxy {
                Some(p) => self.pinned_proxy_client(p)?,
                None => Arc::new(self.client.clone()),
            };
            edit_convert_with(&client, svg, format).await
        })
    }

    fn svgai_convert<'a>(
        &'a self,
        image: &'a [u8],
        mime: &'a str,
        filename: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        Box::pin(async move {
            let client = match proxy {
                Some(p) => self.proxy_client(p)?,
                None => Arc::new(self.svgai.clone()),
            };
            svgai_convert_with(&client, image, mime, filename).await
        })
    }

    fn svgai_proxy_alive<'a>(&'a self, proxy: &str) -> BoxFuture<'a, bool> {
        let p = proxy.to_string();
        Box::pin(async move {
            let client = match self.proxy_client(&p) {
                Ok(c) => c,
                Err(_) => return false,
            };
            svgai_proxy_alive_with(&client).await
        })
    }

    fn vectorize_proxy_alive<'a>(&'a self, proxy: &str) -> BoxFuture<'a, bool> {
        let p = proxy.to_string();
        Box::pin(async move {
            let client = match self.pinned_proxy_client(&p) {
                Ok(c) => c,
                Err(_) => return false,
            };
            vectorize_proxy_alive_with(&client).await
        })
    }

    fn mint_token<'a>(
        &'a self,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(async move {
            let client = match proxy {
                Some(p) => self.proxy_client(p)?,
                None => Arc::new(self.photoroom.clone()),
            };
            mint_token_with(&client).await
        })
    }

    fn remove_bg<'a>(
        &'a self,
        image: &'a [u8],
        token: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        Box::pin(async move {
            let client = match proxy {
                Some(p) => self.proxy_client(p)?,
                None => Arc::new(self.photoroom.clone()),
            };
            remove_bg_with(&client, image, token).await
        })
    }

    fn upscale_photoroom<'a>(
        &'a self,
        image: &'a [u8],
        token: &'a str,
        format: &'a str,
        proxy: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
        Box::pin(async move {
            let client = match proxy {
                Some(p) => self.proxy_client(p)?,
                None => Arc::new(self.photoroom.clone()),
            };
            upscale_photoroom_with(&client, image, token, format).await
        })
    }
}

// Manual Debug (reqwest::Client has no Debug that we need) — harmless.
impl fmt::Debug for PinnedClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedClient").finish_non_exhaustive()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn spki_hash_matches_known_vector() {
        // DER-wrapped sha256("abc") as a fake SPKI
        let der = [
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20, 0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41,
            0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a,
            0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
        ];
        let got = spki_sha256_base64(&der);
        assert_eq!(got, "opgHq9eatZMaHdh1i6NhHrs7bh7+5R/4HNZSrDRzko4=");
    }

    #[test]
    fn endpoints_obfuscated_and_decryptable() {
        let url = decrypt(crate::xstr!("https://svg.new/api/image/vectorize"), XKEY);
        assert_eq!(url, "https://svg.new/api/image/vectorize");
        let enc = crate::xstr!("https://svg.new/api/image/vectorize");
        assert!(!String::from_utf8_lossy(enc).contains("svg.new"));
    }

    #[test]
    fn upscale_endpoint_obfuscated_and_decryptable() {
        let url = decrypt(
            crate::xstr!("https://serverless-api.photoroom.com/v3/upscale"),
            XKEY,
        );
        assert_eq!(url, "https://serverless-api.photoroom.com/v3/upscale");
        let enc = crate::xstr!("https://serverless-api.photoroom.com/v3/upscale");
        assert!(!String::from_utf8_lossy(enc).contains("photoroom"));
    }

    #[test]
    fn photoroom_endpoints_obfuscated_and_decryptable() {
        let url = decrypt(
            crate::xstr!("https://segmentation-inference.photoroom.com/v1/upload"),
            XKEY,
        );
        assert_eq!(
            url,
            "https://segmentation-inference.photoroom.com/v1/upload"
        );
        let enc = crate::xstr!("https://segmentation-inference.photoroom.com/v1/upload");
        assert!(!String::from_utf8_lossy(enc).contains("photoroom"));
        let fb = decrypt(
            crate::xstr!("https://identitytoolkit.googleapis.com/v1/accounts:signUp"),
            XKEY,
        );
        assert!(fb.contains("identitytoolkit"));
        let enc2 = crate::xstr!("https://identitytoolkit.googleapis.com/v1/accounts:signUp");
        assert!(!String::from_utf8_lossy(enc2).contains("identitytoolkit"));
    }

    /// A mock `HttpClient` used by engine tests: returns canned results and
    /// logs every call (proxy rotation tests assert on `log`).
    pub struct MockHttp {
        pub svg: String,           // v2 vectorize result
        pub export: Vec<u8>,       // v2 edit_convert result
        pub fail: bool,            // v2: force an error
        pub v1_svg: Vec<u8>,       // v1 convert result (success)
        pub v1_quota: Vec<String>, // v1: proxies whose convert returns 402
        /// v1: proxy names whose convert returns HTTP 200 with a non-SVG body
        /// (simulates a free proxy injecting its own block page).
        pub v1_not_svg: Vec<String>,
        pub alive: bool,      // v1: proxy health-check result
        pub rb_mask: Vec<u8>, // remove-bg mask result (PNG bytes)
        pub rb_fail: bool,    // remove-bg: force an error
        pub rb_auth_once: std::sync::atomic::AtomicBool, // 1st remove-bg → Auth (token rotation test)
        pub upscale_out: Vec<u8>,                        // upscale-v1 result (image bytes)
        pub upscale_fail: bool,                          // upscale-v1: force an error
        pub upscale_auth_once: std::sync::atomic::AtomicBool, // 1st upscale → Auth
        pub token: String,                               // mint_token result
        pub mint_fail: bool,                             // mint_token: force an error
        /// v2: proxy names (or "direct") whose vectorize returns HTTP 403 —
        /// used by the proxy-rotation test.
        pub v2_403_proxies: Vec<String>,
        /// remove-bg / upscale: proxy names whose call returns a transport
        /// error — simulates a dead proxy that must be rotated past.
        pub transport_proxies: Vec<String>,
        pub log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Default for MockHttp {
        fn default() -> Self {
            MockHttp {
                svg: String::new(),
                export: Vec::new(),
                fail: false,
                v1_svg: b"<svg/>".to_vec(),
                v1_quota: Vec::new(),
                v1_not_svg: Vec::new(),
                alive: true,
                rb_mask: b"mask".to_vec(),
                rb_fail: false,
                rb_auth_once: std::sync::atomic::AtomicBool::new(false),
                upscale_out: b"upscaled".to_vec(),
                upscale_fail: false,
                upscale_auth_once: std::sync::atomic::AtomicBool::new(false),
                token: "tok".into(),
                mint_fail: false,
                v2_403_proxies: Vec::new(),
                transport_proxies: Vec::new(),
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl HttpClient for MockHttp {
        fn vectorize<'a>(
            &'a self,
            _image: &'a [u8],
            proxy: Option<&'a str>,
            progress: Option<&'a ProgressSink>,
        ) -> BoxFuture<'a, Result<String, EngineError>> {
            if let Some(sink) = progress {
                let _ = sink.send(100); // mock emits a single terminal tick
            }
            let p = proxy.unwrap_or("direct").to_string();
            self.log.lock().unwrap().push(format!("vectorize:{p}"));
            let out = if self.fail {
                Err(EngineError::Other("mock failure".into()))
            } else if self.v2_403_proxies.contains(&p) {
                Err(EngineError::Network("HTTP 403 Forbidden: blocked".into()))
            } else if self.transport_proxies.contains(&p) {
                // pesan persis seperti yang muncul di UI (transport error)
                Err(EngineError::Network(
                    "error sending request for url (https://svg.new/api/image/vectorize)".into(),
                ))
            } else {
                Ok(self.svg.clone())
            };
            Box::pin(std::future::ready(out))
        }

        fn edit_convert<'a>(
            &'a self,
            _svg: &'a str,
            _format: &'a str,
            proxy: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let p = proxy.unwrap_or("direct").to_string();
            self.log.lock().unwrap().push(format!("edit_convert:{p}"));
            let out = if self.fail {
                Err(EngineError::Other("mock failure".into()))
            } else {
                Ok(self.export.clone())
            };
            Box::pin(std::future::ready(out))
        }

        fn svgai_convert<'a>(
            &'a self,
            _image: &'a [u8],
            _mime: &'a str,
            _filename: &'a str,
            proxy: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let p = proxy.unwrap_or("direct").to_string();
            self.log.lock().unwrap().push(format!("svgai_convert:{p}"));
            let out = if self.v1_quota.iter().any(|q| q == &p) {
                Err(EngineError::Quota("mock 402".into()))
            } else if self.v1_not_svg.iter().any(|q| q == &p) {
                // HTTP 200 tapi body bukan SVG (block page proxy) — persis
                // seperti svgai_convert_with yang mem-parsing respons nyata.
                Err(EngineError::Parse("response bukan SVG".into()))
            } else if self.fail {
                Err(EngineError::Other("mock failure".into()))
            } else {
                Ok(self.v1_svg.clone())
            };
            Box::pin(std::future::ready(out))
        }

        fn svgai_proxy_alive<'a>(&'a self, proxy: &str) -> BoxFuture<'a, bool> {
            self.log.lock().unwrap().push(format!("alive:{proxy}"));
            Box::pin(std::future::ready(self.alive))
        }

        fn vectorize_proxy_alive<'a>(&'a self, proxy: &str) -> BoxFuture<'a, bool> {
            self.log.lock().unwrap().push(format!("v2_alive:{proxy}"));
            // Dead-transport proxies fail the probe; everything else is usable.
            let ok = self.alive && !self.transport_proxies.iter().any(|p| p == proxy);
            Box::pin(std::future::ready(ok))
        }

        fn mint_token<'a>(
            &'a self,
            proxy: Option<&'a str>,
        ) -> BoxFuture<'a, Result<String, EngineError>> {
            let p = proxy.unwrap_or("direct").to_string();
            self.log.lock().unwrap().push(format!("mint_token:{p}"));
            let out = if self.mint_fail {
                Err(EngineError::Other("mint failed".into()))
            } else {
                Ok(self.token.clone())
            };
            Box::pin(std::future::ready(out))
        }

        fn remove_bg<'a>(
            &'a self,
            _image: &'a [u8],
            _token: &'a str,
            proxy: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let p = proxy.unwrap_or("direct").to_string();
            self.log.lock().unwrap().push(format!("remove_bg:{p}"));
            let out = if self.transport_proxies.contains(&p) {
                Err(EngineError::Network("connection closed by proxy".into()))
            } else if self.rb_fail {
                Err(EngineError::Other("mock failure".into()))
            } else if self
                .rb_auth_once
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Err(EngineError::Auth("mock 401".into()))
            } else {
                Ok(self.rb_mask.clone())
            };
            Box::pin(std::future::ready(out))
        }

        fn upscale_photoroom<'a>(
            &'a self,
            _image: &'a [u8],
            _token: &'a str,
            _format: &'a str,
            proxy: Option<&'a str>,
        ) -> BoxFuture<'a, Result<Vec<u8>, EngineError>> {
            let p = proxy.unwrap_or("direct").to_string();
            self.log.lock().unwrap().push(format!("upscale:{p}"));
            let out = if self.transport_proxies.contains(&p) {
                Err(EngineError::Network("connection closed by proxy".into()))
            } else if self.upscale_fail {
                Err(EngineError::Other("mock failure".into()))
            } else if self
                .upscale_auth_once
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                Err(EngineError::Auth("mock 401".into()))
            } else {
                Ok(self.upscale_out.clone())
            };
            Box::pin(std::future::ready(out))
        }
    }
}
