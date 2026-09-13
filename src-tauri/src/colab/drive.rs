//! Drive transfers. Registry owners persist reservations before creating job files.

use super::{auth::AccessTokenProvider, debug, manifest::PreparedJob};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fmt, path::Path, sync::Arc, time::Duration};
use uuid::Uuid;

pub type TransferProgress = Arc<dyn Fn(u64, u64) + Send + Sync>;
pub type SessionSink = Arc<dyn Fn(Option<String>) -> Result<(), DriveError> + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIds {
    pub root_id: String,
    pub marker_id: String,
    pub jobs_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFiles {
    pub folder_id: String,
    pub input_id: String,
    pub manifest_id: String,
    pub status_a_id: String,
    pub status_b_id: String,
    pub control_id: String,
    pub output_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadResult {
    pub file_id: String,
    pub uploaded_bytes: u64,
}

// No serialization: neither the account nor its address belongs in the registry.
#[derive(Clone)]
pub struct DriveUser {
    pub display_name: Option<String>,
    pub email_address: Option<String>,
    pub permission_id: Option<String>,
}

impl fmt::Debug for DriveUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DriveUser([redacted])")
    }
}

impl DriveUser {
    pub fn masked_email(&self) -> Option<String> {
        let email = self.email_address.as_ref()?;
        if email.len() > 320 || email.chars().any(char::is_control) {
            return None;
        }
        let (local, domain) = email.split_once('@')?;
        if local.is_empty() || domain.is_empty() || domain.contains('@') {
            return None;
        }
        let visible: String = local
            .chars()
            .take(if local.chars().count() > 2 { 2 } else { 1 })
            .collect();
        Some(format!("{visible}***@{domain}"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveError {
    Authentication,
    Permission,
    Quota,
    ApiDisabled,
    NotFound,
    Conflict,
    Network,
    RetryExhausted,
    InvalidResponse,
    UnsafeUrl,
    InvalidId,
    InvalidJob,
    WorkspaceMismatch,
    SizeMismatch,
    LocalIo,
    Storage,
}

impl fmt::Display for DriveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Authentication => "Hubungkan ulang Google Drive",
            Self::Permission => "Izin Google Drive tidak mencukupi",
            Self::Quota => "Ruang Google Drive tidak mencukupi",
            Self::ApiDisabled => "Aktifkan Google Drive API untuk aplikasi ini",
            Self::NotFound => "Berkas Google Drive tidak tersedia untuk akun ini",
            Self::Conflict => "Berkas Google Drive tidak cocok dengan pekerjaan tersimpan",
            Self::Network => "Koneksi Google Drive terputus; coba lanjutkan kembali",
            Self::RetryExhausted => {
                "Google Drive tidak merespons; periksa internet, proxy, atau firewall lalu coba lagi"
            }
            Self::InvalidResponse => "Balasan Google Drive tidak sah",
            Self::UnsafeUrl => "Alamat transfer Google Drive tidak sah",
            Self::InvalidId => "Identitas berkas Google Drive tidak sah",
            Self::InvalidJob => "Data pekerjaan Google Drive tidak sah",
            Self::WorkspaceMismatch => {
                "Folder pekerjaan tidak cocok; hubungkan akun Google Drive sebelumnya"
            }
            Self::SizeMismatch => "Ukuran berkas Google Drive tidak cocok",
            Self::LocalIo => "Berkas transfer lokal tidak dapat dibaca atau ditulis",
            Self::Storage => "Catatan pemulihan transfer tidak dapat disimpan",
        })
    }
}

impl std::error::Error for DriveError {}

pub struct DriveEndpoints {
    api: Url,
    upload: Url,
}

impl DriveEndpoints {
    pub fn production() -> Self {
        Self {
            api: Url::parse("https://www.googleapis.com/drive/v3/").unwrap(),
            upload: Url::parse("https://www.googleapis.com/upload/drive/v3/").unwrap(),
        }
    }

    pub fn loopback(origin: &str) -> Result<Self, DriveError> {
        let base = Url::parse(origin).map_err(|_| DriveError::UnsafeUrl)?;
        if base.scheme() != "http"
            || !base.host_str().is_some_and(|host| {
                host.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            })
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            return Err(DriveError::UnsafeUrl);
        }
        Ok(Self {
            api: base.join("drive/v3/").map_err(|_| DriveError::UnsafeUrl)?,
            upload: base
                .join("upload/drive/v3/")
                .map_err(|_| DriveError::UnsafeUrl)?,
        })
    }
}

pub struct DriveClient {
    http: Client,
    tokens: Arc<dyn AccessTokenProvider>,
    endpoints: DriveEndpoints,
    install_id: Uuid,
    workspace: tokio::sync::Mutex<Option<WorkspaceIds>>,
}

impl DriveClient {
    pub fn new(tokens: Arc<dyn AccessTokenProvider>, install_id: Uuid) -> Result<Self, DriveError> {
        Self::with_endpoints(tokens, install_id, DriveEndpoints::production())
    }

    pub fn with_endpoints(
        tokens: Arc<dyn AccessTokenProvider>,
        install_id: Uuid,
        endpoints: DriveEndpoints,
    ) -> Result<Self, DriveError> {
        crate::net::http::ensure_crypto_provider();
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|_| DriveError::Network)?;
        debug::event("drive", "klien Google Drive siap");
        Ok(Self {
            http,
            tokens,
            endpoints,
            install_id,
            workspace: tokio::sync::Mutex::new(None),
        })
    }

    pub async fn ensure_workspace(&self) -> Result<WorkspaceIds, DriveError> {
        debug::event("drive", "memeriksa workspace XIX-Upscaler");
        let mut lock = self.workspace.lock().await;
        // Rediscover on each new batch; a cached account may have been switched.
        let root = self
            .find_or_create("workspace", "XIX-Upscaler", FOLDER_MIME, "root", None)
            .await?;
        let marker_bytes =
            serde_json::to_vec(&json!({"schema_version":1,"desktop_install_id":self.install_id}))
                .map_err(|_| DriveError::InvalidJob)?;
        let marker = self
            .find_or_create(
                "marker",
                "desktop-marker.json",
                "application/json",
                &root,
                Some(&marker_bytes),
            )
            .await?;
        // Check identity before creating anything else within an existing root.
        self.validate_marker(&marker).await?;
        let jobs = self
            .find_or_create("jobs", "jobs", FOLDER_MIME, &root, None)
            .await?;
        let result = WorkspaceIds {
            root_id: root,
            marker_id: marker,
            jobs_id: jobs,
        };
        self.validate_workspace(&result).await?;
        *lock = Some(result.clone());
        debug::event("drive", "workspace Google Drive tervalidasi");
        Ok(result)
    }

    pub async fn about_user(&self) -> Result<DriveUser, DriveError> {
        debug::event("drive", "memeriksa identitas akun Google Drive");
        let response = match self
            .send(
                self.http
                    .get(self.api("about")?)
                    .query(&[("fields", "user(displayName,emailAddress,permissionId)")]),
                true,
                &[],
            )
            .await
        {
            Ok(response) => response,
            Err(error) => {
                debug::event("drive", format!("pemeriksaan identitas gagal: {error}"));
                return Err(error);
            }
        };
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct User {
            display_name: Option<String>,
            email_address: Option<String>,
            permission_id: Option<String>,
        }
        #[derive(Deserialize)]
        struct About {
            user: User,
        }
        let about: About = serde_json::from_slice(&bounded_body(response).await?)
            .map_err(|_| DriveError::InvalidResponse)?;
        debug::event("drive", "identitas akun Google Drive tervalidasi");
        Ok(DriveUser {
            display_name: about.user.display_name,
            email_address: about.user.email_address,
            permission_id: about.user.permission_id,
        })
    }

    pub async fn reserve_job_files(&self) -> Result<RemoteFiles, DriveError> {
        let ids = self.generate_ids(7).await?;
        Ok(RemoteFiles {
            folder_id: ids[0].clone(),
            input_id: ids[1].clone(),
            manifest_id: ids[2].clone(),
            status_a_id: ids[3].clone(),
            status_b_id: ids[4].clone(),
            control_id: ids[5].clone(),
            output_id: ids[6].clone(),
        })
    }

    pub async fn create_job_files(
        &self,
        workspace: &WorkspaceIds,
        job: &PreparedJob,
        files: &RemoteFiles,
    ) -> Result<(), DriveError> {
        validate_distinct_ids(workspace.all().into_iter().chain(files.all()))?;
        let (job_id, name, _) = manifest_input(&job.manifest_json)?;
        if job_id != job.job_id {
            return Err(DriveError::InvalidJob);
        }
        self.validate_workspace(workspace).await?;
        let folder = self.job_metadata(
            &files.folder_id,
            &job_id.to_string(),
            FOLDER_MIME,
            &workspace.jobs_id,
            "job",
            job_id,
        );
        self.create_once(&folder, None).await?;
        let slot = serde_json::to_vec(&json!({"schema_version":1,"revision":0,"state":"queued","session_id":null,"heartbeat_at":null,"progress":{"segment_index":0,"segment_count":0,"percent":0.0,"last_checkpoint":null},"message":"","metadata":{}})).map_err(|_| DriveError::InvalidJob)?;
        let control = br#"{"schema_version":1,"revision":0,"action":"run"}"#;
        for (id, name, mime, kind, data) in [
            (
                files.input_id.as_str(),
                name.as_str(),
                "application/octet-stream",
                "input",
                b"".as_slice(),
            ),
            (
                &files.status_a_id,
                "desktop-status-a.json",
                "application/json",
                "status-a",
                slot.as_slice(),
            ),
            (
                &files.status_b_id,
                "desktop-status-b.json",
                "application/json",
                "status-b",
                slot.as_slice(),
            ),
            (
                &files.control_id,
                "control.json",
                "application/json",
                "control",
                control.as_slice(),
            ),
            (
                &files.output_id,
                "output.mp4",
                "video/mp4",
                "output",
                b"".as_slice(),
            ),
        ] {
            debug::event("drive", format!("membuat file pekerjaan; jenis={kind}"));
            self.create_once(
                &self.job_metadata(id, name, mime, &files.folder_id, kind, job_id),
                Some(data),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn publish_manifest(
        &self,
        files: &RemoteFiles,
        manifest: &[u8],
    ) -> Result<(), DriveError> {
        validate_distinct_ids(files.all())?;
        if manifest.len() > MAX_JSON {
            return Err(DriveError::InvalidJob);
        }
        let value: Value = serde_json::from_slice(manifest).map_err(|_| DriveError::InvalidJob)?;
        let (job_id, name, size) = manifest_input(&value)?;
        let input = self.metadata(&files.input_id).await?;
        check_metadata(
            &input,
            &self.job_metadata(
                &files.input_id,
                &name,
                "application/octet-stream",
                &files.folder_id,
                "input",
                job_id,
            ),
        )?;
        if file_size(&input)? != size {
            return Err(DriveError::SizeMismatch);
        }
        for (id, name, mime, kind) in [
            (
                &files.status_a_id,
                "desktop-status-a.json",
                "application/json",
                "status-a",
            ),
            (
                &files.status_b_id,
                "desktop-status-b.json",
                "application/json",
                "status-b",
            ),
            (
                &files.control_id,
                "control.json",
                "application/json",
                "control",
            ),
            (&files.output_id, "output.mp4", "video/mp4", "output"),
        ] {
            check_metadata(
                &self.metadata(id).await?,
                &self.job_metadata(id, name, mime, &files.folder_id, kind, job_id),
            )?;
        }
        let metadata = self.job_metadata(
            &files.manifest_id,
            "manifest.json",
            "application/json",
            &files.folder_id,
            "manifest",
            job_id,
        );
        self.create_once(&metadata, Some(manifest)).await?;
        if self.read_json(&files.manifest_id).await? != manifest {
            return Err(DriveError::Conflict);
        }
        Ok(())
    }
    pub async fn upload_resumable(
        &self,
        file_id: &str,
        path: &Path,
        saved_session: Option<&str>,
        session_sink: SessionSink,
        progress: TransferProgress,
    ) -> Result<UploadResult, DriveError> {
        use std::io::{Read, Seek, SeekFrom};
        validate_id(file_id)?;
        let mut session = saved_session
            .map(|url| self.session_url(url, file_id))
            .transpose()?;
        let mut file = std::fs::File::open(path).map_err(|_| DriveError::LocalIo)?;
        let before = file.metadata().map_err(|_| DriveError::LocalIo)?;
        let total = before.len();
        if !before.is_file() || !(1..=104857600).contains(&total) {
            return Err(DriveError::SizeMismatch);
        }
        let mut offset = 0;
        let mut query_offset = session.is_some();
        let mut failures = 0;
        let mut restarts = 0;
        loop {
            if session.is_none() {
                let request = self
                    .http
                    .patch(self.upload(&format!("files/{file_id}"))?)
                    .query(&[("uploadType", "resumable"), ("fields", "id,size")])
                    .header("X-Upload-Content-Type", "application/octet-stream")
                    .header("X-Upload-Content-Length", total.to_string())
                    .json(&json!({}));
                let response = self.send(request, true, &[]).await?;
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or(DriveError::UnsafeUrl)?;
                let url = self.session_url(location, file_id)?;
                session_sink(Some(url.to_string()))?;
                session = Some(url);
                offset = 0;
                query_offset = false;
            }
            let url = session.as_ref().ok_or(DriveError::InvalidResponse)?.clone();
            let (body, range, end) = if query_offset {
                (Vec::new(), format!("bytes */{total}"), total)
            } else {
                let length = (total - offset).min(8 * 1024 * 1024) as usize;
                if length == 0 {
                    return Err(DriveError::InvalidResponse);
                }
                let mut block = vec![0; length];
                file.seek(SeekFrom::Start(offset))
                    .and_then(|_| file.read_exact(&mut block))
                    .map_err(|_| DriveError::LocalIo)?;
                let end = offset + length as u64;
                (block, format!("bytes {offset}-{}/{total}", end - 1), end)
            };
            let request = self
                .http
                .put(url)
                .header("Content-Range", range)
                .header("Content-Length", body.len())
                .header("Content-Type", "application/octet-stream")
                .body(body);
            // Never blindly replay an ambiguous chunk: ask the server first.
            match self.send(request, false, &[308]).await {
                Ok(response) if response.status().as_u16() == 308 => {
                    let next = upload_offset(response.headers(), total)?;
                    if next < offset || (!query_offset && next > end) {
                        return Err(DriveError::InvalidResponse);
                    }
                    if next == offset || next == total {
                        failures += 1;
                        if failures >= 4 {
                            return Err(DriveError::RetryExhausted);
                        }
                        retry_delay(failures, None).await;
                    } else {
                        failures = 0;
                    }
                    offset = next;
                    progress(offset, total);
                    query_offset = offset == total;
                }
                Ok(response) => {
                    let value: Value = serde_json::from_slice(&bounded_body(response).await?)
                        .map_err(|_| DriveError::InvalidResponse)?;
                    if value["id"] != file_id || file_size(&value)? != total {
                        return Err(DriveError::SizeMismatch);
                    }
                    let current = file.metadata().map_err(|_| DriveError::LocalIo)?;
                    if current.len() != total || current.modified().ok() != before.modified().ok() {
                        return Err(DriveError::LocalIo);
                    }
                    if file_size(&self.metadata(file_id).await?)? != total {
                        return Err(DriveError::SizeMismatch);
                    }
                    session_sink(None)?;
                    progress(total, total);
                    return Ok(UploadResult {
                        file_id: file_id.into(),
                        uploaded_bytes: total,
                    });
                }
                Err(DriveError::NotFound) => {
                    restarts += 1;
                    if restarts > 1 {
                        return Err(DriveError::RetryExhausted);
                    }
                    session_sink(None)?;
                    session = None;
                }
                Err(DriveError::Network) => {
                    failures += 1;
                    if failures >= 4 {
                        return Err(DriveError::RetryExhausted);
                    }
                    query_offset = true;
                    retry_delay(failures, None).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub async fn read_json(&self, file_id: &str) -> Result<Vec<u8>, DriveError> {
        validate_id(file_id)?;
        let request = self
            .http
            .get(self.api(&format!("files/{file_id}"))?)
            .query(&[("alt", "media")]);
        bounded_body(self.send(request, true, &[]).await?).await
    }
    pub async fn update_bytes(&self, file_id: &str, bytes: &[u8]) -> Result<(), DriveError> {
        validate_id(file_id)?;
        if bytes.len() > MAX_JSON {
            return Err(DriveError::InvalidJob);
        }
        let request = self
            .http
            .patch(self.upload(&format!("files/{file_id}"))?)
            .query(&[("uploadType", "media")])
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec());
        bounded_body(self.send(request, true, &[]).await?).await?;
        Ok(())
    }
    pub async fn download_resumable(
        &self,
        file_id: &str,
        part_path: &Path,
        progress: TransferProgress,
    ) -> Result<(), DriveError> {
        use std::io::{Seek, SeekFrom, Write};
        validate_id(file_id)?;
        if part_path.extension().and_then(|s| s.to_str()) != Some("part") {
            return Err(DriveError::LocalIo);
        }
        super::output::validate_download_path(part_path).map_err(|_| DriveError::LocalIo)?;
        if let Ok(metadata) = std::fs::symlink_metadata(part_path) {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(DriveError::LocalIo);
            }
        }
        let total = file_size(&self.metadata(file_id).await?)?;
        if total == 0 {
            return Err(DriveError::SizeMismatch);
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(part_path)
            .map_err(|_| DriveError::LocalIo)?;
        super::output::validate_download_path(part_path).map_err(|_| DriveError::LocalIo)?;
        let mut offset = file.metadata().map_err(|_| DriveError::LocalIo)?.len();
        if offset > total {
            return Err(DriveError::SizeMismatch);
        }
        let mut failures = 0;
        while offset < total {
            let mut request = self
                .http
                .get(self.api(&format!("files/{file_id}"))?)
                .query(&[("alt", "media")]);
            if offset > 0 {
                request = request.header("Range", format!("bytes={offset}-"));
            }
            let mut response = self.send(request, true, &[]).await?;
            let remaining = if response.status().as_u16() == 206 {
                let range = response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .ok_or(DriveError::InvalidResponse)?;
                let (start, end, actual_total) = download_range(range)?;
                if start != offset || actual_total != total || end != total - 1 {
                    return Err(DriveError::InvalidResponse);
                }
                total - offset
            } else if response.status().as_u16() == 200 {
                total
            } else {
                return Err(DriveError::InvalidResponse);
            };
            if response
                .content_length()
                .is_some_and(|length| length != remaining)
            {
                return Err(DriveError::SizeMismatch);
            }
            if response.status().as_u16() == 200 && offset > 0 {
                file.set_len(0).map_err(|_| DriveError::LocalIo)?;
                offset = 0;
            }
            file.seek(SeekFrom::Start(offset))
                .map_err(|_| DriveError::LocalIo)?;
            let mut interrupted = false;
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        if chunk.len() as u64 > total - offset {
                            return Err(DriveError::SizeMismatch);
                        }
                        super::output::validate_download_path(part_path)
                            .map_err(|_| DriveError::LocalIo)?;
                        file.write_all(&chunk).map_err(|_| DriveError::LocalIo)?;
                        offset += chunk.len() as u64;
                        progress(offset, total);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        interrupted = true;
                        break;
                    }
                }
            }
            file.sync_all().map_err(|_| DriveError::LocalIo)?;
            super::output::validate_download_path(part_path).map_err(|_| DriveError::LocalIo)?;
            if interrupted || offset != total {
                failures += 1;
                if failures >= 4 {
                    return Err(DriveError::RetryExhausted);
                }
                retry_delay(failures, None).await;
            }
        }
        file.sync_all().map_err(|_| DriveError::LocalIo)?;
        progress(total, total);
        Ok(())
    }
    pub async fn trash(&self, file_id: &str) -> Result<(), DriveError> {
        let mut metadata = self.metadata(file_id).await?;
        let already_trashed = metadata["trashed"].as_bool().ok_or(DriveError::Conflict)?;
        // Identity checks apply on retries too; only the trash flag is allowed
        // to have changed after an earlier successful cleanup.
        metadata["trashed"] = json!(false);
        let job_id = metadata["appProperties"]["xix_job_id"]
            .as_str()
            .and_then(|v| Uuid::parse_str(v).ok())
            .filter(|id| !id.is_nil())
            .ok_or(DriveError::Conflict)?;
        let parent = metadata["parents"]
            .as_array()
            .filter(|p| p.len() == 1)
            .and_then(|p| p[0].as_str())
            .ok_or(DriveError::Conflict)?;
        validate_id(parent)?;
        let expected = self.job_metadata(
            file_id,
            &job_id.to_string(),
            FOLDER_MIME,
            parent,
            "job",
            job_id,
        );
        check_metadata(&metadata, &expected)?;
        let parent_meta = self.metadata(parent).await?;
        if parent_meta["appProperties"]["xix_kind"] != "jobs"
            || parent_meta["mimeType"] != FOLDER_MIME
        {
            return Err(DriveError::Conflict);
        }
        if already_trashed {
            return Ok(());
        }
        self.send(
            self.http
                .patch(self.api(&format!("files/{file_id}"))?)
                .json(&json!({"trashed":true})),
            true,
            &[],
        )
        .await?;
        Ok(())
    }

    fn api(&self, path: &str) -> Result<Url, DriveError> {
        self.endpoints
            .api
            .join(path)
            .map_err(|_| DriveError::UnsafeUrl)
    }
    fn upload(&self, path: &str) -> Result<Url, DriveError> {
        self.endpoints
            .upload
            .join(path)
            .map_err(|_| DriveError::UnsafeUrl)
    }

    fn session_url(&self, value: &str, file_id: &str) -> Result<Url, DriveError> {
        let url = Url::parse(value).map_err(|_| DriveError::UnsafeUrl)?;
        let expected_path = self.upload(&format!("files/{file_id}"))?;
        if value.len() > 8192
            || value
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || url.origin() != self.endpoints.upload.origin()
            || url.path() != expected_path.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url
                .query_pairs()
                .filter(|(key, value)| key == "upload_id" && !value.is_empty())
                .count()
                != 1
        {
            return Err(DriveError::UnsafeUrl);
        }
        Ok(url)
    }

    async fn send(
        &self,
        request: RequestBuilder,
        retry: bool,
        allowed: &[u16],
    ) -> Result<Response, DriveError> {
        let mut retried_auth = false;
        let mut failures = 0;
        loop {
            let access = match self.tokens.access_token().await {
                Ok(access) => access,
                Err(error) => {
                    debug::event("http", format!("token akses tidak tersedia: {error}"));
                    return Err(DriveError::Authentication);
                }
            };
            let response = request
                .try_clone()
                .ok_or(DriveError::InvalidJob)?
                .bearer_auth(access)
                .send()
                .await;
            let mut retry_after = None;
            let retryable = match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    debug::event(
                        "http",
                        format!("Google Drive merespons HTTP {status}; percobaan {}", failures + 1),
                    );
                    if status == 401 {
                        if retried_auth {
                            debug::event("http", "Google Drive tetap menolak token setelah refresh");
                            return Err(DriveError::Authentication);
                        }
                        debug::event("http", "token Drive ditolak; mencoba refresh access token");
                        self.tokens
                            .invalidate_access_token()
                            .await
                            .map_err(|_| DriveError::Authentication)?;
                        retried_auth = true;
                        continue;
                    }
                    if response.status().is_success() || allowed.contains(&status) {
                        return Ok(response);
                    }
                    if response.status().is_redirection() {
                        debug::event("http", "redirect Google Drive ditolak demi keamanan");
                        return Err(DriveError::UnsafeUrl);
                    }
                    if status == 408 || status == 429 || status >= 500 {
                        retry_after = response
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok());
                        true
                    } else {
                        return Err(match status {
                            404 | 410 => DriveError::NotFound,
                            409 => DriveError::Conflict,
                            403 => {
                                let value: Value =
                                    serde_json::from_slice(&bounded_body(response).await?)
                                        .unwrap_or(Value::Null);
                                match value["error"]["errors"][0]["reason"].as_str() {
                                    Some("storageQuotaExceeded") => DriveError::Quota,
                                    Some("accessNotConfigured" | "serviceDisabled") => {
                                        DriveError::ApiDisabled
                                    }
                                    _ => DriveError::Permission,
                                }
                            }
                            _ => DriveError::InvalidResponse,
                        });
                    }
                }
                Err(error) => {
                    debug::event(
                        "http",
                        format!(
                            "transport Google Drive gagal; timeout={}, connect={}, request={}",
                            error.is_timeout(),
                            error.is_connect(),
                            error.is_request()
                        ),
                    );
                    true
                }
            };
            if retryable {
                if !retry {
                    debug::event("http", "permintaan tidak diulang pada tahap ini");
                    return Err(DriveError::Network);
                }
                failures += 1;
                if failures >= 4 {
                    debug::event("http", "percobaan Google Drive habis");
                    return Err(DriveError::RetryExhausted);
                }
                debug::event("http", format!("menunggu sebelum percobaan ulang ke-{failures}"));
                retry_delay(failures, retry_after).await;
            }
        }
    }

    async fn metadata(&self, id: &str) -> Result<Value, DriveError> {
        validate_id(id)?;
        let response = self
            .send(
                self.http
                    .get(self.api(&format!("files/{id}"))?)
                    .query(&[("fields", FILE_FIELDS)]),
                true,
                &[],
            )
            .await?;
        let value: Value = serde_json::from_slice(&bounded_body(response).await?)
            .map_err(|_| DriveError::InvalidResponse)?;
        if value["id"] != id {
            return Err(DriveError::Conflict);
        }
        Ok(value)
    }

    async fn generate_ids(&self, count: usize) -> Result<Vec<String>, DriveError> {
        let response = self
            .send(
                self.http.get(self.api("files/generateIds")?).query(&[
                    ("count", count.to_string()),
                    ("space", "drive".into()),
                    ("type", "files".into()),
                ]),
                true,
                &[],
            )
            .await?;
        #[derive(Deserialize)]
        struct Generated {
            ids: Vec<String>,
        }
        let generated: Generated = serde_json::from_slice(&bounded_body(response).await?)
            .map_err(|_| DriveError::InvalidResponse)?;
        if generated.ids.len() != count {
            return Err(DriveError::InvalidResponse);
        }
        validate_distinct_ids(generated.ids.iter().map(String::as_str))?;
        Ok(generated.ids)
    }

    async fn find_or_create(
        &self,
        kind: &str,
        name: &str,
        mime: &str,
        parent: &str,
        bytes: Option<&[u8]>,
    ) -> Result<String, DriveError> {
        let mut query = format!(
            "trashed = false and appProperties has {{ key='xix_kind' and value='{kind}' }}"
        );
        if parent != "root" {
            validate_id(parent)?;
            query.push_str(&format!(" and '{parent}' in parents"));
        }
        let response = self
            .send(
                self.http.get(self.api("files")?).query(&[
                    ("q", query.as_str()),
                    (
                        "fields",
                        "nextPageToken,files(id,name,mimeType,parents,trashed,appProperties,size)",
                    ),
                    ("pageSize", "100"),
                ]),
                true,
                &[],
            )
            .await?;
        let value: Value = serde_json::from_slice(&bounded_body(response).await?)
            .map_err(|_| DriveError::InvalidResponse)?;
        if value.get("nextPageToken").is_some() {
            return Err(DriveError::WorkspaceMismatch);
        }
        let found = value["files"]
            .as_array()
            .ok_or(DriveError::InvalidResponse)?;
        if found.len() > 1 {
            return Err(DriveError::WorkspaceMismatch);
        }
        let id = if let Some(file) = found.first() {
            file["id"]
                .as_str()
                .ok_or(DriveError::InvalidResponse)?
                .to_string()
        } else {
            self.generate_ids(1).await?.remove(0)
        };
        validate_id(&id)?;
        let expected = json!({"id":id,"name":name,"mimeType":mime,"parents":[parent],"appProperties":{"xix_kind":kind,"xix_version":"1"}});
        if let Some(file) = found.first() {
            check_metadata(file, &expected).map_err(|_| DriveError::WorkspaceMismatch)?;
        } else {
            self.create_once(&expected, bytes).await?;
        }
        Ok(id)
    }

    async fn validate_marker(&self, id: &str) -> Result<(), DriveError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Marker {
            schema_version: u32,
            desktop_install_id: Uuid,
        }
        let marker: Marker = serde_json::from_slice(&self.read_json(id).await?)
            .map_err(|_| DriveError::WorkspaceMismatch)?;
        if marker.schema_version != 1 || marker.desktop_install_id != self.install_id {
            return Err(DriveError::WorkspaceMismatch);
        }
        Ok(())
    }

    pub async fn validate_workspace(&self, ids: &WorkspaceIds) -> Result<(), DriveError> {
        validate_distinct_ids(ids.all())?;
        for (id, name, mime, parent, kind) in [
            (
                &ids.root_id,
                "XIX-Upscaler",
                FOLDER_MIME,
                "root",
                "workspace",
            ),
            (
                &ids.jobs_id,
                "jobs",
                FOLDER_MIME,
                ids.root_id.as_str(),
                "jobs",
            ),
            (
                &ids.marker_id,
                "desktop-marker.json",
                "application/json",
                ids.root_id.as_str(),
                "marker",
            ),
        ] {
            let metadata = self.metadata(id).await.map_err(|e| {
                if matches!(e, DriveError::NotFound | DriveError::Conflict) {
                    DriveError::WorkspaceMismatch
                } else {
                    e
                }
            })?;
            check_metadata(&metadata,&json!({"id":id,"name":name,"mimeType":mime,"parents":[parent],"appProperties":{"xix_kind":kind,"xix_version":"1"}})).map_err(|_|DriveError::WorkspaceMismatch)?;
        }
        self.validate_marker(&ids.marker_id).await
    }

    fn job_metadata(
        &self,
        id: &str,
        name: &str,
        mime: &str,
        parent: &str,
        kind: &str,
        job_id: Uuid,
    ) -> Value {
        json!({"id":id,"name":name,"mimeType":mime,"parents":[parent],"appProperties":{"xix_kind":kind,"xix_version":"1","xix_job_id":job_id.to_string(),"xix_install_id":self.install_id.to_string()}})
    }

    async fn create_once(&self, metadata: &Value, bytes: Option<&[u8]>) -> Result<(), DriveError> {
        let id = metadata["id"].as_str().ok_or(DriveError::InvalidId)?;
        match self.metadata(id).await {
            Ok(existing) => return check_metadata(&existing, metadata),
            Err(DriveError::NotFound) => (),
            Err(error) => return Err(error),
        }
        let request = if let Some(bytes) = bytes {
            let boundary = format!("xix_{}", Uuid::new_v4().simple());
            let mut body = format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{metadata}\r\n--{boundary}\r\nContent-Type: {}\r\n\r\n",metadata["mimeType"].as_str().ok_or(DriveError::InvalidJob)?).into_bytes();
            body.extend_from_slice(bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            self.http
                .post(self.upload("files")?)
                .query(&[("uploadType", "multipart"), ("fields", FILE_FIELDS)])
                .header(
                    "Content-Type",
                    format!("multipart/related; boundary={boundary}"),
                )
                .body(body)
        } else {
            self.http
                .post(self.api("files")?)
                .query(&[("fields", FILE_FIELDS)])
                .json(metadata)
        };
        match self.send(request, true, &[]).await {
            Ok(response) => {
                bounded_body(response).await?;
            }
            Err(DriveError::Conflict) => (),
            Err(error) => return Err(error),
        }
        check_metadata(&self.metadata(id).await?, metadata)
    }
}

const MAX_JSON: usize = 1024 * 1024;
const FILE_FIELDS: &str = "id,name,mimeType,parents,trashed,appProperties,size";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

fn upload_offset(headers: &reqwest::header::HeaderMap, total: u64) -> Result<u64, DriveError> {
    let Some(range) = headers.get("range") else {
        return Ok(0);
    };
    let end = range
        .to_str()
        .ok()
        .and_then(|s| s.strip_prefix("bytes=0-"))
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .and_then(|n| n.checked_add(1))
        .filter(|n| *n <= total)
        .ok_or(DriveError::InvalidResponse)?;
    Ok(end)
}
fn download_range(range: &str) -> Result<(u64, u64, u64), DriveError> {
    let (extent, total) = range
        .strip_prefix("bytes ")
        .and_then(|s| s.split_once('/'))
        .ok_or(DriveError::InvalidResponse)?;
    let (start, end) = extent.split_once('-').ok_or(DriveError::InvalidResponse)?;
    let parse = |s: &str| {
        s.parse::<u64>()
            .ok()
            .filter(|_| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or(DriveError::InvalidResponse)
    };
    let (start, end, total) = (parse(start)?, parse(end)?, parse(total)?);
    if start > end || end >= total {
        return Err(DriveError::InvalidResponse);
    }
    Ok((start, end, total))
}

impl WorkspaceIds {
    pub(super) fn all(&self) -> [&str; 3] {
        [&self.root_id, &self.marker_id, &self.jobs_id]
    }
}
impl RemoteFiles {
    pub(super) fn all(&self) -> [&str; 7] {
        [
            &self.folder_id,
            &self.input_id,
            &self.manifest_id,
            &self.status_a_id,
            &self.status_b_id,
            &self.control_id,
            &self.output_id,
        ]
    }
}
pub(super) fn validate_id(id: &str) -> Result<(), DriveError> {
    if id.is_empty()
        || id.len() > 256
        || id == "root"
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(DriveError::InvalidId);
    }
    Ok(())
}
pub(super) fn validate_distinct_ids<'a>(
    ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), DriveError> {
    let mut used = std::collections::HashSet::new();
    for id in ids {
        validate_id(id)?;
        if !used.insert(id) {
            return Err(DriveError::InvalidId);
        }
    }
    Ok(())
}
fn check_metadata(actual: &Value, expected: &Value) -> Result<(), DriveError> {
    let trashed_matches = actual["trashed"] == false;
    let id_matches = actual["id"] == expected["id"];
    let name_matches = actual["name"] == expected["name"];
    let mime_matches = metadata_mime_matches(actual, expected);
    let properties_match = actual["appProperties"] == expected["appProperties"];
    let parents_match = expected["parents"] == json!(["root"])
        || actual["parents"] == expected["parents"];
    if !(trashed_matches
        && id_matches
        && name_matches
        && mime_matches
        && properties_match
        && parents_match)
    {
        debug::event(
            "drive",
            format!(
                "metadata file tidak cocok; jenis={}; trashed={}, id={}, nama={}, mime={}, properties={}, parent={}",
                expected["appProperties"]["xix_kind"]
                    .as_str()
                    .unwrap_or("unknown"),
                trashed_matches,
                id_matches,
                name_matches,
                mime_matches,
                properties_match,
                parents_match,
            ),
        );
        return Err(DriveError::Conflict);
    }
    Ok(())
}

fn metadata_mime_matches(actual: &Value, expected: &Value) -> bool {
    if actual["mimeType"] == expected["mimeType"] {
        return true;
    }

    // Google Drive may replace the generic MIME used while reserving the
    // input slot with the detected video MIME (for example, video/mp4).
    // The input is a video by protocol, so accept that safe specialization
    // while keeping every other metadata check strict.
    expected["mimeType"].as_str() == Some("application/octet-stream")
        && expected["appProperties"]["xix_kind"].as_str() == Some("input")
        && actual["mimeType"]
            .as_str()
            .map(|mime| mime.starts_with("video/"))
            .unwrap_or(false)
}
pub(super) fn manifest_input(value: &Value) -> Result<(Uuid, String, u64), DriveError> {
    let id = value["job_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| !id.is_nil())
        .ok_or(DriveError::InvalidJob)?;
    let extension = value["input"]["extension"]
        .as_str()
        .ok_or(DriveError::InvalidJob)?;
    let name = format!("input.{extension}");
    let size = value["input"]["size_bytes"]
        .as_u64()
        .ok_or(DriveError::InvalidJob)?;
    if value["schema_version"] != 2
        || value["worker_version"] != "0.2.0"
        || value["exchange_protocol"] != "drive-slots-v1"
        || !["mp4", "mov", "mkv", "webm", "ts", "m4v"].contains(&extension)
        || value["input"]["name"] != name
        || !(1..=104857600).contains(&size)
    {
        return Err(DriveError::InvalidJob);
    }
    Ok((id, name, size))
}
fn file_size(value: &Value) -> Result<u64, DriveError> {
    value["size"]
        .as_str()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .ok_or(DriveError::InvalidResponse)
}
async fn bounded_body(mut response: Response) -> Result<Vec<u8>, DriveError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| DriveError::Network)? {
        if bytes.len() + chunk.len() > MAX_JSON {
            return Err(DriveError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn retry_delay(failures: u32, retry_after: Option<u64>) {
    let jitter = u64::from(Uuid::new_v4().as_bytes()[0]);
    let millis = retry_after
        .map(|s| s.min(30) * 1000)
        .unwrap_or((250u64 << failures.min(4)) + jitter);
    tokio::time::sleep(Duration::from_millis(millis)).await;
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
