//! Public-client OAuth primitives. Secret values never appear in diagnostics.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fmt, path::{Path, PathBuf}, sync::Arc};
use uuid::Uuid;

use super::{debug, vault::TokenVault};

pub const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const PRODUCTION_CLIENT_ID: &str =
    "167537907895-rp94d606ic46esvhjcp184roqikh0v6k.apps.googleusercontent.com";

#[derive(Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    client_secret: Option<String>,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub revoke_endpoint: Url,
    pub scopes: Vec<String>,
}

impl fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("authorization_endpoint", &self.authorization_endpoint)
            .field("token_endpoint", &self.token_endpoint)
            .field("revoke_endpoint", &self.revoke_endpoint)
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl OAuthConfig {
    pub fn production() -> Self {
        Self {
            client_id: PRODUCTION_CLIENT_ID.into(),
            client_secret: None,
            authorization_endpoint: Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
                .unwrap(),
            token_endpoint: Url::parse("https://oauth2.googleapis.com/token").unwrap(),
            revoke_endpoint: Url::parse("https://oauth2.googleapis.com/revoke").unwrap(),
            scopes: vec![DRIVE_FILE_SCOPE.into()],
        }
    }

    fn from_local_environment() -> Self {
        let mut config = Self::production();
        let dotenv_path = std::env::var_os("XIX_GOOGLE_ENV_FILE")
            .map(PathBuf::from)
            .or_else(find_dotenv);
        let Some(dotenv_path) = dotenv_path else {
            debug::event("oauth", "credential OAuth lokal tidak ditemukan");
            return config;
        };
        let values = match read_dotenv(&dotenv_path) {
            Ok(values) => values,
            Err(_) => {
                debug::event("oauth", "credential OAuth lokal tidak bisa dibaca");
                return config;
            }
        };
        let client_id = std::env::var("XIX_GOOGLE_CLIENT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| values.get("XIX_GOOGLE_CLIENT_ID").cloned());
        let client_secret = std::env::var("XIX_GOOGLE_CLIENT_SECRET")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| values.get("XIX_GOOGLE_CLIENT_SECRET").cloned());
        debug::event(
            "oauth",
            format!(
                "credential OAuth lokal dibaca; client_id ada={}, client_secret ada={}",
                client_id.is_some(),
                client_secret.is_some()
            ),
        );
        match (client_id, client_secret) {
            (Some(client_id), Some(client_secret))
                if client_id == config.client_id && valid_client_secret(&client_secret) =>
            {
                config.client_secret = Some(client_secret);
                debug::event(
                    "oauth",
                    "credential OAuth lokal dimuat; client_id cocok=true, client_secret tersedia=true",
                );
            }
            (Some(client_id), _) if client_id != config.client_id => {
                debug::event("oauth", "credential OAuth lokal diabaikan; client_id tidak cocok");
            }
            _ => {
                debug::event(
                    "oauth",
                    "credential OAuth lokal tidak lengkap atau client_secret tidak valid",
                );
            }
        }
        config
    }
}

fn find_dotenv() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    for root in roots {
        let mut directory = Some(root.as_path());
        for _ in 0..6 {
            let Some(directory_path) = directory else {
                break;
            };
            let candidate = directory_path.join(".env");
            if candidate.is_file() {
                return Some(candidate);
            }
            directory = directory_path.parent();
        }
    }
    None
}

fn read_dotenv(path: &Path) -> Result<HashMap<String, String>, ()> {
    let text = std::fs::read_to_string(path).map_err(|_| ())?;
    if text.len() > 64 * 1024 {
        return Err(());
    }
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key != "XIX_GOOGLE_CLIENT_ID" && key != "XIX_GOOGLE_CLIENT_SECRET" {
            continue;
        }
        let mut value = value.trim().to_string();
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        values.insert(key.to_string(), value);
    }
    Ok(values)
}

fn valid_client_secret(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 * 1024 && value.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    InvalidRedirect,
    InvalidCallback,
    StateMismatch,
    Denied,
    Storage,
    Configuration,
    InvalidRequest,
    ReconnectRequired,
    Network,
    Cancelled,
    Timeout,
    Browser,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRedirect => "Alamat balasan login tidak sah",
            Self::InvalidCallback => "Balasan login Google tidak sah; coba hubungkan ulang",
            Self::StateMismatch => "Balasan login tidak cocok dengan permintaan aplikasi",
            Self::Denied => "Izin Google Drive tidak diberikan",
            Self::Storage => "Data izin Google Drive tidak dapat dibaca atau disimpan dengan aman",
            Self::Configuration => {
                "Konfigurasi OAuth Google tidak cocok; periksa client ID aplikasi Desktop"
            }
            Self::InvalidRequest => {
                "Permintaan OAuth Google ditolak; lihat detail di terminal debug"
            }
            Self::ReconnectRequired => "Hubungkan ulang akun Google Drive",
            Self::Network => {
                "Google tidak merespons setelah login; periksa internet, proxy, atau firewall"
            }
            Self::Cancelled => "Login Google dibatalkan",
            Self::Timeout => "Waktu login Google habis; coba hubungkan kembali",
            Self::Browser => "Browser untuk login Google tidak dapat dibuka",
        })
    }
}

impl std::error::Error for AuthError {}

impl AuthError {
    pub fn requires_reconnect(&self) -> bool {
        matches!(self, Self::ReconnectRequired)
    }
}

pub type AuthFuture<'a, T> = futures_util::future::BoxFuture<'a, Result<T, AuthError>>;

/// Shared with the Drive client. Invalidating access never deletes refresh permission.
pub trait AccessTokenProvider: Send + Sync {
    fn access_token(&self) -> AuthFuture<'_, String>;
    fn invalidate_access_token(&self) -> AuthFuture<'_, ()>;
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct AuthStatus {
    pub connected: bool,
    pub masked_account: Option<String>,
}

#[derive(Default)]
struct ManagerState {
    vault: Option<Arc<TokenVault>>,
    refresh_token: Option<String>,
    tokens: Option<TokenSet>,
}

pub struct OAuthManager {
    state: parking_lot::Mutex<ManagerState>,
    operation: tokio::sync::Mutex<()>,
    runtime: AuthRuntime,
    cancellation: tokio::sync::watch::Sender<u64>,
}

struct AuthTimeouts {
    login: std::time::Duration,
    request: std::time::Duration,
    token: std::time::Duration,
    revoke: std::time::Duration,
}

struct AuthRuntime {
    config: OAuthConfig,
    open_browser: Arc<dyn Fn(&Url) -> Result<(), AuthError> + Send + Sync>,
    now: Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>,
    timeouts: AuthTimeouts,
}

impl Default for OAuthManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthManager {
    pub fn new() -> Self {
        Self {
            state: parking_lot::Mutex::new(ManagerState::default()),
            operation: tokio::sync::Mutex::new(()),
            cancellation: tokio::sync::watch::channel(0).0,
            runtime: AuthRuntime {
                config: OAuthConfig::production(),
                open_browser: Arc::new(open_authorization_browser),
                now: Arc::new(chrono::Utc::now),
                timeouts: AuthTimeouts {
                    login: std::time::Duration::from_secs(180),
                    request: std::time::Duration::from_secs(5),
                    token: std::time::Duration::from_secs(30),
                    revoke: std::time::Duration::from_secs(5),
                },
            },
        }
    }

    pub fn new_with_local_environment() -> Self {
        let mut manager = Self::new();
        manager.runtime.config = OAuthConfig::from_local_environment();
        manager
    }

    /// Called once from application setup. Never opens a browser or uses the network.
    pub fn initialize(&self, app_data_dir: PathBuf) -> Result<(), AuthError> {
        let mut state = self.state.lock();
        if state.vault.is_some() {
            return Err(AuthError::Storage);
        }
        let vault = Arc::new(TokenVault::new(&app_data_dir));
        state.vault = Some(vault.clone());
        state.refresh_token = vault.load()?.map(|login| login.refresh_token);
        debug::event(
            "oauth",
            format!(
                "penyimpanan kredensial siap; refresh token tersimpan: {}",
                state.refresh_token.is_some()
            ),
        );
        Ok(())
    }

    pub fn status(&self) -> AuthStatus {
        AuthStatus {
            connected: self.state.lock().refresh_token.is_some(),
            masked_account: None,
        }
    }

    pub async fn connect(&self) -> Result<AuthStatus, AuthError> {
        debug::event("oauth", "permintaan koneksi dimulai");
        let mut cancel = self.cancellation.subscribe();
        let _operation = self.operation.lock().await;
        if cancel.has_changed().unwrap_or(true) {
            debug::event("oauth", "koneksi dibatalkan sebelum dimulai");
            return Err(AuthError::Cancelled);
        }
        if self.status().connected {
            debug::event("oauth", "koneksi dilewati; refresh token sudah tersedia");
            return Ok(self.status());
        }
        if self.state.lock().vault.is_none() {
            debug::event("oauth", "koneksi gagal; penyimpanan kredensial belum siap");
            return Err(AuthError::Storage);
        }
        let login = async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|_| AuthError::InvalidRedirect)?;
            let address = listener
                .local_addr()
                .map_err(|_| AuthError::InvalidRedirect)?;
            let attempt = PkceAttempt::new(format!("http://{address}/"))?;
            (self.runtime.open_browser)(&attempt.authorization_url(&self.runtime.config, false))?;
            debug::event("oauth", "browser OAuth dibuka; menunggu callback lokal");
            let (mut socket, peer) = listener
                .accept()
                .await
                .map_err(|_| AuthError::InvalidCallback)?;
            drop(listener);
            if !peer.ip().is_loopback() {
                debug::event("oauth", "callback ditolak; sumber bukan loopback");
                return Err(AuthError::InvalidCallback);
            }
            let code = tokio::time::timeout(
                self.runtime.timeouts.request,
                read_callback(&mut socket, address, &attempt.state),
            )
            .await
            .map_err(|_| AuthError::Timeout)?;
            // This response never contains the request URL or credentials.
            use tokio::io::AsyncWriteExt;
            let reply = if code.is_ok() {
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nSilakan kembali ke XIX-Upscaler untuk melihat hasil koneksi.".as_slice()
            } else {
                b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\n\r\nLogin tidak sah. Silakan kembali ke XIX-Upscaler.".as_slice()
            };
            let _ =
                tokio::time::timeout(self.runtime.timeouts.request, socket.write_all(reply)).await;
            drop(socket);
            if code.is_ok() {
                debug::event("oauth", "callback OAuth diterima dan tervalidasi");
            } else {
                debug::event("oauth", "callback OAuth diterima tetapi ditolak");
            }
            let code = code?;
            debug::event("oauth", "menukar authorization code ke token");
            let mut token_form = vec![
                ("client_id", self.runtime.config.client_id.as_str()),
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &attempt.redirect_uri),
                ("code_verifier", &attempt.verifier),
            ];
            if let Some(secret) = self.runtime.config.client_secret.as_deref() {
                token_form.push(("client_secret", secret));
            }
            let tokens = match self
                .request_token(&token_form)
                .await
            {
                Ok(tokens) => tokens,
                Err(error) => {
                    debug::event("oauth", format!("pertukaran token gagal: {error}"));
                    return Err(error);
                }
            };
            debug::event("oauth", "pertukaran token berhasil");
            self.install_tokens(tokens, false)?;
            debug::event("oauth", "refresh token disimpan; OAuth selesai");
            Ok(self.status())
        };
        tokio::select! {
            biased;
            _ = cancel.changed() => Err(AuthError::Cancelled),
            result = tokio::time::timeout(self.runtime.timeouts.login, login) => result.map_err(|_| AuthError::Timeout)?,
        }
    }

    pub async fn disconnect(&self) -> Result<(), AuthError> {
        // Wake the login/refresh future before acquiring its lock. Dropping that
        // future closes the listener and prevents a late response restoring login.
        self.cancellation
            .send_modify(|value| *value = value.wrapping_add(1));
        let _operation = self.operation.lock().await;
        let refresh = self.state.lock().refresh_token.take();
        let vault = {
            let mut state = self.state.lock();
            state.tokens = None;
            state.vault.clone().ok_or(AuthError::Storage)?
        };
        let cleared = vault.clear();
        if let Some(refresh) = refresh {
            if let Ok(client) = token_client(self.runtime.timeouts.revoke) {
                let _ = tokio::time::timeout(
                    self.runtime.timeouts.revoke,
                    client
                        .post(self.runtime.config.revoke_endpoint.clone())
                        .form(&[("token", refresh)])
                        .send(),
                )
                .await;
            }
        }
        cleared
    }

    async fn request_token(&self, form: &[(&str, &str)]) -> Result<TokenSet, AuthError> {
        debug::event(
            "oauth",
            format!(
                "permintaan token disiapkan; client_id={}, grant_type={}, code={}, redirect_uri={}, code_verifier={}, client_secret={}",
                form.iter().any(|(name, _)| *name == "client_id"),
                form.iter().any(|(name, _)| *name == "grant_type"),
                form.iter().any(|(name, _)| *name == "code"),
                form.iter().any(|(name, _)| *name == "redirect_uri"),
                form.iter().any(|(name, _)| *name == "code_verifier"),
                form.iter().any(|(name, _)| *name == "client_secret"),
            ),
        );
        let client = token_client(self.runtime.timeouts.token)?;
        let mut response = match client
            .post(self.runtime.config.token_endpoint.clone())
            .form(form)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                debug::event(
                    "oauth",
                    format!(
                        "request token gagal; timeout={}, connect={}, request={}",
                        error.is_timeout(),
                        error.is_connect(),
                        error.is_request()
                    ),
                );
                return Err(AuthError::Network);
            }
        };
        let status = response.status();
        debug::event("oauth", format!("endpoint token merespons HTTP {status}"));
        if status.is_redirection() {
            return Err(AuthError::Network);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            debug::event(
                "oauth",
                format!(
                    "membaca respons token gagal; timeout={}, request={}",
                    error.is_timeout(),
                    error.is_request()
                ),
            );
            AuthError::Network
        })? {
            if bytes.len() + chunk.len() > 65536 {
                return Err(AuthError::InvalidCallback);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| AuthError::InvalidCallback)?;
        if !status.is_success() {
            let error_code = value["error"].as_str();
            debug::event(
                "oauth",
                format!(
                    "penukaran token ditolak Google; kode={}, detail={}",
                    token_error_label(error_code),
                    token_error_description(&value),
                ),
            );
            return Err(classify_token_error(error_code));
        }
        let token: TokenResponse =
            serde_json::from_slice(&bytes).map_err(|_| AuthError::InvalidCallback)?;
        if !valid_token(&token.access_token)
            || token
                .refresh_token
                .as_ref()
                .is_some_and(|v| !valid_token(v))
            || !token.token_type.eq_ignore_ascii_case("Bearer")
            || token.expires_in == 0
            || token.expires_in > 604800
            || token
                .scope
                .as_deref()
                .is_some_and(|s| s != DRIVE_FILE_SCOPE)
        {
            return Err(AuthError::InvalidCallback);
        }
        Ok(TokenSet {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: (self.runtime.now)() + chrono::Duration::seconds(token.expires_in as i64),
        })
    }

    fn install_tokens(&self, tokens: TokenSet, retain_refresh: bool) -> Result<(), AuthError> {
        let mut state = self.state.lock();
        let refresh = tokens
            .refresh_token
            .clone()
            .or_else(|| {
                if retain_refresh {
                    state.refresh_token.clone()
                } else {
                    None
                }
            })
            .ok_or(AuthError::ReconnectRequired)?;
        state
            .vault
            .as_ref()
            .ok_or(AuthError::Storage)?
            .save(&refresh, DRIVE_FILE_SCOPE)?;
        state.refresh_token = Some(refresh);
        state.tokens = Some(tokens);
        Ok(())
    }
}

impl AccessTokenProvider for OAuthManager {
    fn access_token(&self) -> AuthFuture<'_, String> {
        Box::pin(async {
            let mut cancel = self.cancellation.subscribe();
            let _operation = self.operation.lock().await;
            if cancel.has_changed().unwrap_or(true) {
                return Err(AuthError::Cancelled);
            }
            let refresh = {
                let state = self.state.lock();
                if let Some(token) = &state.tokens {
                    if token.expires_at > (self.runtime.now)() + chrono::Duration::seconds(60) {
                        return Ok(token.access_token.clone());
                    }
                }
                state
                    .refresh_token
                    .clone()
                    .ok_or(AuthError::ReconnectRequired)?
            };
            debug::event("oauth", "refresh access token dimulai");
            let refresh = async {
                let mut token_form = vec![
                    ("client_id", self.runtime.config.client_id.as_str()),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh),
                ];
                if let Some(secret) = self.runtime.config.client_secret.as_deref() {
                    token_form.push(("client_secret", secret));
                }
                match self.request_token(&token_form).await {
                    Ok(tokens) => {
                        let access = tokens.access_token.clone();
                        self.install_tokens(tokens, true)?;
                        debug::event("oauth", "refresh access token berhasil");
                        Ok(access)
                    }
                    Err(AuthError::ReconnectRequired) => {
                        let mut state = self.state.lock();
                        state.refresh_token = None;
                        state.tokens = None;
                        state.vault.as_ref().ok_or(AuthError::Storage)?.clear()?;
                        Err(AuthError::ReconnectRequired)
                    }
                    Err(error) => {
                        debug::event("oauth", format!("refresh access token gagal: {error}"));
                        Err(error)
                    }
                }
            };
            tokio::select! {
                biased;
                _ = cancel.changed() => Err(AuthError::Cancelled),
                result = refresh => result,
            }
        })
    }

    fn invalidate_access_token(&self) -> AuthFuture<'_, ()> {
        Box::pin(async {
            let _operation = self.operation.lock().await;
            self.state.lock().tokens = None;
            Ok(())
        })
    }
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    refresh_token: Option<String>,
    scope: Option<String>,
}

fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 * 1024 && value.bytes().all(|b| b.is_ascii_graphic())
}

fn classify_token_error(error_code: Option<&str>) -> AuthError {
    match error_code {
        Some("invalid_grant") => AuthError::ReconnectRequired,
        Some("access_denied") => AuthError::Denied,
        Some(
            "invalid_client"
            | "unauthorized_client"
            | "redirect_uri_mismatch",
        ) => AuthError::Configuration,
        Some("invalid_request") => AuthError::InvalidRequest,
        _ => AuthError::Network,
    }
}

fn token_error_label(error_code: Option<&str>) -> &'static str {
    match error_code {
        Some("invalid_grant") => "invalid_grant",
        Some("access_denied") => "access_denied",
        Some("invalid_client") => "invalid_client",
        Some("unauthorized_client") => "unauthorized_client",
        Some("invalid_request") => "invalid_request",
        Some("redirect_uri_mismatch") => "redirect_uri_mismatch",
        Some(_) => "other",
        None => "missing",
    }
}

fn token_error_description(value: &serde_json::Value) -> String {
    let Some(description) = value["error_description"].as_str() else {
        return "not_available".into();
    };
    let cleaned: String = description
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "not_available".into()
    } else {
        cleaned
    }
}

fn token_client(timeout: std::time::Duration) -> Result<reqwest::Client, AuthError> {
    crate::net::http::ensure_crypto_provider();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| AuthError::Network)
}

async fn read_callback(
    socket: &mut tokio::net::TcpStream,
    address: std::net::SocketAddr,
    state: &str,
) -> Result<String, AuthError> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    loop {
        let mut block = [0; 1024];
        let n = socket
            .read(&mut block)
            .await
            .map_err(|_| AuthError::InvalidCallback)?;
        if n == 0 {
            return Err(AuthError::InvalidCallback);
        }
        bytes.extend_from_slice(&block[..n]);
        if bytes.len() > 8192 {
            return Err(AuthError::InvalidCallback);
        }
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            if bytes.len() != end + 4 {
                return Err(AuthError::InvalidCallback);
            }
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| AuthError::InvalidCallback)?;
    let mut lines = text[..text.len() - 4].split("\r\n");
    let parts: Vec<_> = lines
        .next()
        .ok_or(AuthError::InvalidCallback)?
        .split(' ')
        .collect();
    if parts.len() != 3 || parts[0] != "GET" || parts[2] != "HTTP/1.1" {
        return Err(AuthError::InvalidCallback);
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (key, value) = line.split_once(':').ok_or(AuthError::InvalidCallback)?;
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
            || headers
                .insert(key.to_ascii_lowercase(), value.trim())
                .is_some()
        {
            return Err(AuthError::InvalidCallback);
        }
    }
    if headers.get("host").copied() != Some(address.to_string().as_str())
        || headers.contains_key("transfer-encoding")
        || headers.get("content-length").is_some_and(|v| *v != "0")
    {
        return Err(AuthError::InvalidCallback);
    }
    parse_callback(parts[1], state)
}

fn open_authorization_browser(url: &Url) -> Result<(), AuthError> {
    if url.scheme() != "https"
        || url.host_str() != Some("accounts.google.com")
        || url.path() != "/o/oauth2/v2/auth"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err(AuthError::Browser);
    }
    #[cfg(windows)]
    {
        use windows::{
            core::{HSTRING, PCWSTR},
            Win32::{
                Foundation::HWND,
                UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            },
        };
        let url = HSTRING::from(url.as_str());
        let verb = HSTRING::from("open");
        let result = unsafe {
            ShellExecuteW(
                HWND::default(),
                &verb,
                &url,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            return Err(AuthError::Browser);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(AuthError::Browser)
    }
}

// Intentionally not serializable: access credentials belong in memory only.
pub struct TokenSet {
    pub(super) access_token: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_at: chrono::DateTime<chrono::Utc>,
}

impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenSet([redacted])")
    }
}

pub struct PkceAttempt {
    pub(super) redirect_uri: String,
    pub(super) verifier: String,
    pub(super) state: String,
}

impl fmt::Debug for PkceAttempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PkceAttempt([redacted])")
    }
}

impl PkceAttempt {
    pub fn new(redirect_uri: String) -> Result<Self, AuthError> {
        let redirect = Url::parse(&redirect_uri).map_err(|_| AuthError::InvalidRedirect)?;
        if redirect_uri.bytes().any(|byte| byte.is_ascii_whitespace())
            || redirect.scheme() != "http"
            || redirect.host_str() != Some("127.0.0.1")
            || !matches!(redirect.port(), Some(1..=65535))
            || redirect.path() != "/"
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            return Err(AuthError::InvalidRedirect);
        }
        // UUID v4 uses OS cryptographic randomness. Three independent UUIDs
        // give 366 random bits and 96 unreserved characters, without a new RNG.
        let verifier = (0..3)
            .map(|_| Uuid::new_v4().simple().to_string())
            .collect();
        Ok(Self {
            redirect_uri: redirect.to_string(),
            verifier,
            state: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        })
    }

    pub fn authorization_url(&self, config: &OAuthConfig, has_refresh_token: bool) -> Url {
        let mut url = config.authorization_endpoint.clone();
        url.query_pairs_mut().clear().extend_pairs([
            ("client_id", config.client_id.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", config.scopes.join(" ").as_str()),
            ("state", self.state.as_str()),
            ("code_challenge", pkce_challenge(&self.verifier).as_str()),
            ("code_challenge_method", "S256"),
            ("access_type", "offline"),
        ]);
        if !has_refresh_token {
            url.query_pairs_mut().append_pair("prompt", "consent");
        }
        url
    }
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

#[cfg(all(test, windows))]
mod manager_tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::{
        collections::VecDeque,
        net::SocketAddr,
        sync::atomic::{AtomicI64, Ordering},
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::mpsc,
    };

    struct Reply {
        status: u16,
        body: String,
        extra: String,
        delay: Duration,
    }
    impl Reply {
        fn json(status: u16, body: serde_json::Value) -> Self {
            Self {
                status,
                body: body.to_string(),
                extra: String::new(),
                delay: Duration::ZERO,
            }
        }
        fn token(access: &str, refresh: Option<&str>) -> Self {
            let mut body = serde_json::json!({"access_token":access,"token_type":"Bearer","expires_in":3600,"scope":DRIVE_FILE_SCOPE});
            if let Some(refresh) = refresh {
                body["refresh_token"] = refresh.into();
            }
            Self::json(200, body)
        }
    }

    struct Server {
        url: Url,
        requests: Arc<parking_lot::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Server {
        async fn new(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let requests = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let mut replies = VecDeque::from(replies);
            let task = tokio::spawn(async move {
                loop {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut data = Vec::new();
                    let read = async {
                        loop {
                            let mut chunk = [0; 1024];
                            let n = socket.read(&mut chunk).await.unwrap();
                            if n == 0 {
                                break;
                            }
                            data.extend_from_slice(&chunk[..n]);
                            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                                let length = headers
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .map(|s| s.trim().parse::<usize>().unwrap())
                                    .unwrap_or(0);
                                if data.len() >= end + 4 + length {
                                    break;
                                }
                            }
                            assert!(data.len() < 128 * 1024);
                        }
                    };
                    if tokio::time::timeout(Duration::from_secs(3), read)
                        .await
                        .is_err()
                    {
                        continue;
                    }
                    recorded.lock().push(String::from_utf8(data).unwrap());
                    let reply = replies.pop_front().unwrap_or_else(|| {
                        Reply::json(500, serde_json::json!({"error":"unexpected_request"}))
                    });
                    tokio::time::sleep(reply.delay).await;
                    let response = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}", reply.status, reply.body.len(), reply.extra, reply.body);
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
            Self {
                url,
                requests,
                task,
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct Fixture {
        manager: Arc<OAuthManager>,
        directory: crate::colab::test_support::TestDir,
        server: Server,
        browser: mpsc::UnboundedReceiver<Url>,
        seconds: Arc<AtomicI64>,
    }
    impl Fixture {
        async fn new(saved: Option<&str>, replies: Vec<Reply>) -> Self {
            let directory = crate::colab::test_support::TestDir::new();
            if let Some(refresh) = saved {
                TokenVault::new(&directory.0)
                    .save(refresh, DRIVE_FILE_SCOPE)
                    .unwrap();
            }
            let server = Server::new(replies).await;
            let (send, browser) = mpsc::unbounded_channel();
            let seconds = Arc::new(AtomicI64::new(1_788_300_000));
            let clock = seconds.clone();
            let mut manager = OAuthManager::new();
            manager.runtime.config.authorization_endpoint = server.url.join("auth").unwrap();
            manager.runtime.config.token_endpoint = server.url.join("token").unwrap();
            manager.runtime.config.revoke_endpoint = server.url.join("revoke").unwrap();
            manager.runtime.open_browser = Arc::new(move |url| {
                send.send(url.clone()).unwrap();
                Ok(())
            });
            manager.runtime.now =
                Arc::new(move || Utc.timestamp_opt(clock.load(Ordering::SeqCst), 0).unwrap());
            manager.runtime.timeouts = AuthTimeouts {
                login: Duration::from_secs(2),
                request: Duration::from_millis(150),
                token: Duration::from_millis(500),
                revoke: Duration::from_millis(100),
            };
            manager.initialize(directory.0.clone()).unwrap();
            Self {
                manager: Arc::new(manager),
                directory,
                server,
                browser,
                seconds,
            }
        }
        async fn begin(&mut self) -> (tokio::task::JoinHandle<Result<AuthStatus, AuthError>>, Url) {
            let manager = self.manager.clone();
            let task = tokio::spawn(async move { manager.connect().await });
            let url = tokio::time::timeout(Duration::from_secs(1), self.browser.recv())
                .await
                .unwrap()
                .unwrap();
            (task, url)
        }
        fn saved(&self) -> Option<String> {
            TokenVault::new(&self.directory.0)
                .load()
                .unwrap()
                .map(|l| l.refresh_token)
        }
    }
    fn callback_parts(url: &Url) -> (SocketAddr, String) {
        let pairs: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let redirect = Url::parse(&pairs["redirect_uri"]).unwrap();
        (
            format!("127.0.0.1:{}", redirect.port().unwrap())
                .parse()
                .unwrap(),
            pairs["state"].clone(),
        )
    }
    async fn callback(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), stream.read_to_string(&mut response))
            .await
            .unwrap();
        response
    }
    fn valid_request(address: SocketAddr, state: &str) -> String {
        format!("GET /?code=private-code&state={state} HTTP/1.1\r\nHost: {address}\r\n\r\n")
    }

    #[tokio::test]
    async fn refresh_is_single_flight_and_keeps_omitted_refresh_token() {
        let f = Fixture::new(
            Some("account-a-refresh"),
            vec![Reply::token("access-1", None)],
        )
        .await;
        let results =
            futures_util::future::join_all((0..16).map(|_| f.manager.access_token())).await;
        assert!(results.iter().all(|r| r.as_deref() == Ok("access-1")));
        assert_eq!(f.server.requests.lock().len(), 1);
        assert_eq!(f.saved().as_deref(), Some("account-a-refresh"));
        assert_eq!(f.server.requests.lock()[0].split("\r\n\r\n").nth(1).unwrap(), "client_id=167537907895-rp94d606ic46esvhjcp184roqikh0v6k.apps.googleusercontent.com&grant_type=refresh_token&refresh_token=account-a-refresh");
    }

    #[tokio::test]
    async fn refresh_cache_expires_at_sixty_second_boundary_and_invalidation_gets_new_token() {
        let f = Fixture::new(
            Some("old-refresh"),
            vec![
                Reply::token("a1", Some("new-refresh")),
                Reply::token("a2", None),
                Reply::token("a3", None),
            ],
        )
        .await;
        assert_eq!(f.manager.access_token().await.unwrap(), "a1");
        f.seconds.fetch_add(3539, Ordering::SeqCst);
        assert_eq!(f.manager.access_token().await.unwrap(), "a1");
        f.seconds.fetch_add(1, Ordering::SeqCst);
        assert_eq!(f.manager.access_token().await.unwrap(), "a2");
        f.manager.invalidate_access_token().await.unwrap();
        assert_eq!(f.manager.access_token().await.unwrap(), "a3");
        assert_eq!(f.saved().as_deref(), Some("new-refresh"));
        assert_eq!(f.server.requests.lock().len(), 3);
        assert!(f.server.requests.lock()[1].contains("refresh_token=new-refresh"));
    }

    #[tokio::test]
    async fn refresh_invalid_grant_clears_disk_memory_and_status() {
        let f = Fixture::new(
            Some("revoked"),
            vec![Reply::json(
                400,
                serde_json::json!({"error":"invalid_grant","error_description":"PRIVATE"}),
            )],
        )
        .await;
        let error = f.manager.access_token().await.unwrap_err();
        assert!(error.requires_reconnect());
        assert!(!format!("{error:?} {error}").contains("PRIVATE"));
        assert!(f.saved().is_none());
        assert!(!f.manager.status().connected);
        assert!(f
            .manager
            .access_token()
            .await
            .unwrap_err()
            .requires_reconnect());
        assert_eq!(f.server.requests.lock().len(), 1);
    }

    #[tokio::test]
    async fn token_redirects_never_forward_credentials() {
        let sink = Server::new(vec![]).await;
        for status in [302, 307, 308] {
            let mut reply = Reply::json(status, serde_json::json!({}));
            reply.extra = format!("Location: {}PRIVATE\r\n", sink.url);
            let f = Fixture::new(Some("PRIVATE-refresh"), vec![reply]).await;
            let error = f.manager.access_token().await.unwrap_err();
            assert!(!format!("{error:?} {error}").contains("PRIVATE"));
            assert_eq!(f.saved().as_deref(), Some("PRIVATE-refresh"));
        }
        assert!(sink.requests.lock().is_empty());
    }

    #[tokio::test]
    async fn token_bad_payloads_and_broader_scopes_are_rejected_without_losing_refresh() {
        for body in [
            serde_json::json!({"access_token":"PRIVATE","token_type":"Bearer","expires_in":0}),
            serde_json::json!({"access_token":"PRIVATE","token_type":"Bearer","expires_in":-1}),
            serde_json::json!({"access_token":"PRIVATE","token_type":"Bearer","expires_in":u64::MAX}),
            serde_json::json!({"access_token":"PRIVATE","token_type":"Other","expires_in":3600}),
            serde_json::json!({"access_token":"PRIVATE\r\n","token_type":"Bearer","expires_in":3600}),
            serde_json::json!({"access_token":"PRIVATE","token_type":"Bearer","expires_in":3600,"scope":"email"}),
            serde_json::json!({"access_token":"PRIVATE","token_type":"Bearer","expires_in":3600,"refresh_token":""}),
        ] {
            let f = Fixture::new(Some("keep-refresh"), vec![Reply::json(200, body)]).await;
            let error = f.manager.access_token().await.unwrap_err();
            assert!(!format!("{error:?} {error}").contains("PRIVATE"));
            assert_eq!(f.saved().as_deref(), Some("keep-refresh"));
        }
    }

    #[tokio::test]
    async fn token_response_size_and_time_are_bounded() {
        let mut slow = Reply::token("late", None);
        slow.delay = Duration::from_secs(5);
        for reply in [
            Reply::json(200, serde_json::json!({"padding":"x".repeat(65537)})),
            slow,
        ] {
            let f = Fixture::new(Some("keep"), vec![reply]).await;
            assert!(
                tokio::time::timeout(Duration::from_secs(1), f.manager.access_token())
                    .await
                    .unwrap()
                    .is_err()
            );
            assert_eq!(f.saved().as_deref(), Some("keep"));
        }
    }

    #[tokio::test]
    async fn connect_exchanges_pkce_once_and_persists_only_refresh() {
        let mut f = Fixture::new(
            None,
            vec![Reply::token("PRIVATE-access", Some("PRIVATE-refresh"))],
        )
        .await;
        let (task, url) = f.begin().await;
        let (address, state) = callback_parts(&url);
        let response = callback(address, &valid_request(address, &state)).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(!response.contains("private-code"));
        assert!(task.await.unwrap().unwrap().connected);
        assert!(TcpStream::connect(address).await.is_err());
        assert_eq!(f.manager.access_token().await.unwrap(), "PRIVATE-access");
        let requests = f.server.requests.lock();
        assert_eq!(requests.len(), 1);
        let body = requests[0].split_once("\r\n\r\n").unwrap().1;
        let parsed = Url::parse(&format!("http://fixture/?{body}")).unwrap();
        let form: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        let auth: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "private-code");
        assert_eq!(form["redirect_uri"], auth["redirect_uri"]);
        assert_eq!(
            pkce_challenge(&form["code_verifier"]),
            auth["code_challenge"]
        );
        assert!(!form.contains_key("client_secret"));
        assert_eq!(form.len(), 5);
        drop(requests);
        assert_eq!(f.saved().as_deref(), Some("PRIVATE-refresh"));
        let bytes = std::fs::read(f.directory.0.join("colab/google-login.dpapi")).unwrap();
        let plain = crate::secure::dpapi::unprotect(&bytes).unwrap();
        assert!(!String::from_utf8(plain).unwrap().contains("PRIVATE-access"));
    }

    #[tokio::test]
    async fn account_switch_without_new_refresh_never_reuses_prior_account() {
        let mut f = Fixture::new(
            Some("account-a"),
            vec![
                Reply::json(200, serde_json::json!({})),
                Reply::token("account-b-access", None),
            ],
        )
        .await;
        f.manager.disconnect().await.unwrap();
        let (task, url) = f.begin().await;
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["prompt"], "consent");
        let (address, state) = callback_parts(&url);
        callback(address, &valid_request(address, &state)).await;
        assert!(task.await.unwrap().unwrap_err().requires_reconnect());
        assert!(f.saved().is_none());
        assert!(!f.manager.status().connected);
        assert!(f
            .manager
            .access_token()
            .await
            .unwrap_err()
            .requires_reconnect());
    }

    #[tokio::test]
    async fn disconnect_cancels_pending_login_without_late_credential_resurrection() {
        let mut f = Fixture::new(None, vec![]).await;
        let (task, url) = f.begin().await;
        let (address, _) = callback_parts(&url);
        tokio::time::timeout(Duration::from_millis(500), f.manager.disconnect())
            .await
            .unwrap()
            .unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(TcpStream::connect(address).await.is_err());
        assert!(f.saved().is_none());
        assert!(!f.manager.status().connected);
        assert!(f.server.requests.lock().is_empty());
    }

    #[tokio::test]
    async fn dropping_connect_releases_listener_and_allows_a_new_attempt() {
        let mut f = Fixture::new(None, vec![]).await;
        let (task, url) = f.begin().await;
        let (address, _) = callback_parts(&url);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(TcpStream::connect(address).await.is_err());
        let (next, _) = f.begin().await;
        f.manager.disconnect().await.unwrap();
        assert!(next.await.unwrap().is_err());
        assert!(!f.manager.status().connected);
        assert!(f.saved().is_none());
    }

    #[tokio::test]
    async fn disconnect_clears_before_bounded_best_effort_revoke() {
        let mut reply = Reply::json(500, serde_json::json!({"error":"PRIVATE"}));
        reply.delay = Duration::from_secs(5);
        let f = Fixture::new(Some("old-refresh"), vec![reply]).await;
        tokio::time::timeout(Duration::from_millis(500), f.manager.disconnect())
            .await
            .unwrap()
            .unwrap();
        assert!(f.saved().is_none());
        assert!(!f.manager.status().connected);
        assert_eq!(f.server.requests.lock().len(), 1);
        assert!(f.server.requests.lock()[0].starts_with("POST /revoke HTTP/1.1"));
        assert!(f.server.requests.lock()[0].ends_with("token=old-refresh"));
        f.manager.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn loopback_rejects_wrong_host_duplicates_state_and_http_ambiguity() {
        for kind in [
            "host",
            "duplicate-host",
            "state",
            "post",
            "absolute",
            "body",
            "folded",
            "transfer",
        ] {
            let mut f = Fixture::new(None, vec![]).await;
            let (task, url) = f.begin().await;
            let (address, state) = callback_parts(&url);
            let request = match kind {
                "host" => valid_request(address, &state)
                    .replace(&format!("Host: {address}"), "Host: attacker.invalid"),
                "duplicate-host" => valid_request(address, &state)
                    .replace("\r\n\r\n", &format!("\r\nhOsT: {address}\r\n\r\n")),
                "state" => valid_request(address, "wrong"),
                "post" => valid_request(address, &state).replacen("GET", "POST", 1),
                "absolute" => valid_request(address, &state).replacen(
                    "GET /",
                    &format!("GET http://{address}/"),
                    1,
                ),
                "body" => valid_request(address, &state)
                    .replace("\r\n\r\n", "\r\nContent-Length: 1\r\n\r\nx"),
                "folded" => {
                    valid_request(address, &state).replace("\r\n\r\n", "\r\n X-Header: bad\r\n\r\n")
                }
                _ => valid_request(address, &state)
                    .replace("\r\n\r\n", "\r\nTransfer-Encoding: chunked\r\n\r\n"),
            };
            let response = callback(address, &request).await;
            assert!(!response.contains("private-code"));
            assert!(task.await.unwrap().is_err(), "accepted {kind}");
            assert!(f.server.requests.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn loopback_header_limit_counts_all_bytes_and_accepts_fragmentation() {
        for total in [8192, 8193] {
            let mut f = Fixture::new(None, vec![Reply::token("access", Some("refresh"))]).await;
            let (task, url) = f.begin().await;
            let (address, state) = callback_parts(&url);
            let base =
                valid_request(address, &state).replace("\r\n\r\n", "\r\nX-Padding: \r\n\r\n");
            let request = base.replace(
                "X-Padding: ",
                &format!("X-Padding: {}", "x".repeat(total - base.len())),
            );
            assert_eq!(request.len(), total);
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket.write_all(&request.as_bytes()[..15]).await.unwrap();
            tokio::task::yield_now().await;
            let _ = socket.write_all(&request.as_bytes()[15..]).await;
            let mut response = Vec::new();
            let _ = socket.read_to_end(&mut response).await;
            assert_eq!(task.await.unwrap().is_ok(), total == 8192);
        }
    }

    #[tokio::test]
    async fn loopback_slow_partial_request_times_out_and_closes_listener() {
        let mut f = Fixture::new(None, vec![]).await;
        let (task, url) = f.begin().await;
        let (address, _) = callback_parts(&url);
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(b"GET /").await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(TcpStream::connect(address).await.is_err());
        assert!(f.server.requests.lock().is_empty());
    }
}

fn decode_callback_component(encoded: &str) -> Result<String, AuthError> {
    let mut decoded = Vec::with_capacity(encoded.len());
    let mut bytes = encoded.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(match byte {
            b'+' => b' ',
            b'%' => {
                let upper = bytes.next().and_then(|v| (v as char).to_digit(16));
                let lower = bytes.next().and_then(|v| (v as char).to_digit(16));
                match (upper, lower) {
                    (Some(upper), Some(lower)) => ((upper << 4) | lower) as u8,
                    _ => return Err(AuthError::InvalidCallback),
                }
            }
            byte => byte,
        });
    }
    let text = String::from_utf8(decoded).map_err(|_| AuthError::InvalidCallback)?;
    if text.chars().any(char::is_control) {
        return Err(AuthError::InvalidCallback);
    }
    Ok(text)
}

pub fn parse_callback(request_target: &str, expected_state: &str) -> Result<String, AuthError> {
    if request_target.len() > 8192
        || expected_state.is_empty()
        || request_target.contains(['#', '\\'])
        || request_target
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(AuthError::InvalidCallback);
    }
    let (path, query) = request_target
        .split_once('?')
        .ok_or(AuthError::InvalidCallback)?;
    if path != "/" || query.is_empty() {
        return Err(AuthError::InvalidCallback);
    }
    let mut params = HashMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').ok_or(AuthError::InvalidCallback)?;
        let key = decode_callback_component(key)?;
        let value = decode_callback_component(value)?;
        if key.is_empty() || params.insert(key, value).is_some() {
            return Err(AuthError::InvalidCallback);
        }
    }
    let state = params.get("state").ok_or(AuthError::InvalidCallback)?;
    if state.len() != expected_state.len()
        || state
            .bytes()
            .zip(expected_state.bytes())
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            != 0
    {
        return Err(AuthError::StateMismatch);
    }
    if params.contains_key("error") {
        return Err(AuthError::Denied);
    }
    let code = params.remove("code").ok_or(AuthError::InvalidCallback)?;
    if code.trim().is_empty() {
        return Err(AuthError::InvalidCallback);
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn manager_restores_encrypted_login_and_is_a_shared_token_provider() {
        let directory = super::super::test_support::TestDir::new();
        let vault = super::super::vault::TokenVault::new(&directory.0);
        vault.save("saved-refresh", DRIVE_FILE_SCOPE).unwrap();
        let manager = std::sync::Arc::new(OAuthManager::new());
        manager.initialize(directory.0.clone()).unwrap();
        let status = serde_json::to_value(manager.status()).unwrap();
        assert_eq!(
            status,
            serde_json::json!({"connected":true,"masked_account":null})
        );
        let provider: std::sync::Arc<dyn AccessTokenProvider> = manager;
        provider.invalidate_access_token().await.unwrap();
        assert!(vault.load().unwrap().is_some());
    }

    #[tokio::test]
    async fn manager_without_login_requires_connect_without_opening_browser() {
        let directory = super::super::test_support::TestDir::new();
        let manager = OAuthManager::new();
        manager.initialize(directory.0.clone()).unwrap();
        assert!(!manager.status().connected);
        assert!(manager
            .access_token()
            .await
            .unwrap_err()
            .requires_reconnect());
    }

    #[test]
    fn in_memory_tokens_are_redacted_from_debug_output() {
        let tokens = TokenSet {
            access_token: "private-access-value".into(),
            refresh_token: Some("private-refresh-value".into()),
            expires_at: chrono::Utc::now(),
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("private-access-value"));
        assert!(!debug.contains("private-refresh-value"));
    }

    #[test]
    fn dotenv_loader_reads_oauth_values_without_debugging_the_secret() {
        let directory = super::super::test_support::TestDir::new();
        let path = directory.0.join(".env");
        std::fs::write(
            &path,
            "XIX_GOOGLE_CLIENT_ID=client-id\nXIX_GOOGLE_CLIENT_SECRET=secret-value\n",
        )
        .unwrap();
        let values = read_dotenv(&path).unwrap();
        assert_eq!(values.get("XIX_GOOGLE_CLIENT_ID").map(String::as_str), Some("client-id"));
        assert_eq!(
            values
                .get("XIX_GOOGLE_CLIENT_SECRET")
                .map(String::as_str),
            Some("secret-value")
        );
        assert!(valid_client_secret("secret-value"));
        let debug = format!("{:?}", OAuthConfig::production());
        assert!(!debug.contains("secret-value"));
    }

    #[test]
    fn oauth_token_error_codes_are_classified_without_exposing_details() {
        assert_eq!(
            classify_token_error(Some("invalid_client")),
            AuthError::Configuration
        );
        assert_eq!(
            classify_token_error(Some("redirect_uri_mismatch")),
            AuthError::Configuration
        );
        assert_eq!(
            classify_token_error(Some("invalid_request")),
            AuthError::InvalidRequest
        );
        assert_eq!(
            classify_token_error(Some("invalid_grant")),
            AuthError::ReconnectRequired
        );
        assert_eq!(token_error_label(Some("unexpected")), "other");
        assert_eq!(token_error_label(None), "missing");
    }

    #[test]
    fn authorization_requests_only_drive_file_without_a_client_secret() {
        let config = OAuthConfig::production();
        let attempt = PkceAttempt::new("http://127.0.0.1:49152/".into()).unwrap();
        let url = attempt.authorization_url(&config, false);
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            url.origin().ascii_serialization(),
            "https://accounts.google.com"
        );
        assert_eq!(
            params["client_id"],
            "167537907895-rp94d606ic46esvhjcp184roqikh0v6k.apps.googleusercontent.com"
        );
        assert_eq!(
            params["scope"],
            "https://www.googleapis.com/auth/drive.file"
        );
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:49152/");
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["access_type"], "offline");
        assert_eq!(params["prompt"], "consent");
        assert_eq!(params.len(), 9);
        assert!(!params.contains_key("client_secret"));
        let reconnect = attempt.authorization_url(&config, true);
        assert!(!reconnect.query_pairs().any(|(key, _)| key == "prompt"));
    }

    #[test]
    fn pkce_matches_the_rfc7636_s256_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn attempts_have_independent_unreserved_verifiers_and_state() {
        let a = PkceAttempt::new("http://127.0.0.1:49152/".into()).unwrap();
        let b = PkceAttempt::new("http://127.0.0.1:49152/".into()).unwrap();
        assert_eq!(a.verifier.len(), 96);
        assert!(a
            .verifier
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c)));
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.state, b.state);
        assert!(a.state.len() >= 32);
        let debug = format!("{a:?}");
        assert!(!debug.contains(&a.verifier));
        assert!(!debug.contains(&a.state));
    }

    #[test]
    fn redirect_rejects_non_loopback_or_extra_components() {
        for uri in [
            "https://example.com/",
            "http://localhost:49152/",
            "http://127.0.0.1/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:49152/other",
            "http://127.0.0.1:49152/?q=1",
            "http://user@127.0.0.1:49152/",
            "http://127.0.0.1:49152/#fragment",
        ] {
            assert!(PkceAttempt::new(uri.into()).is_err(), "accepted {uri}");
        }
    }

    #[test]
    fn callback_requires_exact_state_and_one_code() {
        assert_eq!(parse_callback("/?code=abc&state=s1", "s1").unwrap(), "abc");
        assert_eq!(
            parse_callback("/?code=a%2Bb%2Fc&state=s%2B1", "s+1").unwrap(),
            "a+b/c"
        );
        // Google may include informational response parameters.
        assert_eq!(
            parse_callback("/?code=abc&state=s1&scope=drive.file&authuser=0", "s1").unwrap(),
            "abc"
        );
        for target in [
            "/?code=abc&state=wrong",
            "/?code=a&code=b&state=s1",
            "/?code=a&state=s1&state=s1",
            "/?code=a",
            "/?state=s1",
            "/?code=&state=s1",
            "/?code=abc&state=",
            "/?code=abc&st%61te=s1&state=s1",
            "/?error=access_denied&state=s1",
            "/?code=a&error=access_denied&state=s1",
        ] {
            assert!(parse_callback(target, "s1").is_err(), "accepted {target}");
        }
    }

    #[test]
    fn malformed_callback_targets_are_rejected_without_echoing_secrets() {
        for target in [
            "http://127.0.0.1:49152/?code=PRIVATE&state=s1",
            "//example.com/?code=PRIVATE&state=s1",
            "/other?code=PRIVATE&state=s1",
            "/?code=PRIVATE&state=s1#fragment",
            "/?code=%GGPRIVATE&state=s1",
            "/?code=%FFPRIVATE&state=s1",
            "/?code=PRIVATE%00&state=s1",
            "/?code=PRIVATE\r\n&state=s1",
            "/?code=PRIVATE&state=s1&scope=a&scope=b",
        ] {
            let error = parse_callback(target, "s1").unwrap_err();
            assert!(!error.to_string().contains("PRIVATE"));
            assert!(!format!("{error:?}").contains("PRIVATE"));
        }
        assert!(parse_callback("/?code=PRIVATE&state=", "").is_err());
        assert!(parse_callback(&format!("/?code={}&state=s1", "a".repeat(8192)), "s1").is_err());
    }
}
