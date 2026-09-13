//! Strict reader for the two desktop-owned Drive status slots.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerState {
    Queued,
    Claimed,
    Preparing,
    Processing,
    Paused,
    Failed,
    Cancelled,
    Completed,
}

impl WorkerState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Claimed | Self::Preparing | Self::Processing)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemotePhase {
    Validating,
    Authenticating,
    Uploading,
    WaitingForColab,
    Preparing,
    Processing,
    Paused,
    RuntimeDisconnected,
    Downloading,
    Verifying,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedOutput {
    pub name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub duration_seconds: f64,
    pub fps: String,
    pub has_audio: bool,
}

impl CompletedOutput {
    pub(super) fn validate(&self) -> Result<(), StatusError> {
        if self.name != "output.mp4"
            || self.size_bytes == 0
            || self.width == 0
            || self.height == 0
            || !self.duration_seconds.is_finite()
            || self.duration_seconds <= 0.0
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || super::media::parse_ratio(&self.fps).is_err()
        {
            return Err(StatusError);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteProgress {
    pub segment_index: u64,
    pub segment_count: u64,
    pub percent: f64,
    pub last_checkpoint: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    output: Option<CompletedOutput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotDocument {
    schema_version: u64,
    revision: u64,
    state: WorkerState,
    session_id: Option<String>,
    heartbeat_at: Option<String>,
    progress: RemoteProgress,
    message: String,
    metadata: Metadata,
}

#[derive(Clone, Debug)]
pub struct RemoteStatus {
    pub revision: u64,
    pub state: WorkerState,
    pub session_id: Option<Uuid>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub progress: RemoteProgress,
    pub message: String,
    metadata: Metadata,
}

impl RemoteStatus {
    pub fn output(&self) -> Option<&CompletedOutput> {
        self.metadata.output.as_ref()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StatusError;

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Status Colab belum lengkap atau tidak sah; menunggu pembaruan berikutnya")
    }
}
impl std::error::Error for StatusError {}

pub fn parse_status_slot(bytes: &[u8]) -> Result<RemoteStatus, StatusError> {
    if bytes.len() > 1024 * 1024 {
        return Err(StatusError);
    }
    // Option fields must still be present on the wire, with null when empty.
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| StatusError)?;
    let object = value.as_object().ok_or(StatusError)?;
    if object.len() != 8
        || !object.contains_key("session_id")
        || !object.contains_key("heartbeat_at")
        || !value
            .get("progress")
            .and_then(|p| p.as_object())
            .is_some_and(|p| p.len() == 4 && p.contains_key("last_checkpoint"))
    {
        return Err(StatusError);
    }
    let raw: SlotDocument = serde_json::from_slice(bytes).map_err(|_| StatusError)?;
    if raw.schema_version != 1
        || raw.progress.segment_index > raw.progress.segment_count
        || !raw.progress.percent.is_finite()
        || !(0.0..=100.0).contains(&raw.progress.percent)
        || raw
            .progress
            .last_checkpoint
            .is_some_and(|index| index >= raw.progress.segment_count)
    {
        return Err(StatusError);
    }
    let session_id = raw
        .session_id
        .map(|value| Uuid::parse_str(&value).map_err(|_| StatusError))
        .transpose()?;
    let heartbeat_at = raw
        .heartbeat_at
        .map(|value| {
            if !value.ends_with('Z') {
                return Err(StatusError);
            }
            DateTime::parse_from_rfc3339(&value)
                .map(|time| time.with_timezone(&Utc))
                .map_err(|_| StatusError)
        })
        .transpose()?;
    if !matches!(raw.state, WorkerState::Queued | WorkerState::Failed)
        && (session_id.is_none() || heartbeat_at.is_none())
    {
        return Err(StatusError);
    }
    if raw.state == WorkerState::Completed {
        raw.metadata
            .output
            .as_ref()
            .ok_or(StatusError)?
            .validate()?;
    } else if raw.metadata.output.is_some() {
        return Err(StatusError);
    }
    Ok(RemoteStatus {
        revision: raw.revision,
        state: raw.state,
        session_id,
        heartbeat_at,
        progress: raw.progress,
        message: raw.message,
        metadata: raw.metadata,
    })
}

pub fn select_latest_status(
    a: Result<RemoteStatus, StatusError>,
    b: Result<RemoteStatus, StatusError>,
    previous_revision: u64,
) -> Option<RemoteStatus> {
    let (a, b) = (a.ok(), b.ok());
    if let (Some(a), Some(b)) = (&a, &b) {
        if a.revision != 0 && a.revision == b.revision {
            return None;
        }
    }
    [
        a.filter(|slot| slot.revision == 0 || slot.revision % 2 == 1),
        b.filter(|slot| slot.revision % 2 == 0),
    ]
    .into_iter()
    .flatten()
    .filter(|slot| slot.revision >= previous_revision)
    .max_by_key(|slot| slot.revision)
}

pub struct ReducedStatus {
    pub phase: RemotePhase,
    pub percent: f64,
    pub message: String,
}

pub fn reduce_status(status: &RemoteStatus, now: DateTime<Utc>) -> ReducedStatus {
    let stale = status.state.is_active()
        && status
            .heartbeat_at
            .is_some_and(|heartbeat| now.signed_duration_since(heartbeat) > Duration::seconds(60));
    let (phase, message) = if stale {
        (
            RemotePhase::RuntimeDisconnected,
            "RUNTIME COLAB TERPUTUS — JALANKAN RUN ALL KEMBALI".into(),
        )
    } else {
        match status.state {
            WorkerState::Queued => (
                RemotePhase::WaitingForColab,
                "MENUNGGU COLAB DIJALANKAN".into(),
            ),
            WorkerState::Claimed | WorkerState::Preparing => {
                (RemotePhase::Preparing, "MENYIAPKAN GPU DAN MODEL".into())
            }
            WorkerState::Processing => (
                RemotePhase::Processing,
                format!(
                    "MEMPROSES SEGMEN {} DARI {}",
                    status.progress.segment_index, status.progress.segment_count
                ),
            ),
            WorkerState::Paused => (RemotePhase::Paused, "DIJEDA SETELAH CHECKPOINT".into()),
            // Local completion must wait for download, checksum and media verification.
            WorkerState::Completed => (RemotePhase::Downloading, "MENGUNDUH HASIL".into()),
            WorkerState::Cancelled => (RemotePhase::Cancelled, "DIBATALKAN".into()),
            WorkerState::Failed => (
                RemotePhase::Failed,
                "GAGAL — PERIKSA PESAN DI NOTEBOOK COLAB".into(),
            ),
        }
    };
    ReducedStatus {
        phase,
        percent: status.progress.percent,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use serde_json::{json, Value};

    fn slot(revision: u64, state: &str) -> Value {
        json!({
            "schema_version": 1, "revision": revision, "state": state,
            "session_id": "11111111-1111-4111-8111-111111111111",
            "heartbeat_at": "2026-09-02T12:00:00Z",
            "progress": {"segment_index": 2, "segment_count": 4, "percent": 50.0, "last_checkpoint": 1},
            "message": "Memproses video", "metadata": {}
        })
    }

    fn parsed(value: &Value) -> Result<RemoteStatus, StatusError> {
        parse_status_slot(&serde_json::to_vec(value).unwrap())
    }

    fn completed() -> Value {
        let mut value = slot(9, "completed");
        value["metadata"] = json!({"output": {
            "name": "output.mp4", "size_bytes": 1234, "sha256": "a".repeat(64),
            "width": 128, "height": 96, "duration_seconds": 1.0,
            "fps": "60000/1001", "has_audio": false
        }});
        value
    }

    #[test]
    fn latest_complete_slot_survives_other_slot_partial_write_and_never_rolls_back() {
        let partial = br#"{"schema_version":1,"revision":8"#;
        let selected = select_latest_status(
            parsed(&slot(7, "processing")),
            parse_status_slot(partial),
            6,
        )
        .unwrap();
        assert_eq!(selected.revision, 7);
        assert!(select_latest_status(
            parsed(&slot(7, "processing")),
            parse_status_slot(partial),
            8
        )
        .is_none());
        assert_eq!(
            select_latest_status(
                parsed(&slot(7, "processing")),
                parsed(&slot(8, "paused")),
                8
            )
            .unwrap()
            .state,
            WorkerState::Paused
        );
    }

    #[test]
    fn slot_parity_and_duplicate_revisions_cannot_override_valid_progress() {
        assert!(select_latest_status(
            parsed(&slot(8, "processing")),
            parsed(&slot(8, "paused")),
            0
        )
        .is_none());
        assert_eq!(
            select_latest_status(
                parsed(&slot(6, "processing")),
                parsed(&slot(8, "paused")),
                0
            )
            .unwrap()
            .revision,
            8
        );
        let mut queued = slot(0, "queued");
        queued["session_id"] = Value::Null;
        queued["heartbeat_at"] = Value::Null;
        assert_eq!(
            select_latest_status(parsed(&queued), parsed(&queued), 0)
                .unwrap()
                .state,
            WorkerState::Queued
        );
    }

    #[test]
    fn active_heartbeat_is_disconnected_only_after_sixty_seconds() {
        let heartbeat = Utc.with_ymd_and_hms(2026, 9, 2, 12, 0, 0).unwrap();
        assert_eq!(
            reduce_status(
                &parsed(&slot(7, "processing")).unwrap(),
                heartbeat + Duration::seconds(60)
            )
            .phase,
            RemotePhase::Processing
        );
        let stale = reduce_status(
            &parsed(&slot(7, "processing")).unwrap(),
            heartbeat + Duration::milliseconds(60_001),
        );
        assert_eq!(stale.phase, RemotePhase::RuntimeDisconnected);
        assert!(stale.message.contains("RUN ALL KEMBALI"));
        assert_eq!(
            reduce_status(
                &parsed(&slot(8, "paused")).unwrap(),
                heartbeat + Duration::days(1)
            )
            .phase,
            RemotePhase::Paused
        );
    }

    #[test]
    fn completed_worker_output_starts_download_instead_of_claiming_local_success() {
        let status = parsed(&completed()).unwrap();
        let output = status.output().unwrap();
        assert_eq!(output.size_bytes, 1234);
        assert_eq!(output.fps, "60000/1001");
        assert!(!output.has_audio);
        assert_eq!(
            reduce_status(&status, Utc::now()).phase,
            RemotePhase::Downloading
        );
    }

    #[test]
    fn exact_schema_and_valid_active_owner_are_required() {
        let mut missing = slot(1, "processing");
        missing.as_object_mut().unwrap().remove("message");
        assert!(parsed(&missing).is_err());
        for (key, value) in [
            ("schema_version", json!(2)),
            ("schema_version", json!(true)),
            ("revision", json!(-1)),
            ("revision", json!(1.5)),
            ("state", json!("unknown")),
            ("session_id", Value::Null),
            ("session_id", json!("bad")),
            ("heartbeat_at", Value::Null),
            ("heartbeat_at", json!("2026-09-02T12:00:00+01:00")),
            ("heartbeat_at", json!("bad")),
            ("extra", json!(1)),
            ("metadata", json!({"token":"secret"})),
        ] {
            let mut value_with_error = slot(1, "processing");
            value_with_error[key] = value;
            assert!(parsed(&value_with_error).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn progress_requires_finite_consistent_segment_counts() {
        for (key, value) in [
            ("percent", json!(-1)),
            ("percent", json!(101)),
            ("percent", json!(true)),
            ("segment_index", json!(5)),
            ("segment_count", json!(0)),
            ("last_checkpoint", json!(4)),
            ("last_checkpoint", json!(-1)),
            ("extra", json!(true)),
        ] {
            let mut malformed = slot(1, "processing");
            malformed["progress"][key] = value;
            assert!(parsed(&malformed).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn completed_requires_size_checksum_ratio_and_media_metadata() {
        assert!(parsed(&slot(1, "completed")).is_err());
        for (key, value) in [
            ("name", json!("../output.mp4")),
            ("size_bytes", json!(0)),
            ("sha256", json!("bad")),
            ("width", json!(0)),
            ("height", json!(-1)),
            ("duration_seconds", json!(0)),
            ("fps", json!("24")),
            ("fps", json!("24/0")),
            ("fps", json!("0/1")),
            ("has_audio", json!("false")),
            ("extra", json!(true)),
        ] {
            let mut malformed = completed();
            malformed["metadata"]["output"][key] = value;
            assert!(parsed(&malformed).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn malformed_status_never_echoes_untrusted_content() {
        let error = parse_status_slot(b"private-access-token").unwrap_err();
        assert!(!format!("{error} {error:?}").contains("private-access-token"));
        assert!(parse_status_slot(&vec![b' '; 1_048_577]).is_err());
    }

    #[test]
    fn missing_nullable_progress_and_duplicate_fields_are_not_valid_slots() {
        let mut value = slot(1, "processing");
        value["progress"]
            .as_object_mut()
            .unwrap()
            .remove("last_checkpoint");
        assert!(parsed(&value).is_err());
        let encoded = serde_json::to_string(&slot(1, "processing")).unwrap();
        let duplicate = encoded.replacen("\"revision\":1", "\"revision\":99,\"revision\":1", 1);
        assert!(parse_status_slot(duplicate.as_bytes()).is_err());
    }

    #[test]
    fn worker_redaction_can_expand_message_without_hiding_a_failed_job() {
        let mut value = slot(9, "failed");
        // Worker truncates before replacing credential words with [redacted].
        value["message"] = json!("[redacted] ".repeat(80));
        let status = parsed(&value).unwrap();
        let reduced = reduce_status(&status, Utc::now());
        assert_eq!(reduced.phase, RemotePhase::Failed);
        assert!(!reduced.message.contains("[redacted]"));
    }
}
