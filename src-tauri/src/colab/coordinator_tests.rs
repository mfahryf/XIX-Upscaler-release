use super::*;
use crate::colab::{media::SourceMedia, test_support::TestDir};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::atomic::AtomicUsize;

const HASH: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
#[derive(Default)]
struct Events {
    rows: Mutex<Vec<ColabEvent>>,
    done: Mutex<Vec<ColabDone>>,
    opens: AtomicUsize,
}
impl ColabEvents for Events {
    fn event(&self, e: ColabEvent) {
        self.rows.lock().push(e)
    }
    fn done(&self, d: ColabDone) {
        self.done.lock().push(d)
    }
    fn open_notebook(&self) {
        self.opens.fetch_add(1, Ordering::SeqCst);
    }
}
struct Auth {
    connected: AtomicBool,
    log: Arc<Mutex<Vec<String>>>,
}
impl ColabAuth for Auth {
    fn status(&self) -> AuthStatus {
        AuthStatus {
            connected: self.connected.load(Ordering::SeqCst),
            masked_account: None,
        }
    }
    fn ensure_connected(&self, interactive: bool) -> ColabFuture<'_, ()> {
        Box::pin(async move {
            self.log.lock().push(format!("auth:{interactive}"));
            if !interactive && !self.status().connected {
                return Err(ColabError("Hubungkan ulang Google Drive"));
            }
            self.connected.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn disconnect(&self) -> ColabFuture<'_, ()> {
        Box::pin(async move {
            self.connected.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Clock {
    ticks: AtomicUsize,
    permits: tokio::sync::Semaphore,
}
impl ColabClock for Clock {
    fn now(&self) -> DateTime<Utc> {
        "2026-09-03T00:00:00Z".parse().unwrap()
    }
    fn tick(&self) -> ColabFuture<'_, ()> {
        Box::pin(async move {
            self.ticks.fetch_add(1, Ordering::SeqCst);
            self.permits.acquire().await.unwrap().forget();
            Ok(())
        })
    }
}
struct Probe {
    log: Arc<Mutex<Vec<String>>>,
    reject_output: AtomicBool,
}
impl ColabProbe for Probe {
    fn prepare(
        &self,
        input: &Path,
        out: &Path,
        _options: &EngineOptions,
        _now: DateTime<Utc>,
        reserved: &mut HashSet<PathBuf>,
    ) -> Result<PreparedJob, ColabError> {
        let name = input.file_name().unwrap().to_str().unwrap();
        self.log.lock().push(format!("prepare:{name}"));
        if name == "invalid.mp4" {
            return Err(ColabError("Video melebihi batas 1 menit"));
        }
        let id = Uuid::new_v4();
        let output = out.join(format!("{name}-colab.mp4"));
        if !reserved.insert(output.clone()) {
            return Err(ColabError("Nama hasil bertabrakan"));
        }
        Ok(PreparedJob {
            job_id: id,
            input_path: input.into(),
            output_path: output,
            input_sha256: HASH.into(),
            source_media: SourceMedia {
                width: 16,
                height: 16,
                duration_seconds: 1.0,
                time_base: "1/24".into(),
                nominal_fps: "24/1".into(),
                has_audio: false,
                rotation: 0,
            },
            manifest_json: json!({"schema_version":2,"worker_version":"0.2.0","exchange_protocol":"drive-slots-v1","job_id":id,"created_at":"2026-09-03T00:00:00Z","input":{"name":"input.mp4","extension":"mp4","size_bytes":3,"sha256":HASH},"source":{"width":16,"height":16,"duration_seconds":1.0,"time_base":"1/24","nominal_fps":"24/1","has_audio":false},"upscale":{"model":"nanovsr-644k","scale":4},"interpolation":"off","mute_audio":false,"output":{"format":"mp4","video_codec":"h264","suffix":"-colab"}}),
        })
    }
    fn verify_input(&self, r: &RemoteJobRecord) -> Result<(), ColabError> {
        self.log.lock().push(format!("verify-input:{}", r.job_id));
        Ok(())
    }
    fn promote(&self, r: &RemoteJobRecord, _e: &CompletedOutput) -> Result<(), ColabError> {
        self.log.lock().push(format!("verify-output:{}", r.job_id));
        if self.reject_output.load(Ordering::SeqCst) {
            return Err(ColabError("Hasil video tidak cocok"));
        }
        std::fs::write(&r.local_output, b"verified").unwrap();
        Ok(())
    }
}
struct Drive {
    log: Arc<Mutex<Vec<String>>>,
    registry: Arc<RemoteRegistry>,
    fail_upload: AtomicBool,
    fail_one_input: Mutex<Option<String>>,
    wrong_workspace: AtomicBool,
    completed: AtomicBool,
    controls: Mutex<Vec<(u64, ControlAction)>>,
    hold_download: AtomicBool,
    download_started: AtomicUsize,
    published: AtomicBool,
    hold_upload: AtomicBool,
    hold_one_input: Mutex<Option<String>>,
    upload_started: AtomicUsize,
    status_state: Mutex<String>,
}
impl ColabDrive for Drive {
    fn account_identity(&self) -> ColabFuture<'_, DriveUser> {
        Box::pin(async move {
            self.log.lock().push("account".into());
            Ok(DriveUser {
                display_name: None,
                email_address: Some("fahry@example.com".into()),
                permission_id: None,
            })
        })
    }
    fn ensure_workspace(&self) -> ColabFuture<'_, WorkspaceIds> {
        Box::pin(async move {
            self.log.lock().push("workspace".into());
            Ok(WorkspaceIds {
                root_id: "workspace-id".into(),
                marker_id: "marker".into(),
                jobs_id: "jobs".into(),
            })
        })
    }
    fn validate_workspace<'a>(&'a self, _: &'a WorkspaceIds) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            self.log.lock().push("validate-workspace".into());
            if self.wrong_workspace.load(Ordering::SeqCst) {
                Err(ColabError("Hubungkan akun Google Drive sebelumnya"))
            } else {
                Ok(())
            }
        })
    }
    fn reserve_job_files(&self) -> ColabFuture<'_, RemoteFiles> {
        Box::pin(async move {
            let id = Uuid::new_v4();
            self.log.lock().push("reserve".into());
            Ok(RemoteFiles {
                folder_id: format!("folder-{id}"),
                input_id: format!("input-{id}"),
                manifest_id: format!("manifest-{id}"),
                status_a_id: format!("a-{id}"),
                status_b_id: format!("b-{id}"),
                control_id: format!("control-{id}"),
                output_id: format!("output-{id}"),
            })
        })
    }
    fn create_job_files<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            assert!(
                self.registry
                    .snapshot()
                    .iter()
                    .any(|s| s.job_id == r.job_id),
                "reservation must be durable before any remote create"
            );
            self.log.lock().push(format!("create:{}", r.job_id));
            Ok(())
        })
    }
    fn manifest_published<'a>(&'a self, _: &'a RemoteJobRecord) -> ColabFuture<'a, bool> {
        Box::pin(async { Ok(self.published.load(Ordering::SeqCst)) })
    }
    fn upload_input<'a>(
        &'a self,
        r: &'a RemoteJobRecord,
        sink: SessionSink,
        p: TransferProgress,
    ) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            self.log.lock().push(format!("upload:{}", r.job_id));
            sink(Some(format!(
                "https://www.googleapis.com/upload/drive/v3/files/{}?upload_id=resume",
                r.remote_files.input_id
            )))
            .unwrap();
            p(1, 3);
            self.upload_started.fetch_add(1, Ordering::SeqCst);
            if self.hold_upload.load(Ordering::SeqCst)
                || self.hold_one_input.lock().as_deref()
                    == r.local_input.file_name().and_then(|v| v.to_str())
            {
                std::future::pending::<()>().await;
            }
            if self.fail_upload.load(Ordering::SeqCst)
                || self.fail_one_input.lock().as_deref()
                    == r.local_input.file_name().and_then(|v| v.to_str())
            {
                return Err(ColabError("Koneksi terputus"));
            }
            sink(None).unwrap();
            p(3, 3);
            Ok(())
        })
    }
    fn publish_manifest<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            assert!(self
                .registry
                .snapshot()
                .iter()
                .find(|j| j.job_id == r.job_id)
                .unwrap()
                .upload_session
                .is_none());
            self.log.lock().push(format!("publish:{}", r.job_id));
            Ok(())
        })
    }
    fn read_status_slots<'a>(&'a self, _: &'a RemoteFiles) -> ColabFuture<'a, (Vec<u8>, Vec<u8>)> {
        Box::pin(async move {
            let complete = self.completed.load(Ordering::SeqCst);
            let state = if complete {
                "completed".to_owned()
            } else {
                self.status_state.lock().clone()
            };
            let value = json!({"schema_version":1,"revision":if complete{3}else{1},"state":state,"session_id":"11111111-1111-4111-8111-111111111111","heartbeat_at":"2026-09-03T00:00:00Z","progress":{"segment_index":0,"segment_count":1,"percent":if complete{100}else{50},"last_checkpoint":null},"message":"safe","metadata":if complete {json!({"output":{"name":"output.mp4","size_bytes":3,"sha256":HASH,"width":64,"height":64,"duration_seconds":1.0,"fps":"24/1","has_audio":false}})}else{json!({})}});
            Ok((serde_json::to_vec(&value).unwrap(), b"partial".to_vec()))
        })
    }
    fn write_control<'a>(&'a self, r: &'a RemoteJobRecord) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            let persisted = self
                .registry
                .snapshot()
                .into_iter()
                .find(|j| j.job_id == r.job_id)
                .unwrap();
            assert_eq!(
                (persisted.control_revision, persisted.desired_control),
                (r.control_revision, r.desired_control)
            );
            self.controls
                .lock()
                .push((r.control_revision, r.desired_control));
            Ok(())
        })
    }
    fn download_output<'a>(
        &'a self,
        r: &'a RemoteJobRecord,
        part: &'a Path,
        p: TransferProgress,
    ) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            assert!(self
                .registry
                .snapshot()
                .iter()
                .find(|j| j.job_id == r.job_id)
                .unwrap()
                .completed_output
                .is_some());
            self.log.lock().push(format!("download:{}", r.job_id));
            std::fs::write(part, b"abc").unwrap();
            p(3, 3);
            self.download_started.fetch_add(1, Ordering::SeqCst);
            if self.hold_download.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
    fn trash_job<'a>(&'a self, folder: &'a str) -> ColabFuture<'a, ()> {
        Box::pin(async move {
            let r = self
                .registry
                .snapshot()
                .into_iter()
                .find(|r| r.remote_files.folder_id == folder)
                .unwrap();
            assert_eq!(r.state, RemotePhase::Completed);
            assert!(r.local_output.exists());
            self.log.lock().push(format!("trash:{}", r.job_id));
            Ok(())
        })
    }
}
struct Harness {
    root: TestDir,
    coordinator: Arc<ColabCoordinator>,
    registry: Arc<RemoteRegistry>,
    events: Arc<Events>,
    drive: Arc<Drive>,
    probe: Arc<Probe>,
    clock: Arc<Clock>,
    auth: Arc<Auth>,
    log: Arc<Mutex<Vec<String>>>,
}
impl Harness {
    fn new() -> Self {
        let root = TestDir::new();
        let registry = Arc::new(RemoteRegistry::load(&root.0).unwrap());
        let log = Arc::new(Mutex::new(vec![]));
        let auth = Arc::new(Auth {
            connected: AtomicBool::new(true),
            log: log.clone(),
        });
        let events = Arc::new(Events::default());
        let drive = Arc::new(Drive {
            log: log.clone(),
            registry: registry.clone(),
            fail_upload: AtomicBool::new(false),
            fail_one_input: Mutex::new(None),
            wrong_workspace: AtomicBool::new(false),
            completed: AtomicBool::new(true),
            controls: Default::default(),
            hold_download: AtomicBool::new(false),
            download_started: AtomicUsize::new(0),
            published: AtomicBool::new(false),
            hold_upload: AtomicBool::new(false),
            hold_one_input: Mutex::new(None),
            upload_started: AtomicUsize::new(0),
            status_state: Mutex::new("processing".into()),
        });
        let probe = Arc::new(Probe {
            log: log.clone(),
            reject_output: AtomicBool::new(false),
        });
        let clock = Arc::new(Clock {
            ticks: AtomicUsize::new(0),
            permits: tokio::sync::Semaphore::new(0),
        });
        let coordinator = Arc::new(ColabCoordinator::new(
            auth.clone(),
            drive.clone(),
            probe.clone(),
            registry.clone(),
            clock.clone(),
            events.clone(),
        ));
        Self {
            root,
            coordinator,
            registry,
            events,
            drive,
            probe,
            clock,
            auth,
            log,
        }
    }
    fn request(&self, names: &[&str]) -> StartColabRequest {
        StartColabRequest {
            files: names.iter().map(|n| self.root.0.join(n)).collect(),
            output_dir: self.root.0.clone(),
            options: EngineOptions::from([("engine".into(), json!("video-colab"))]),
        }
    }
    async fn done(&self) {
        until(|| !self.events.done.lock().is_empty()).await;
        until(|| !self.coordinator.status().busy).await;
    }
}
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("coordinator did not reach expected state");
}

#[tokio::test]
async fn entire_batch_is_preflighted_before_any_login_or_remote_write() {
    let h = Harness::new();
    let error = h
        .coordinator
        .start_batch(h.request(&["ok.mp4", "invalid.mp4"]))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("1 menit"));
    assert_eq!(*h.log.lock(), ["prepare:ok.mp4", "prepare:invalid.mp4"]);
    assert!(!h.coordinator.status().busy);
}
#[tokio::test]
async fn uploads_publish_in_playlist_order_and_complete_only_after_verified_local_output() {
    let h = Harness::new();
    h.coordinator
        .start_batch(h.request(&["a.mp4", "b.mp4"]))
        .await
        .unwrap();
    h.done().await;
    assert_eq!(
        *h.events.done.lock(),
        [ColabDone {
            ok: 2,
            fail: 0,
            cancelled: 0,
            total: 2
        }],
        "events: {:?}; log: {:?}",
        h.events.rows.lock(),
        h.log.lock()
    );
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 1);
    assert!(h.registry.snapshot().is_empty());
    let log = h.log.lock();
    assert_eq!(
        &log[..4],
        ["prepare:a.mp4", "prepare:b.mp4", "auth:true", "account"]
    );
    for id in h
        .events
        .rows
        .lock()
        .iter()
        .filter(|e| e.phase == RemotePhase::Completed)
        .map(|e| e.job_id)
    {
        let pos = |s: &str| log.iter().position(|v| v == &format!("{s}:{id}")).unwrap();
        assert!(pos("upload") < pos("publish"));
        assert!(pos("publish") < pos("download"));
        assert!(pos("verify-output") < pos("trash"));
    }
}
#[tokio::test]
async fn upload_failure_keeps_recovery_session_and_never_publishes_manifest() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    assert_eq!(h.events.done.lock()[0].fail, 1);
    assert!(h.registry.snapshot()[0].upload_session.is_some());
    assert_eq!(h.registry.snapshot()[0].state, RemotePhase::Uploading);
    assert!(!h.log.lock().iter().any(|s| s.starts_with("publish:")));
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn verification_failure_preserves_part_and_never_trashes_job() {
    let h = Harness::new();
    h.probe.reject_output.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    let r = h.registry.snapshot().pop().unwrap();
    assert!(super::super::output::part_path(&r.local_output).exists());
    assert!(!r.local_output.exists());
    assert!(r.completed_output.is_some());
    assert!(!h.log.lock().iter().any(|s| s.starts_with("trash:")));
    assert_eq!(h.events.done.lock()[0].ok, 0);
}
#[tokio::test]
async fn retention_skips_trash_but_still_finishes_verified_result() {
    let h = Harness::new();
    let mut req = h.request(&["a.mp4"]);
    req.options.insert("keep_drive_files".into(), json!(true));
    h.coordinator.start_batch(req).await.unwrap();
    h.done().await;
    assert_eq!(h.events.done.lock()[0].ok, 1);
    assert!(h.registry.snapshot().is_empty());
    assert!(!h.log.lock().iter().any(|s| s.starts_with("trash:")));
}
#[tokio::test]
async fn pause_resume_stop_are_durable_monotonic_and_wake_poll_without_another_tick() {
    let h = Harness::new();
    h.drive.completed.store(false, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    until(|| h.clock.ticks.load(Ordering::SeqCst) > 0).await;
    h.coordinator.set_paused(true).await.unwrap();
    h.coordinator.set_paused(true).await.unwrap();
    h.coordinator.set_paused(false).await.unwrap();
    h.coordinator.cancel().await.unwrap();
    h.done().await;
    let controls = h.drive.controls.lock();
    assert!(controls.contains(&(1, ControlAction::Pause)));
    assert!(controls.contains(&(2, ControlAction::Run)));
    assert!(controls.contains(&(3, ControlAction::Cancel)));
    assert!(controls.windows(2).all(|w| w[1].0 >= w[0].0));
    assert_eq!(h.events.done.lock()[0].cancelled, 1);
}
#[tokio::test]
async fn repeated_recovery_never_duplicates_upload_and_does_not_open_login() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    h.events.done.lock().clear();
    h.drive.fail_upload.store(false, Ordering::SeqCst);
    h.drive.completed.store(false, Ordering::SeqCst);
    h.log.lock().clear();
    h.coordinator.resume_pending().await.unwrap();
    h.coordinator.resume_pending().await.unwrap();
    until(|| h.clock.ticks.load(Ordering::SeqCst) > 0).await;
    assert_eq!(
        h.log
            .lock()
            .iter()
            .filter(|s| s.starts_with("upload:"))
            .count(),
        1
    );
    assert!(h.log.lock().contains(&"auth:false".into()));
    assert!(!h.log.lock().contains(&"auth:true".into()));
    h.coordinator.cancel().await.unwrap();
    h.done().await;
}
#[tokio::test]
async fn recovery_in_a_different_account_does_not_create_or_upload() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    h.events.done.lock().clear();
    h.log.lock().clear();
    h.drive.wrong_workspace.store(true, Ordering::SeqCst);
    h.coordinator.resume_pending().await.unwrap();
    h.done().await;
    assert!(!h
        .log
        .lock()
        .iter()
        .any(|s| s == "workspace" || s.starts_with("create:") || s.starts_with("upload:")));
    assert_eq!(h.registry.snapshot().len(), 1);
}
#[tokio::test]
async fn disconnected_startup_only_restores_snapshot_without_browser() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    h.auth.connected.store(false, Ordering::SeqCst);
    h.log.lock().clear();
    let snapshots = h.coordinator.resume_pending().await.unwrap();
    assert_eq!(snapshots.len(), 1);
    assert!(!h.coordinator.status().busy);
    assert!(h.log.lock().is_empty());
}

#[tokio::test]
async fn stop_interrupts_local_download_without_promoting_or_cleaning_drive() {
    let h = Harness::new();
    h.drive.hold_download.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    until(|| h.drive.download_started.load(Ordering::SeqCst) > 0).await;
    h.coordinator.cancel().await.unwrap();
    h.done().await;
    assert_eq!(h.events.done.lock()[0].cancelled, 1);
    let r = h.registry.snapshot().pop().unwrap();
    assert_eq!(r.desired_control, ControlAction::Cancel);
    assert!(super::super::output::part_path(&r.local_output).exists());
    assert!(!r.local_output.exists());
    assert!(!h.log.lock().iter().any(|s| s.starts_with("trash:")));
}
#[tokio::test]
async fn failed_terminal_jobs_are_not_offered_as_pending_uploads_on_restart() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    let mut r = h.registry.snapshot().pop().unwrap();
    r.state = RemotePhase::Failed;
    h.registry.upsert(&r).unwrap();
    h.auth.connected.store(false, Ordering::SeqCst);
    assert!(h.coordinator.resume_pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn completed_history_with_missing_output_never_reenters_recovery_or_blocks_new_input() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator.start_batch(h.request(&["a.mp4"])).await.unwrap();
    h.done().await;
    let mut record = h.registry.snapshot().pop().unwrap();
    record.state = RemotePhase::Completed;
    record.upload_session = None;
    record.last_revision = 1;
    record.completed_output = Some(CompletedOutput {
        name: "output.mp4".into(), size_bytes: 3, sha256: HASH.into(),
        width: 64, height: 64, duration_seconds: 1.0, fps: "24/1".into(), has_audio: false,
    });
    h.registry.upsert(&record).unwrap();
    assert!(!record.local_output.exists());
    h.events.rows.lock().clear();
    h.events.done.lock().clear();
    h.log.lock().clear();

    assert!(h.coordinator.resume_pending().await.unwrap().is_empty());
    assert!(!h.coordinator.status().busy);
    assert!(h.events.rows.lock().is_empty());
    assert!(h.events.done.lock().is_empty());
    assert!(h.log.lock().is_empty(), "completed history must not verify, download or trash");
    assert!(h.registry.snapshot() == vec![record.clone()], "history must be preserved");

    // The same source can start a new job; actual name-collision handling is
    // covered by manifest::tests (this harness has a deliberately simple probe).
    let mut request = h.request(&["a.mp4"]);
    request.output_dir = h.root.0.join("new-results");
    std::fs::create_dir(&request.output_dir).unwrap();
    h.drive.fail_upload.store(false, Ordering::SeqCst);
    h.coordinator.start_batch(request).await.unwrap();
    h.done().await;
    assert_eq!(h.events.done.lock()[0].ok, 1);
    assert!(h.registry.snapshot() == vec![record]);
}
#[tokio::test]
async fn drive_identity_is_masked_and_account_changes_are_blocked_during_remote_work() {
    let h = Harness::new();
    let status = h.coordinator.connect().await.unwrap();
    assert_eq!(status.masked_account.as_deref(), Some("fa***@example.com"));
    assert!(!status.busy);
    h.drive.completed.store(false, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    until(|| h.clock.ticks.load(Ordering::SeqCst) > 0).await;
    assert!(h.coordinator.disconnect().await.is_err());
    assert!(h.coordinator.connect().await.is_err());
    h.coordinator.cancel().await.unwrap();
    h.done().await;
    h.coordinator.disconnect().await.unwrap();
    assert!(!h.coordinator.status().connected);
    assert!(h.coordinator.status().masked_account.is_none());
}

#[tokio::test]
async fn refreshing_drive_status_rechecks_saved_auth_and_drive_identity() {
    let h = Harness::new();
    let status = h.coordinator.refresh_status().await.unwrap();
    assert_eq!(status.masked_account.as_deref(), Some("fa***@example.com"));
    assert_eq!(h.log.lock().as_slice(), ["auth:false", "account"]);
}

#[tokio::test]
async fn stop_after_manifest_publish_crash_reaches_the_worker_without_reuploading() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    // Disk phase is still uploading but the immutable remote manifest survived.
    h.drive.published.store(true, Ordering::SeqCst);
    h.events.done.lock().clear();
    h.log.lock().clear();
    h.coordinator.cancel().await.unwrap();
    h.drive.controls.lock().clear();
    h.coordinator.resume_pending().await.unwrap();
    h.done().await;
    assert!(h
        .drive
        .controls
        .lock()
        .contains(&(1, ControlAction::Cancel)));
    assert!(!h
        .log
        .lock()
        .iter()
        .any(|s| s.starts_with("upload:") || s.starts_with("create:")));
}
#[tokio::test]
async fn polling_does_not_rewrite_unchanged_run_control_each_tick() {
    let h = Harness::new();
    h.drive.completed.store(false, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    until(|| h.clock.ticks.load(Ordering::SeqCst) > 0).await;
    let writes = h.drive.controls.lock().len();
    h.clock.permits.add_permits(1);
    until(|| h.clock.ticks.load(Ordering::SeqCst) > 1).await;
    assert_eq!(h.drive.controls.lock().len(), writes);
    h.coordinator.cancel().await.unwrap();
    h.done().await;
}
#[tokio::test]
async fn pause_during_download_retains_bytes_then_resumes_when_requested() {
    let h = Harness::new();
    h.drive.hold_download.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    until(|| h.drive.download_started.load(Ordering::SeqCst) > 0).await;
    h.coordinator.set_paused(true).await.unwrap();
    until(|| {
        h.events
            .rows
            .lock()
            .last()
            .is_some_and(|e| e.phase == RemotePhase::Paused)
    })
    .await;
    assert!(h.events.done.lock().is_empty());
    h.drive.hold_download.store(false, Ordering::SeqCst);
    h.coordinator.set_paused(false).await.unwrap();
    h.done().await;
    assert_eq!(h.events.done.lock()[0].ok, 1);
}

#[tokio::test]
async fn pause_before_all_uploads_finish_marks_waiting_upload_rows_safe_to_resume() {
    let h = Harness::new();
    h.drive.hold_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4", "b.mp4"]))
        .await
        .unwrap();
    until(|| h.drive.upload_started.load(Ordering::SeqCst) > 0).await;
    h.coordinator.set_paused(true).await.unwrap();
    until(|| {
        h.coordinator
            .snapshot
            .lock()
            .iter()
            .all(|e| e.phase == RemotePhase::Paused)
    })
    .await;
    h.drive.hold_upload.store(false, Ordering::SeqCst);
    h.coordinator.set_paused(false).await.unwrap();
    h.done().await;
    assert_eq!(h.events.done.lock()[0].ok, 2);
}

#[tokio::test]
async fn pause_during_a_later_upload_observes_an_already_published_worker_and_resume_reopens_colab()
{
    let h = Harness::new();
    h.drive.completed.store(false, Ordering::SeqCst);
    *h.drive.hold_one_input.lock() = Some("b.mp4".into());
    h.coordinator
        .start_batch(h.request(&["a.mp4", "b.mp4"]))
        .await
        .unwrap();
    until(|| h.drive.upload_started.load(Ordering::SeqCst) >= 2).await;
    *h.drive.status_state.lock() = "paused".into();
    h.coordinator.set_paused(true).await.unwrap();
    until(|| {
        let rows = h.coordinator.snapshot.lock();
        rows.len() == 2 && rows.iter().all(|row| row.phase == RemotePhase::Paused)
    })
    .await;
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 0);
    h.drive.completed.store(true, Ordering::SeqCst);
    *h.drive.hold_one_input.lock() = None;
    h.coordinator.set_paused(false).await.unwrap();
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 1);
    h.done().await;
    assert_eq!(h.events.done.lock()[0].ok, 2);
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn immediate_stop_before_background_login_does_not_open_google() {
    let h = Harness::new();
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.coordinator.cancel().await.unwrap();
    h.done().await;
    assert!(!h.log.lock().iter().any(|s| s.starts_with("auth:")));
    assert_eq!(h.events.done.lock()[0].cancelled, 1);
    assert!(h.registry.snapshot().is_empty());
}
#[tokio::test]
async fn confirmed_cancellations_are_not_replayed_or_included_in_later_batch_totals() {
    let h = Harness::new();
    h.drive.completed.store(false, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    until(|| h.clock.ticks.load(Ordering::SeqCst) > 0).await;
    h.coordinator.cancel().await.unwrap();
    h.done().await;
    h.events.done.lock().clear();
    h.log.lock().clear();
    assert!(h.coordinator.resume_pending().await.unwrap().is_empty());
    assert!(!h.coordinator.status().busy);
    assert!(h.log.lock().is_empty());
}

#[tokio::test]
async fn an_unobserved_published_job_is_not_claimed_paused_before_worker_confirmation() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    let mut record = h.registry.snapshot().pop().unwrap();
    record.upload_session = None;
    record.state = RemotePhase::WaitingForColab;
    h.registry.upsert(&record).unwrap();
    h.events.rows.lock().clear();
    h.coordinator.set_paused(true).await.unwrap();
    assert!(!h
        .events
        .rows
        .lock()
        .iter()
        .any(|e| e.phase == RemotePhase::Paused));
    assert!(h.drive.controls.lock().contains(&(1, ControlAction::Pause)));
}

#[tokio::test]
async fn cancelling_pause_before_worker_confirmation_still_reopens_colab_for_run_all() {
    let h = Harness::new();
    h.drive.fail_upload.store(true, Ordering::SeqCst);
    h.coordinator
        .start_batch(h.request(&["a.mp4"]))
        .await
        .unwrap();
    h.done().await;
    let mut record = h.registry.snapshot().pop().unwrap();
    record.upload_session = None;
    record.state = RemotePhase::WaitingForColab;
    h.registry.upsert(&record).unwrap();
    h.coordinator.set_paused(true).await.unwrap();
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 0);
    h.coordinator.set_paused(false).await.unwrap();
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_partially_failed_upload_batch_does_not_auto_open_the_notebook() {
    let h = Harness::new();
    *h.drive.fail_one_input.lock() = Some("a.mp4".into());
    h.coordinator
        .start_batch(h.request(&["a.mp4", "b.mp4"]))
        .await
        .unwrap();
    h.done().await;
    assert_eq!(
        h.events.done.lock()[0],
        ColabDone {
            ok: 1,
            fail: 1,
            cancelled: 0,
            total: 2
        }
    );
    assert_eq!(h.events.opens.load(Ordering::SeqCst), 0);
    assert_eq!(h.registry.snapshot().len(), 1);
}

#[test]
fn result_duration_must_differ_by_less_than_one_target_frame() {
    let h = Harness::new();
    let mut reserved = HashSet::new();
    let prepared = h
        .probe
        .prepare(
            &h.root.0.join("a.mp4"),
            &h.root.0,
            &EngineOptions::from([("engine".into(), json!("video-colab"))]),
            h.clock.now(),
            &mut reserved,
        )
        .unwrap();
    let mut record = RemoteJobRecord {
        job_id: prepared.job_id,
        local_input: prepared.input_path,
        local_output: prepared.output_path,
        input_sha256: prepared.input_sha256,
        source_media: prepared.source_media,
        manifest_json: prepared.manifest_json,
        workspace_ids: WorkspaceIds {
            root_id: "root".into(),
            marker_id: "marker".into(),
            jobs_id: "jobs".into(),
        },
        remote_files: RemoteFiles {
            folder_id: "folder".into(),
            input_id: "input".into(),
            manifest_id: "manifest".into(),
            status_a_id: "a".into(),
            status_b_id: "b".into(),
            control_id: "control".into(),
            output_id: "output".into(),
        },
        upload_session: None,
        state: RemotePhase::Processing,
        last_revision: 0,
        control_revision: 0,
        desired_control: ControlAction::Run,
        keep_drive_files: false,
        completed_output: None,
    };
    let mut output = CompletedOutput {
        name: "output.mp4".into(),
        size_bytes: 3,
        sha256: HASH.into(),
        width: 64,
        height: 64,
        duration_seconds: 1.0,
        fps: "24/1".into(),
        has_audio: false,
    };
    for (fps, accepted_delta, rejected_delta) in [
        ("24/1", 0.040, 0.042),
        ("60/1", 0.016, 0.017),
        ("24000/1001", 0.041, 0.042),
        ("60000/1001", 0.016, 0.017),
    ] {
        record.manifest_json["interpolation"] = json!(fps);
        output.fps = fps.into();
        output.duration_seconds = 1.0 + accepted_delta;
        assert!(
            validate_result_contract(&record, &output).is_ok(),
            "accept {fps}"
        );
        output.duration_seconds = 1.0 + rejected_delta;
        assert!(
            validate_result_contract(&record, &output).is_err(),
            "reject {fps}"
        );
    }
    record.manifest_json["upscale"] = json!({"model": "realesrgan-x2plus", "scale": 2});
    output.width = 32;
    output.height = 32;
    output.fps = "24/1".into();
    output.duration_seconds = 1.0;
    record.manifest_json["interpolation"] = json!("off");
    assert!(validate_result_contract(&record, &output).is_ok());
}
