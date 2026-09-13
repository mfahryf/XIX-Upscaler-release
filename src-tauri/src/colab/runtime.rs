//! Production adapters. The lifecycle itself is independent of Tauri and HTTP.
use super::{
    auth::{AccessTokenProvider, AuthError, AuthStatus, OAuthManager},
    coordinator::*,
    debug,
    drive::{
        DriveClient, DriveError, DriveUser, RemoteFiles, SessionSink, TransferProgress,
        WorkspaceIds,
    },
    manifest::{self, PreparedJob},
    registry::RemoteJobRecord,
    remote_status::CompletedOutput,
    ColabError,
};
use crate::engines::EngineOptions;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

pub fn initialize(
    oauth: Arc<OAuthManager>,
    app_data: &Path,
    resource_root: Option<&Path>,
    cwd: &Path,
    events: Arc<dyn ColabEvents>,
) -> Result<Arc<ColabCoordinator>, ColabError> {
    oauth
        .initialize(app_data.to_path_buf())
        .map_err(auth_error)?;
    let install_id = super::output::install_id(app_data)?;
    let registry = Arc::new(
        super::registry::RemoteRegistry::load(app_data)
            .map_err(|_| ColabError("Catatan pekerjaan Colab tidak dapat dibaca dengan aman"))?,
    );
    let drive = Arc::new(DriveClient::new(oauth.clone(), install_id).map_err(drive_error)?);
    let probe = Arc::new(LocalProbe {
        ffprobe: super::media::resolve_ffprobe(resource_root, cwd),
    });
    debug::event("runtime", "integrasi Colab siap");
    Ok(Arc::new(ColabCoordinator::new(
        oauth,
        drive,
        probe,
        registry,
        Arc::new(PollClock),
        events,
    )))
}

impl ColabAuth for OAuthManager {
    fn status(&self) -> AuthStatus {
        OAuthManager::status(self)
    }
    fn ensure_connected(&self, interactive: bool) -> ColabFuture<'_, ()> {
        Box::pin(async move {
            if self.status().connected {
                match self.access_token().await {
                    Ok(_) => return Ok(()),
                    Err(error) if interactive && error.requires_reconnect() => {}
                    Err(error) => return Err(auth_error(error)),
                }
            }
            if !interactive {
                return Err(auth_error(AuthError::ReconnectRequired));
            }
            self.connect().await.map(|_| ()).map_err(auth_error)
        })
    }
    fn disconnect(&self) -> ColabFuture<'_, ()> {
        Box::pin(async move { OAuthManager::disconnect(self).await.map_err(auth_error) })
    }
}
impl ColabDrive for DriveClient {
    fn account_identity(&self) -> ColabFuture<'_, DriveUser> {
        Box::pin(async move { self.about_user().await.map_err(drive_error) })
    }
    fn ensure_workspace(&self) -> ColabFuture<'_, WorkspaceIds> {
        Box::pin(async move {
            DriveClient::ensure_workspace(self)
                .await
                .map_err(drive_error)
        })
    }
    fn validate_workspace<'a>(&'a self, ids: &'a WorkspaceIds) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            DriveClient::validate_workspace(self, ids)
                .await
                .map_err(drive_error)
        })
    }
    fn reserve_job_files(&self) -> ColabFuture<'_, RemoteFiles> {
        Box::pin(async move {
            DriveClient::reserve_job_files(self)
                .await
                .map_err(drive_error)
        })
    }
    fn create_job_files<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            let job = PreparedJob {
                job_id: r.job_id,
                input_path: r.local_input.clone(),
                output_path: r.local_output.clone(),
                input_sha256: r.input_sha256.clone(),
                source_media: r.source_media.clone(),
                manifest_json: r.manifest_json.clone(),
            };
            DriveClient::create_job_files(self, &r.workspace_ids, &job, &r.remote_files)
                .await
                .map_err(drive_error)
        })
    }
    fn manifest_published<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, bool> {
        Box::pin(async move {
            match self.read_json(&r.remote_files.manifest_id).await {
                Err(DriveError::NotFound) => Ok(false),
                Err(error) => Err(drive_error(error)),
                Ok(bytes) => {
                    let value: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|_| drive_error(DriveError::InvalidResponse))?;
                    if value != r.manifest_json {
                        return Err(drive_error(DriveError::Conflict));
                    }
                    Ok(true)
                }
            }
        })
    }
    fn upload_input<'a>(
        &'a self,
        r: &'a RemoteJobRecord,
        sink: SessionSink,
        progress: TransferProgress,
    ) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            self.upload_resumable(
                &r.remote_files.input_id,
                &r.local_input,
                r.upload_session.as_deref(),
                sink,
                progress,
            )
            .await
            .map(|_| ())
            .map_err(drive_error)
        })
    }
    fn publish_manifest<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            let bytes = serde_json::to_vec(&r.manifest_json)
                .map_err(|_| drive_error(DriveError::InvalidJob))?;
            DriveClient::publish_manifest(self, &r.remote_files, &bytes)
                .await
                .map_err(drive_error)
        })
    }
    fn read_status_slots<'a>(
        &'a self,
        files: &'a RemoteFiles,
    ) -> ColabFuture<'a, (Vec<u8>, Vec<u8>)> {
        Box::pin(async move {
            let (a, b) = tokio::join!(
                self.read_json(&files.status_a_id),
                self.read_json(&files.status_b_id)
            );
            match (a, b) {
                (Err(error), Err(_)) => Err(drive_error(error)),
                (a, b) => Ok((a.unwrap_or_default(), b.unwrap_or_default())),
            }
        })
    }
    fn write_control<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            let bytes=serde_json::to_vec(&serde_json::json!({"schema_version":1,"revision":r.control_revision,"action":r.desired_control})).map_err(|_|drive_error(DriveError::InvalidJob))?;
            self.update_bytes(&r.remote_files.control_id, &bytes)
                .await
                .map_err(drive_error)
        })
    }
    fn download_output<'a>(
        &'a self,
        r: &'a RemoteJobRecord,
        part: &'a Path,
        progress: TransferProgress,
    ) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            self.download_resumable(&r.remote_files.output_id, part, progress)
                .await
                .map_err(drive_error)
        })
    }
    fn trash_job<'a>(&'a self, folder: &'a str) -> ColabFuture<'a, ()> {
        Box::pin(async move { self.trash(folder).await.map_err(drive_error) })
    }
}
pub struct LocalProbe {
    pub ffprobe: Result<PathBuf, ColabError>,
}
impl ColabProbe for LocalProbe {
    fn prepare(
        &self,
        input: &Path,
        output: &Path,
        options: &EngineOptions,
        now: DateTime<Utc>,
        reserved: &mut HashSet<PathBuf>,
    ) -> Result<PreparedJob, ColabError> {
        manifest::prepare_job(
            input,
            output,
            options,
            self.ffprobe.as_ref().map_err(Clone::clone)?,
            now,
            reserved,
        )
    }
    fn verify_input(&self, r: &RemoteJobRecord) -> Result<(), ColabError> {
        super::output::verify_input(r)
    }
    fn promote(&self, r: &RemoteJobRecord, expected: &CompletedOutput) -> Result<(), ColabError> {
        super::output::promote_output(
            self.ffprobe.as_ref().map_err(Clone::clone)?,
            &super::output::part_path(&r.local_output),
            &r.local_output,
            expected,
            r.source_media.rotation,
        )
    }
}
pub struct PollClock;
impl ColabClock for PollClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn tick(&self) -> ColabFuture<'_, ()> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(())
        })
    }
}
fn auth_error(error: AuthError) -> ColabError {
    ColabError(match error {
        AuthError::ReconnectRequired => "Hubungkan ulang Google Drive",
        AuthError::Denied => "Izin Google Drive tidak diberikan",
        AuthError::Storage => "Data izin Google Drive tidak dapat disimpan dengan aman",
        AuthError::Configuration => {
            "Konfigurasi OAuth Google tidak cocok; periksa client ID aplikasi Desktop"
        }
        AuthError::InvalidRequest => {
            "Permintaan OAuth Google ditolak; lihat detail di terminal debug"
        }
        AuthError::Network => {
            "Google tidak merespons setelah login; periksa internet, proxy, atau firewall"
        }
        AuthError::Cancelled => "Login Google dibatalkan",
        AuthError::Timeout => "Waktu login Google habis; coba hubungkan kembali",
        AuthError::Browser => "Browser untuk login Google tidak dapat dibuka",
        _ => "Balasan login Google tidak sah; coba hubungkan ulang",
    })
}
pub(super) fn drive_error(error: DriveError) -> ColabError {
    ColabError(match error {
        DriveError::Authentication => "Hubungkan ulang Google Drive",
        DriveError::Permission => "Izin Google Drive tidak mencukupi",
        DriveError::Quota => "Ruang Google Drive tidak mencukupi",
        DriveError::ApiDisabled => "Aktifkan Google Drive API untuk aplikasi ini",
        DriveError::NotFound => "Berkas Google Drive tidak tersedia untuk akun ini",
        DriveError::Conflict => "Berkas Google Drive tidak cocok dengan pekerjaan tersimpan",
        DriveError::Network => "Koneksi Google Drive terputus; mencoba kembali",
        DriveError::RetryExhausted => {
            "Google Drive tidak merespons; periksa internet, proxy, atau firewall lalu coba lagi"
        }
        DriveError::WorkspaceMismatch => {
            "Folder pekerjaan tidak cocok; hubungkan akun Google Drive sebelumnya"
        }
        DriveError::SizeMismatch => "Ukuran berkas Google Drive tidak cocok",
        DriveError::LocalIo => "Berkas transfer lokal tidak dapat dibaca atau ditulis",
        DriveError::Storage => "Catatan pemulihan transfer tidak dapat disimpan",
        _ => "Balasan atau identitas pekerjaan Google Drive tidak sah",
    })
}

pub const NOTEBOOK_URL:&str="https://colab.research.google.com/github/mfahryf/xix-upscaler-colab/blob/main/XIX-Upscaler-Colab.ipynb";
/// No caller-supplied URL or shell command is accepted at this boundary.
pub fn open_notebook() -> Result<(), ColabError> {
    debug::event("notebook", "meminta browser membuka notebook publik");
    #[cfg(windows)]
    {
        use windows::{
            core::{HSTRING, PCWSTR},
            Win32::{
                Foundation::HWND,
                UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            },
        };
        let result = unsafe {
            ShellExecuteW(
                HWND::default(),
                &HSTRING::from("open"),
                &HSTRING::from(NOTEBOOK_URL),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize > 32 {
            debug::event("notebook", "browser notebook berhasil dibuka");
            return Ok(());
        }
    }
    debug::event("notebook", "browser notebook gagal dibuka");
    Err(ColabError("Notebook Colab tidak dapat dibuka di browser"))
}
