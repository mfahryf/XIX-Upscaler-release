use super::*;
use crate::colab::test_support::TestDir;
use serde_json::json;
use std::fs;

fn record(dir: &Path) -> RemoteJobRecord {
    let id = Uuid::new_v4();
    let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    RemoteJobRecord {
        job_id: id,
        local_input: dir.join("private-input.mp4"),
        local_output: dir.join(format!("private-output-{id}.mp4")),
        input_sha256: hash.into(),
        source_media: SourceMedia {
            width: 16,
            height: 16,
            duration_seconds: 1.0,
            time_base: "1/24".into(),
            nominal_fps: "24/1".into(),
            has_audio: false,
            rotation: 0,
        },
        manifest_json: json!({"schema_version":2,"worker_version":"0.2.0","exchange_protocol":"drive-slots-v1","job_id":id,"created_at":"2026-09-03T00:00:00Z","input":{"name":"input.mp4","extension":"mp4","size_bytes":3,"sha256":hash},"source":{"width":16,"height":16,"duration_seconds":1.0,"time_base":"1/24","nominal_fps":"24/1","has_audio":false},"upscale":{"model":"nanovsr-644k","scale":4},"interpolation":"off","mute_audio":false,"output":{"format":"mp4","video_codec":"h264","suffix":"-colab"}}),
        workspace_ids: WorkspaceIds {
            root_id: "workspace".into(),
            marker_id: "marker".into(),
            jobs_id: "jobs".into(),
        },
        remote_files: RemoteFiles {
            folder_id: format!("folder-{id}"),
            input_id: format!("input-{id}"),
            manifest_id: format!("manifest-{id}"),
            status_a_id: format!("a-{id}"),
            status_b_id: format!("b-{id}"),
            control_id: format!("control-{id}"),
            output_id: format!("output-{id}"),
        },
        upload_session: Some(format!(
            "https://www.googleapis.com/upload/drive/v3/files/input-{id}?upload_id=private-session"
        )),
        state: RemotePhase::Uploading,
        last_revision: 0,
        control_revision: 0,
        desired_control: ControlAction::Run,
        keep_drive_files: false,
        completed_output: None,
    }
}

#[test]
fn recovery_roundtrip_encrypts_sensitive_values_and_preserves_missing_local_files() {
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    let record = record(&dir.0);
    registry.upsert(&record).unwrap();
    assert_eq!(registry.snapshot(), vec![record.clone()]);
    let encrypted = fs::read(&registry.path).unwrap();
    for secret in ["private-input", "private-output", "private-session"] {
        assert!(!encrypted
            .windows(secret.len())
            .any(|w| w == secret.as_bytes()));
        assert!(!format!("{record:?}").contains(secret));
    }
    let plain = crate::secure::dpapi::unprotect(&encrypted).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(value["version"], 1);
    for key in ["access_token", "refresh_token", "email"] {
        assert!(!String::from_utf8_lossy(&plain).contains(key));
    }
    assert_eq!(
        RemoteRegistry::load(&dir.0).unwrap().snapshot(),
        vec![record.clone()]
    );
    registry.remove(record.job_id).unwrap();
    assert!(RemoteRegistry::load(&dir.0).unwrap().snapshot().is_empty());
}

#[test]
fn failed_atomic_save_keeps_previous_disk_and_memory_records() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    let mut entry = record(&dir.0);
    registry.upsert(&entry).unwrap();
    let old = fs::read(&registry.path).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&registry.path)
        .unwrap();
    entry.last_revision = 5;
    assert!(registry.upsert(&entry).is_err());
    assert!(registry.remove(entry.job_id).is_err());
    assert_eq!(registry.snapshot()[0].last_revision, 0);
    drop(lock);
    assert_eq!(fs::read(&registry.path).unwrap(), old);
    assert_eq!(
        fs::read_dir(registry.path.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn immutable_job_identity_and_monotonic_revisions_survive_restart() {
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    let mut entry = record(&dir.0);
    entry.last_revision = 7;
    entry.control_revision = 4;
    entry.desired_control = ControlAction::Pause;
    registry.upsert(&entry).unwrap();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    for field in [
        "status-revision",
        "control-revision",
        "manifest",
        "output",
        "drive-id",
        "input",
        "source",
        "retention",
    ] {
        let mut changed = entry.clone();
        match field {
            "status-revision" => changed.last_revision = 6,
            "control-revision" => changed.control_revision = 3,
            "manifest" => changed.manifest_json["mute_audio"] = json!(true),
            "output" => changed.local_output = dir.0.join("different.mp4"),
            "drive-id" => changed.remote_files.output_id = "different-id".into(),
            "input" => changed.local_input = dir.0.join("different-input.mp4"),
            "source" => changed.source_media.rotation = 90,
            _ => changed.keep_drive_files = true,
        }
        assert!(
            registry.upsert(&changed).is_err(),
            "accepted change to {field}"
        );
    }
    entry.last_revision = 8;
    entry.control_revision = 5;
    entry.desired_control = ControlAction::Cancel;
    entry.upload_session = None;
    registry.upsert(&entry).unwrap();
    assert_eq!(
        RemoteRegistry::load(&dir.0).unwrap().snapshot(),
        vec![entry]
    );
}

#[test]
fn collisions_invalid_contracts_and_foreign_sessions_are_refused() {
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    let initial = record(&dir.0);
    registry.upsert(&initial).unwrap();
    for kind in [
        "collision",
        "duplicate-id",
        "relative",
        "source",
        "manifest-hash",
        "version",
        "protocol",
        "scope",
        "session-file",
        "session-origin",
        "duration",
        "missing-field",
    ] {
        let mut entry = record(&dir.0);
        match kind {
            "collision" => {
                entry.local_output =
                    PathBuf::from(initial.local_output.to_string_lossy().to_uppercase())
            }
            "duplicate-id" => entry.remote_files.output_id = entry.remote_files.input_id.clone(),
            "relative" => entry.local_input = "relative.mp4".into(),
            "source" => entry.source_media.width = 20,
            "manifest-hash" => entry.manifest_json["input"]["sha256"] = json!("bad"),
            "version" => entry.manifest_json["worker_version"] = json!("0.1.0"),
            "protocol" => entry.manifest_json["exchange_protocol"] = json!("old"),
            "scope" => entry.manifest_json["access_token"] = json!("private"),
            "session-file" => {
                entry.upload_session = Some(
                    "https://www.googleapis.com/upload/drive/v3/files/other?upload_id=x".into(),
                )
            }
            "session-origin" => {
                entry.upload_session =
                    Some("https://www.googleapis.com.attacker.invalid/upload?upload_id=x".into())
            }
            "duration" => {
                entry.source_media.duration_seconds = 60.001;
                entry.manifest_json["source"]["duration_seconds"] = json!(60.001);
            }
            _ => {
                entry
                    .manifest_json
                    .as_object_mut()
                    .unwrap()
                    .remove("mute_audio");
            }
        }
        assert!(registry.upsert(&entry).is_err(), "accepted {kind}");
    }
    assert_eq!(registry.snapshot(), vec![initial]);
}

#[test]
fn unsupported_or_corrupt_registry_is_preserved_not_silently_reset() {
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    fs::create_dir_all(registry.path.parent().unwrap()).unwrap();
    for bytes in [
        b"private-corrupt-data".to_vec(),
        crate::secure::dpapi::protect(br#"{"version":2,"jobs":[]}"#).unwrap(),
        crate::secure::dpapi::protect(br#"{"version":1,"jobs":[],"access_token":"secret"}"#)
            .unwrap(),
    ] {
        fs::write(&registry.path, &bytes).unwrap();
        assert!(RemoteRegistry::load(&dir.0).is_err());
        assert_eq!(fs::read(&registry.path).unwrap(), bytes);
    }
}

#[test]
fn completed_metadata_and_pending_control_are_durable_and_cannot_be_rewritten() {
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    let mut entry = record(&dir.0);
    entry.completed_output = Some(CompletedOutput {
        name: "output.mp4".into(),
        size_bytes: 80,
        sha256: "a".repeat(64),
        width: 64,
        height: 64,
        duration_seconds: 1.0,
        fps: "24/1".into(),
        has_audio: false,
    });
    entry.state = RemotePhase::Downloading;
    entry.last_revision = 9;
    entry.upload_session = None;
    entry.desired_control = ControlAction::Cancel;
    entry.control_revision = 3;
    registry.upsert(&entry).unwrap();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    assert_eq!(registry.snapshot(), vec![entry.clone()]);
    let mut changed = entry.clone();
    changed.completed_output.as_mut().unwrap().sha256 = "b".repeat(64);
    assert!(registry.upsert(&changed).is_err());
    let mut changed = entry.clone();
    changed.desired_control = ControlAction::Run;
    assert!(registry.upsert(&changed).is_err());
    entry.state = RemotePhase::Completed;
    registry.upsert(&entry).unwrap();
}

#[test]
fn recovery_preserves_any_normalized_rotation_accepted_by_preflight() {
    let dir = TestDir::new();
    let registry = RemoteRegistry::load(&dir.0).unwrap();
    let mut entry = record(&dir.0);
    entry.source_media.rotation = 13;
    registry.upsert(&entry).unwrap();
    assert_eq!(
        RemoteRegistry::load(&dir.0).unwrap().snapshot()[0]
            .source_media
            .rotation,
        13
    );
}
