//! Desktop-owned lifecycle; external services sit behind narrow async ports.
use super::{
    auth::AuthStatus,
    debug,
    drive::{DriveUser, RemoteFiles, SessionSink, TransferProgress, WorkspaceIds},
    manifest::PreparedJob,
    registry::{ControlAction, RemoteJobRecord, RemoteRegistry},
    remote_status::{CompletedOutput, RemotePhase},
    ColabError,
};
use crate::engines::EngineOptions;
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use serde::Serialize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use uuid::Uuid;

pub type ColabFuture<'a, T> = BoxFuture<'a, Result<T, ColabError>>;
pub trait ColabAuth: Send + Sync {
    fn status(&self) -> AuthStatus;
    fn ensure_connected(&self, interactive: bool) -> ColabFuture<'_, ()>;
    fn disconnect(&self) -> ColabFuture<'_, ()>;
}
pub trait ColabDrive: Send + Sync {
    fn account_identity(&self) -> ColabFuture<'_, DriveUser>;
    fn ensure_workspace(&self) -> ColabFuture<'_, WorkspaceIds>;
    fn validate_workspace<'a>(&'a self, ids: &'a WorkspaceIds) -> ColabFuture<'a, ()>;
    fn reserve_job_files(&self) -> ColabFuture<'_, RemoteFiles>;
    fn create_job_files<'a>(&'a self, record: &'a RemoteJobRecord) -> ColabFuture<'a, ()>;
    fn manifest_published<'a>(&'a self, record: &'a RemoteJobRecord) -> ColabFuture<'a, bool>;
    fn upload_input<'a>(
        &'a self,
        record: &'a RemoteJobRecord,
        sink: SessionSink,
        progress: TransferProgress,
    ) -> ColabFuture<'a, ()>;
    fn publish_manifest<'a>(&'a self, record: &'a RemoteJobRecord) -> ColabFuture<'a, ()>;
    fn read_status_slots<'a>(
        &'a self,
        files: &'a RemoteFiles,
    ) -> ColabFuture<'a, (Vec<u8>, Vec<u8>)>;
    fn write_control<'a>(&'a self, record: &'a RemoteJobRecord) -> ColabFuture<'a, ()>;
    fn download_output<'a>(
        &'a self,
        record: &'a RemoteJobRecord,
        part: &'a Path,
        progress: TransferProgress,
    ) -> ColabFuture<'a, ()>;
    fn trash_job<'a>(&'a self, folder: &'a str) -> ColabFuture<'a, ()>;
}
pub trait ColabProbe: Send + Sync {
    fn prepare(
        &self,
        input: &Path,
        output: &Path,
        options: &EngineOptions,
        now: DateTime<Utc>,
        reserved: &mut HashSet<PathBuf>,
    ) -> Result<PreparedJob, ColabError>;
    fn verify_input(&self, record: &RemoteJobRecord) -> Result<(), ColabError>;
    fn promote(
        &self,
        record: &RemoteJobRecord,
        expected: &CompletedOutput,
    ) -> Result<(), ColabError>;
}
pub trait ColabClock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    fn tick(&self) -> ColabFuture<'_, ()>;
}
#[derive(Clone, Debug, Serialize)]
pub struct ColabEvent {
    pub job_id: Uuid,
    pub file: PathBuf,
    pub engine: Option<String>,
    pub phase: RemotePhase,
    pub percent: f64,
    pub message: String,
    pub is_error: bool,
}
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ColabDone {
    pub ok: usize,
    pub fail: usize,
    pub cancelled: usize,
    pub total: usize,
}
pub trait ColabEvents: Send + Sync {
    fn event(&self, event: ColabEvent);
    fn open_notebook(&self);
    fn done(&self, done: ColabDone);
}
pub struct StartColabRequest {
    pub files: Vec<PathBuf>,
    pub output_dir: PathBuf,
    pub options: EngineOptions,
}
#[derive(Serialize)]
pub struct DriveStatus {
    pub connected: bool,
    pub masked_account: Option<String>,
    pub busy: bool,
}
pub struct ColabCoordinator {
    auth: Arc<dyn ColabAuth>,
    drive: Arc<dyn ColabDrive>,
    probe: Arc<dyn ColabProbe>,
    registry: Arc<RemoteRegistry>,
    clock: Arc<dyn ColabClock>,
    events: Arc<dyn ColabEvents>,
    busy: Arc<AtomicBool>,
    notebook_opened: AtomicBool,
    records: parking_lot::Mutex<()>,
    control: tokio::sync::Mutex<()>,
    action: tokio::sync::watch::Sender<ControlAction>,
    snapshot: parking_lot::Mutex<Vec<ColabEvent>>,
    masked: parking_lot::Mutex<Option<String>>,
    acknowledged: parking_lot::Mutex<std::collections::HashMap<Uuid, u64>>,
}
impl ColabCoordinator {
    pub fn new(
        auth: Arc<dyn ColabAuth>,
        drive: Arc<dyn ColabDrive>,
        probe: Arc<dyn ColabProbe>,
        registry: Arc<RemoteRegistry>,
        clock: Arc<dyn ColabClock>,
        events: Arc<dyn ColabEvents>,
    ) -> Self {
        Self {
            auth,
            drive,
            probe,
            registry,
            clock,
            events,
            busy: Arc::new(AtomicBool::new(false)),
            notebook_opened: AtomicBool::new(false),
            records: Default::default(),
            control: Default::default(),
            action: tokio::sync::watch::channel(ControlAction::Run).0,
            snapshot: Default::default(),
            masked: Default::default(),
            acknowledged: Default::default(),
        }
    }
    fn acquire(&self) -> Result<Lease, ColabError> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ColabError("Pekerjaan atau login Colab masih berjalan"))?;
        Ok(Lease(self.busy.clone()))
    }
    pub fn status(&self) -> DriveStatus {
        let connected = self.auth.status().connected;
        DriveStatus {
            connected,
            masked_account: if connected {
                self.masked.lock().clone()
            } else {
                None
            },
            busy: self.busy.load(Ordering::Acquire),
        }
    }
    pub async fn refresh_status(&self) -> Result<DriveStatus, ColabError> {
        if !self.auth.status().connected {
            debug::event("status", "DRIVE belum terhubung; probe dilewati");
            return Ok(self.status());
        }
        debug::event("status", "memulai probe token dan identitas DRIVE");
        self.auth.ensure_connected(false).await?;
        *self.masked.lock() = self.drive.account_identity().await?.masked_email();
        debug::event("status", "probe DRIVE berhasil");
        Ok(self.status())
    }
    pub async fn connect(&self) -> Result<DriveStatus, ColabError> {
        debug::event("status", "tombol DRIVE meminta koneksi");
        let lease = self.acquire()?;
        self.auth.ensure_connected(true).await?;
        *self.masked.lock() = self.drive.account_identity().await?.masked_email();
        drop(lease);
        debug::event("status", "tombol DRIVE berhasil terhubung dan terverifikasi");
        Ok(self.status())
    }
    pub async fn disconnect(&self) -> Result<(), ColabError> {
        let _lease = self.acquire()?;
        *self.masked.lock() = None;
        self.auth.disconnect().await
    }
    pub async fn start_batch(
        self: &Arc<Self>,
        request: StartColabRequest,
    ) -> Result<(), ColabError> {
        let lease = self.acquire()?;
        self.notebook_opened.store(false, Ordering::Release);
        self.action.send_replace(ControlAction::Run);
        if !matches!(
            request.options.get("engine").and_then(|v| v.as_str()),
            Some("video-colab")
        ) {
            return Err(ColabError("Pilih engine Video Colab"));
        }
        if request.files.is_empty() {
            return Err(ColabError("Pilih video terlebih dahulu"));
        }
        let old = self.registry.snapshot();
        if old.iter().any(|r| !terminal(r.state)) {
            return Err(ColabError(
                "Lanjutkan pekerjaan Colab tersimpan terlebih dahulu",
            ));
        }
        let mut reserved = old
            .iter()
            .map(|r| super::manifest::output_key(&r.local_output))
            .collect();
        let mut jobs = Vec::new();
        // No login, workspace creation, or upload until the WHOLE batch is valid.
        for input in &request.files {
            jobs.push(self.probe.prepare(
                input,
                &request.output_dir,
                &request.options,
                self.clock.now(),
                &mut reserved,
            )?);
        }
        self.snapshot.lock().clear();
        debug::event(
            "batch",
            format!("preflight selesai; {} video siap dikirim", jobs.len()),
        );
        for job in &jobs {
            self.emit(
                job.job_id,
                job.input_path.clone(),
                RemotePhase::Authenticating,
                0.,
                "HUBUNGKAN GOOGLE DRIVE",
                false,
            );
        }
        let keep = request
            .options
            .get("keep_drive_files")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let this = self.clone();
        tokio::spawn(async move {
            let total = jobs.len();
            let result = this.start_jobs(jobs, keep).await;
            let done = match result {
                Ok(done) => done,
                Err(error) => {
                    debug::event("batch", format!("batch berhenti: {}", error.0));
                    let rows = this.snapshot.lock().clone();
                    let cancelled = *this.action.borrow() == ControlAction::Cancel;
                    for row in rows {
                        this.emit(
                            row.job_id,
                            row.file,
                            if cancelled {
                                RemotePhase::Cancelled
                            } else {
                                RemotePhase::Failed
                            },
                            0.,
                            if cancelled { "DIBATALKAN" } else { error.0 },
                            !cancelled,
                        );
                    }
                    ColabDone {
                        total,
                        ok: 0,
                        fail: if cancelled { 0 } else { total },
                        cancelled: if cancelled { total } else { 0 },
                    }
                }
            };
            drop(lease);
            this.events.done(done);
        });
        Ok(())
    }
    async fn start_jobs(
        self: &Arc<Self>,
        jobs: Vec<PreparedJob>,
        keep: bool,
    ) -> Result<ColabDone, ColabError> {
        if *self.action.borrow() == ControlAction::Cancel {
            return Err(ColabError("DIBATALKAN"));
        }
        let mut changes = self.action.subscribe();
        debug::event("batch", "memulai autentikasi Google untuk batch");
        // Dropping an interrupted OAuth future also closes its loopback listener.
        let login = self.auth.ensure_connected(true);
        tokio::pin!(login);
        loop {
            tokio::select! {
                result=&mut login=>{result?;break},
                _=changes.changed()=>{if *changes.borrow_and_update()==ControlAction::Cancel{return Err(ColabError("DIBATALKAN"))}}
            }
        }
        debug::event("batch", "autentikasi Google selesai");
        *self.masked.lock() = self.drive.account_identity().await?.masked_email();
        debug::event("batch", "identitas DRIVE untuk batch tervalidasi");
        let workspace = self.drive.ensure_workspace().await?;
        debug::event("batch", "workspace DRIVE siap");
        let mut ids = Vec::new();
        for job in jobs {
            let files = self.drive.reserve_job_files().await?;
            debug::event("batch", "slot file pekerjaan berhasil dicadangkan");
            let _lock = self.records.lock();
            let desired = *self.action.borrow();
            let record = RemoteJobRecord {
                job_id: job.job_id,
                local_input: job.input_path,
                local_output: job.output_path,
                input_sha256: job.input_sha256,
                source_media: job.source_media,
                manifest_json: job.manifest_json,
                workspace_ids: workspace.clone(),
                remote_files: files,
                upload_session: None,
                state: RemotePhase::Uploading,
                last_revision: 0,
                control_revision: if desired == ControlAction::Run { 0 } else { 1 },
                desired_control: desired,
                keep_drive_files: keep,
                completed_output: None,
            };
            self.registry.upsert(&record).map_err(|_| storage())?;
            if desired == ControlAction::Pause {
                self.emit_record(&record, RemotePhase::Paused, 0., "UNGGAHAN DIJEDA", false);
            }
            ids.push(record.job_id);
        }
        Ok(self.run_records(ids, true).await)
    }
    pub async fn resume_pending(self: &Arc<Self>) -> Result<Vec<ColabEvent>, ColabError> {
        if self.busy.load(Ordering::Acquire) {
            let rows = self.snapshot.lock().clone();
            for row in &rows {
                self.events.event(row.clone())
            }
            return Ok(rows);
        }
        let lease = match self.acquire() {
            Ok(lease) => lease,
            Err(_) => return Ok(self.snapshot.lock().clone()),
        };
        let records = self.registry.snapshot();
        self.snapshot.lock().clear();
        // Completed is local verified success, not worker-only success (which
        // maps to Downloading). Historical results may have been moved/deleted;
        // never replay their publication/cleanup as an active job on startup.
        for r in records.iter().filter(|r| !terminal(r.state)) {
            self.emit_record(
                r,
                r.state,
                0.,
                if self.auth.status().connected {
                    "MEMULIHKAN PEKERJAAN COLAB"
                } else {
                    "HUBUNGKAN GOOGLE DRIVE UNTUK MELANJUTKAN"
                },
                false,
            );
        }
        let rows = self.snapshot.lock().clone();
        let ids: Vec<_> = records
            .iter()
            .filter(|r| !terminal(r.state))
            .map(|r| r.job_id)
            .collect();
        if ids.is_empty() || !self.auth.status().connected {
            return Ok(rows);
        }
        // Preserve each record's pending control; startup must never resume a pause.
        self.action.send_replace(
            if records
                .iter()
                .filter(|r| !terminal(r.state))
                .all(|r| r.desired_control == ControlAction::Pause)
            {
                ControlAction::Pause
            } else {
                ControlAction::Run
            },
        );
        let this = self.clone();
        tokio::spawn(async move {
            let result = async {
                this.auth.ensure_connected(false).await?;
                *this.masked.lock() = this.drive.account_identity().await?.masked_email();
                Ok::<_, ColabError>(this.run_records(ids.clone(), false).await)
            }
            .await;
            let done = match result {
                Ok(done) => done,
                Err(error) => {
                    for id in &ids {
                        if let Ok(r) = this.record(*id) {
                            this.emit_record(&r, RemotePhase::Failed, 0., error.0, true);
                        }
                    }
                    ColabDone {
                        total: ids.len(),
                        fail: ids.len(),
                        ..Default::default()
                    }
                }
            };
            drop(lease);
            this.events.done(done);
        });
        Ok(rows)
    }
    fn record(&self, id: Uuid) -> Result<RemoteJobRecord, ColabError> {
        self.registry
            .snapshot()
            .into_iter()
            .find(|r| r.job_id == id)
            .ok_or_else(storage)
    }
    fn mutate(
        &self,
        id: Uuid,
        change: impl FnOnce(&mut RemoteJobRecord),
    ) -> Result<RemoteJobRecord, ColabError> {
        let _lock = self.records.lock();
        let mut record = self.record(id)?;
        change(&mut record);
        self.registry.upsert(&record).map_err(|_| storage())?;
        Ok(record)
    }
    fn emit(
        &self,
        id: Uuid,
        file: PathBuf,
        phase: RemotePhase,
        percent: f64,
        message: &str,
        is_error: bool,
    ) {
        self.emit_with_engine(id, file, None, phase, percent, message, is_error);
    }
    fn emit_with_engine(
        &self,
        id: Uuid,
        file: PathBuf,
        engine: Option<String>,
        phase: RemotePhase,
        percent: f64,
        message: &str,
        is_error: bool,
    ) {
        let row = ColabEvent {
            job_id: id,
            file,
            engine,
            phase,
            percent,
            message: message.into(),
            is_error,
        };
        {
            let mut rows = self.snapshot.lock();
            if let Some(old) = rows.iter_mut().find(|r| r.job_id == id) {
                *old = row.clone()
            } else {
                rows.push(row.clone())
            }
        }
        self.events.event(row);
    }
    fn emit_record(
        &self,
        r: &RemoteJobRecord,
        phase: RemotePhase,
        percent: f64,
        message: &str,
        error: bool,
    ) {
        let engine = Some("video-colab".to_string());
        self.emit_with_engine(
            r.job_id,
            r.local_input.clone(),
            engine,
            phase,
            percent,
            message,
            error,
        )
    }
    pub async fn set_paused(&self, paused: bool) -> Result<(), ColabError> {
        self.set_action(if paused {
            ControlAction::Pause
        } else {
            ControlAction::Run
        })
        .await
    }
    pub async fn cancel(&self) -> Result<(), ColabError> {
        self.set_action(ControlAction::Cancel).await
    }
    async fn set_action(&self, action: ControlAction) -> Result<(), ColabError> {
        if action == ControlAction::Pause {
            self.notebook_opened.store(false, Ordering::Release);
        }
        {
            let _records = self.records.lock();
            for mut r in self
                .registry
                .snapshot()
                .into_iter()
                .filter(|r| !terminal(r.state))
            {
                if r.desired_control == ControlAction::Cancel || r.desired_control == action {
                    continue;
                }
                r.control_revision = r.control_revision.checked_add(1).ok_or_else(storage)?;
                r.desired_control = action;
                self.registry.upsert(&r).map_err(|_| storage())?;
            }
            self.action.send_replace(action);
        }
        if action == ControlAction::Pause {
            for r in self.registry.snapshot().iter().filter(|r| {
                r.desired_control == ControlAction::Pause && r.state == RemotePhase::Uploading
            }) {
                self.emit_record(r, RemotePhase::Paused, 0., "ANTREAN DIJEDA", false);
            }
        }
        let mut error = None;
        for r in self.registry.snapshot().into_iter().filter(|r| {
            !terminal(r.state) && r.state != RemotePhase::Uploading && r.completed_output.is_none()
        }) {
            if let Err(e) = self.send_control(r.job_id).await {
                error = Some(e)
            }
        }
        if error.is_none()
            && action == ControlAction::Run
            && self.registry.snapshot().iter().any(|record| {
                record.state != RemotePhase::Uploading
                    && !terminal(record.state)
                    && record.completed_output.is_none()
            })
            && !self.notebook_opened.swap(true, Ordering::AcqRel)
        {
            self.events.open_notebook();
        }
        error.map_or(Ok(()), Err)
    }
    async fn send_control(&self, id: Uuid) -> Result<(), ColabError> {
        // Serialize both the command and its retry; never send an older revision last.
        let _control = self.control.lock().await;
        let record = self.record(id)?;
        if record.control_revision == 0
            || self
                .acknowledged
                .lock()
                .get(&id)
                .is_some_and(|revision| *revision >= record.control_revision)
        {
            return Ok(());
        }
        self.drive.write_control(&record).await?;
        self.acknowledged.lock().insert(id, record.control_revision);
        Ok(())
    }
    async fn upload(
        self: &Arc<Self>,
        id: Uuid,
        index: usize,
        total: usize,
    ) -> Result<(), ColabError> {
        let mut changes = self.action.subscribe();
        loop {
            let r = self.record(id)?;
            // Publication may have succeeded just before the previous process
            // died. Resolve that boundary BEFORE honoring upload-only controls.
            if self.drive.manifest_published(&r).await? {
                self.mutate(id, |r| {
                    r.upload_session = None;
                    r.state = RemotePhase::WaitingForColab;
                })?;
                return Ok(());
            }
            if r.desired_control == ControlAction::Cancel {
                return Ok(());
            }
            if r.desired_control == ControlAction::Pause {
                self.emit_record(&r, RemotePhase::Paused, 0., "UNGGAHAN DIJEDA", false);
                for other in self.registry.snapshot().into_iter().filter(|other| {
                    other.job_id != id
                        && other.state != RemotePhase::Uploading
                        && !terminal(other.state)
                        && other.completed_output.is_none()
                }) {
                    let _ = self.observe_status(other.job_id).await;
                }
                tokio::select! {
                    changed=changes.changed()=>{changed.map_err(|_| storage())?;},
                    _=self.clock.tick()=>{}
                }
                continue;
            }
            self.probe.verify_input(&r)?;
            debug::event("upload", format!("menyiapkan pekerjaan {} dari {}", index + 1, total));
            self.drive.create_job_files(&r).await?;
            self.send_control(id).await?;
            let this = self.clone();
            let sink: SessionSink = Arc::new(move |session| {
                this.mutate(id, |r| r.upload_session = session)
                    .map(|_| ())
                    .map_err(|_| super::drive::DriveError::Storage)
            });
            let this = self.clone();
            let file = r.local_input.clone();
            let progress: TransferProgress = Arc::new(move |sent, bytes| {
                let percent = if bytes > 0 {
                    sent as f64 * 100. / bytes as f64
                } else {
                    0.
                };
                this.emit(
                    id,
                    file.clone(),
                    RemotePhase::Uploading,
                    percent,
                    &format!("MENGUNGGAH {} DARI {} — {:.0}%", index + 1, total, percent),
                    false,
                );
            });
            let transfer = self.drive.upload_input(&r, sink, progress);
            tokio::select! {
                result=transfer=>{result?;},
                _=changes.changed()=>{continue;}
            }
            debug::event("upload", format!("unggah pekerjaan {} selesai; menerbitkan manifest", index + 1));
            let _control = self.control.lock().await;
            let latest = self.record(id)?;
            if latest.desired_control != ControlAction::Run {
                continue;
            }
            // No worker-visible manifest exists before upload is acknowledged.
            self.drive.publish_manifest(&latest).await?;
            let saved = self.mutate(id, |r| {
                r.upload_session = None;
                r.state = RemotePhase::WaitingForColab;
            })?;
            self.emit_record(&saved, saved.state, 0., "MENUNGGU COLAB DIJALANKAN", false);
            debug::event("upload", format!("pekerjaan {} siap diproses Colab", index + 1));
            return Ok(());
        }
    }
    async fn run_records(self: &Arc<Self>, ids: Vec<Uuid>, open: bool) -> ColabDone {
        let mut done = ColabDone {
            total: ids.len(),
            ..Default::default()
        };
        let mut pending = Vec::new();
        let mut published = 0;
        // A single upload is deliberately used: within the two-upload ceiling,
        // deterministic playlist order, and bounded memory on modest machines.
        for (index, id) in ids.iter().copied().enumerate() {
            let result = async {
                let r = self.record(id)?;
                self.drive.validate_workspace(&r.workspace_ids).await?;
                if r.state == RemotePhase::Uploading {
                    self.upload(id, index, ids.len()).await?;
                }
                let r = self.record(id)?;
                if r.desired_control != ControlAction::Run && r.state != RemotePhase::Uploading {
                    self.send_control(id).await?;
                }
                Ok::<_, ColabError>(r)
            }
            .await;
            match result {
                Ok(r) => {
                    if r.state != RemotePhase::Uploading
                        && r.desired_control != ControlAction::Cancel
                    {
                        published += 1
                    }
                    pending.push(id)
                }
                Err(error) => {
                    debug::event("batch", format!("pekerjaan gagal disiapkan: {}", error.0));
                    done.fail += 1;
                    if let Ok(r) = self.record(id) {
                        self.emit_record(&r, RemotePhase::Failed, 0., error.0, true)
                    }
                }
            }
        }
        if open
            && done.fail == 0
            && published > 0
            && *self.action.borrow() != ControlAction::Cancel
            && !self.notebook_opened.swap(true, Ordering::AcqRel)
        {
            debug::event("notebook", "semua pekerjaan siap; membuka notebook Colab");
            self.events.open_notebook()
        }
        let mut changes = self.action.subscribe();
        while !pending.is_empty() {
            let mut next = Vec::new();
            for id in pending {
                match self.poll_one(id).await {
                    Ok(Some(RemotePhase::Completed)) => done.ok += 1,
                    Ok(Some(RemotePhase::Cancelled)) => done.cancelled += 1,
                    Ok(Some(_)) => done.fail += 1,
                    Ok(None) => next.push(id),
                    Err(error) => {
                        done.fail += 1;
                        if let Ok(r) = self.record(id) {
                            self.emit_record(&r, RemotePhase::Failed, 0., error.0, true)
                        }
                    }
                }
            }
            pending = next;
            if !pending.is_empty() {
                // Recheck a command that arrived while polling before sleeping.
                if pending.iter().any(|id| {
                    self.record(*id)
                        .is_ok_and(|r| r.desired_control == ControlAction::Cancel)
                }) {
                    continue;
                }
                tokio::select! {_=changes.changed()=>{},_=self.clock.tick()=>{}}
            }
        }
        done
    }
    async fn poll_one(self: &Arc<Self>, id: Uuid) -> Result<Option<RemotePhase>, ColabError> {
        let r = self.record(id)?;
        if r.desired_control == ControlAction::Cancel {
            // Unpublished jobs cannot run; published jobs receive durable Cancel.
            if r.state != RemotePhase::Uploading && r.completed_output.is_none() {
                self.send_control(id).await?;
            }
            let r = self.mutate(id, |r| r.state = RemotePhase::Cancelled)?;
            self.emit_record(
                &r,
                RemotePhase::Cancelled,
                0.,
                "PEMBATALAN DIMINTA — COLAB BERHENTI DI CHECKPOINT AMAN",
                false,
            );
            return Ok(Some(RemotePhase::Cancelled));
        }
        if r.completed_output.is_some() {
            return self.finish_output(id).await.map(Some);
        }
        let observed = self.observe_status(id).await?;
        let record = self.record(id)?;
        if record.completed_output.is_some() {
            self.finish_output(id).await.map(Some)
        } else if observed.is_some_and(terminal) {
            Ok(observed)
        } else {
            Ok(None)
        }
    }

    async fn observe_status(&self, id: Uuid) -> Result<Option<RemotePhase>, ColabError> {
        use super::remote_status::{parse_status_slot, reduce_status, select_latest_status};
        let r = self.record(id)?;
        self.send_control(id).await?;
        let (a, b) = match self.drive.read_status_slots(&r.remote_files).await {
            Ok(slots) => slots,
            Err(error) if self.auth.status().connected && retryable(&error) => {
                debug::event("poll", format!("Drive sementara gagal; menunggu pemulihan: {}", error.0));
                self.emit_record(
                    &r,
                    RemotePhase::RuntimeDisconnected,
                    0.,
                    "KONEKSI DRIVE TERPUTUS — MENCOBA KEMBALI",
                    false,
                );
                return Ok(None);
            }
            Err(error) => {
                debug::event("poll", format!("pembacaan status pekerjaan gagal: {}", error.0));
                return Err(error);
            }
        };
        let Some(status) = select_latest_status(
            parse_status_slot(&a),
            parse_status_slot(&b),
            r.last_revision,
        ) else {
            return Ok(None);
        };
        let reduced = reduce_status(&status, self.clock.now());
        if let Some(output) = status.output() {
            validate_result_contract(&r, output)?;
        }
        let record = self.mutate(id, |r| {
            r.last_revision = status.revision;
            r.state = reduced.phase;
            if let Some(output) = status.output() {
                r.completed_output = Some(output.clone());
                r.upload_session = None;
            }
        })?;
        self.emit_record(
            &record,
            reduced.phase,
            reduced.percent,
            &reduced.message,
            reduced.phase == RemotePhase::Failed,
        );
        Ok(Some(record.state))
    }
    async fn finish_output(self: &Arc<Self>, id: Uuid) -> Result<RemotePhase, ColabError> {
        let mut changes = self.action.subscribe();
        loop {
            let r = self.record(id)?;
            if r.state == RemotePhase::Completed {
                break;
            }
            if r.desired_control == ControlAction::Cancel {
                let r = self.mutate(id, |r| r.state = RemotePhase::Cancelled)?;
                self.emit_record(
                    &r,
                    r.state,
                    0.,
                    "UNDUHAN DIBATALKAN — HASIL DI DRIVE TETAP DISIMPAN",
                    false,
                );
                return Ok(RemotePhase::Cancelled);
            }
            if r.desired_control == ControlAction::Pause {
                self.emit_record(&r, RemotePhase::Paused, 0., "UNDUHAN DIJEDA", false);
                changes.changed().await.map_err(|_| storage())?;
                continue;
            }
            if !r.local_output.exists() {
                let r = self.mutate(id, |r| r.state = RemotePhase::Downloading)?;
                let this = self.clone();
                let file = r.local_input.clone();
                let progress: TransferProgress = Arc::new(move |n, total| {
                    this.emit(
                        id,
                        file.clone(),
                        RemotePhase::Downloading,
                        if total > 0 {
                            n as f64 * 100. / total as f64
                        } else {
                            0.
                        },
                        "MENGUNDUH HASIL",
                        false,
                    )
                });
                let part = super::output::part_path(&r.local_output);
                tokio::select! {
                    result=self.drive.download_output(&r,&part,progress)=>{result?;},
                    _=changes.changed()=>{continue;}
                }
            }
            if self.record(id)?.desired_control != ControlAction::Run {
                continue;
            }
            break;
        }
        let mut r = self.record(id)?;
        let expected = r.completed_output.clone().ok_or_else(storage)?;
        if r.state != RemotePhase::Completed {
            // If the previous process promoted the result but crashed before saving,
            // promote verifies the existing result rather than replacing it.
            r = self.mutate(id, |r| r.state = RemotePhase::Verifying)?;
            self.emit_record(&r, r.state, 100., "MEMVERIFIKASI HASIL", false);
        }
        self.probe.promote(&r, &expected)?;
        r = self.mutate(id, |r| r.state = RemotePhase::Completed)?;
        if !r.keep_drive_files {
            if self
                .drive
                .trash_job(&r.remote_files.folder_id)
                .await
                .is_err()
            {
                self.emit_record(
                    &r,
                    RemotePhase::Completed,
                    100.,
                    "SELESAI — PEMBERSIHAN DRIVE AKAN DICOBA LAGI",
                    false,
                );
                return Ok(RemotePhase::Completed);
            }
        }
        {
            let _lock = self.records.lock();
            self.registry.remove(id).map_err(|_| storage())?;
        }
        self.emit_record(&r, RemotePhase::Completed, 100., "SELESAI", false);
        Ok(RemotePhase::Completed)
    }
}

struct Lease(Arc<AtomicBool>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release)
    }
}
fn storage() -> ColabError {
    ColabError("Catatan pemulihan Colab tidak dapat disimpan")
}
fn terminal(p: RemotePhase) -> bool {
    matches!(
        p,
        RemotePhase::Completed | RemotePhase::Cancelled | RemotePhase::Failed
    )
}
fn retryable(e: &ColabError) -> bool {
    matches!(
        e.0,
        "Koneksi Google Drive terputus; mencoba kembali"
            | "Google Drive tidak merespons; periksa internet, proxy, atau firewall lalu coba lagi"
    )
}
fn validate_result_contract(
    r: &RemoteJobRecord,
    output: &CompletedOutput,
) -> Result<(), ColabError> {
    let expected_fps = r.manifest_json["interpolation"]
        .as_str()
        .filter(|v| *v != "off")
        .unwrap_or(&r.source_media.nominal_fps);
    let expected_audio =
        r.source_media.has_audio && !r.manifest_json["mute_audio"].as_bool().unwrap_or(true);
    let ratio = |v: &str| super::media::parse_ratio(v);
    let (a, b) = ratio(expected_fps)?;
    let (c, d) = ratio(&output.fps)?;
    let model = r.manifest_json["upscale"]["model"].as_str();
    let scale = r.manifest_json["upscale"]["scale"].as_u64();
    let scale = match (model, scale) {
        (Some("nanovsr-644k"), Some(4)) | (Some("realesrgan-x4plus"), Some(4)) => 4,
        (Some("realesrgan-x2plus"), Some(2)) => 2,
        _ => return Err(ColabError("Profil upscale Colab tidak valid")),
    };
    if r.source_media.width.checked_mul(scale) != Some(output.width)
        || r.source_media.height.checked_mul(scale) != Some(output.height)
        || u128::from(a) * u128::from(d) != u128::from(c) * u128::from(b)
        || output.has_audio != expected_audio
        || (output.duration_seconds - r.source_media.duration_seconds).abs() >= b as f64 / a as f64
    {
        return Err(ColabError(
            "Hasil Colab tidak sesuai ukuran, FPS, durasi, atau pilihan Mute",
        ));
    }
    Ok(())
}

#[cfg(all(test, windows))]
#[path = "coordinator_tests.rs"]
mod tests;
