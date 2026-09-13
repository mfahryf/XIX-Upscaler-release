//! Encrypted recovery records. A mutation becomes visible only after it is durable.
use super::{
    drive::{RemoteFiles, WorkspaceIds},
    media::SourceMedia,
    remote_status::{CompletedOutput, RemotePhase},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlAction {
    Run,
    Pause,
    Cancel,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteJobRecord {
    pub job_id: Uuid,
    pub local_input: PathBuf,
    pub local_output: PathBuf,
    pub input_sha256: String,
    pub source_media: SourceMedia,
    pub manifest_json: serde_json::Value,
    pub workspace_ids: WorkspaceIds,
    pub remote_files: RemoteFiles,
    pub upload_session: Option<String>,
    pub state: RemotePhase,
    pub last_revision: u64,
    pub control_revision: u64,
    pub desired_control: ControlAction,
    pub keep_drive_files: bool,
    pub completed_output: Option<CompletedOutput>,
}
impl fmt::Debug for RemoteJobRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteJobRecord([redacted])")
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistryError;
impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Catatan pekerjaan Colab tidak dapat dibaca atau disimpan dengan aman")
    }
}
impl std::error::Error for RegistryError {}

pub struct RemoteRegistry {
    path: PathBuf,
    jobs: parking_lot::Mutex<Vec<RemoteJobRecord>>,
}
impl RemoteRegistry {
    pub fn load(app_data_dir: &Path) -> Result<Self, RegistryError> {
        let path = app_data_dir.join("colab/colab-jobs.bin");
        let jobs = match fs::File::open(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(_) => return Err(RegistryError),
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_REGISTRY_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| RegistryError)?;
                if bytes.is_empty() || bytes.len() as u64 > MAX_REGISTRY_BYTES {
                    return Err(RegistryError);
                }
                let plain = crate::secure::dpapi::unprotect(&bytes).map_err(|_| RegistryError)?;
                let envelope: Envelope =
                    serde_json::from_slice(&plain).map_err(|_| RegistryError)?;
                if envelope.version != 1 {
                    return Err(RegistryError);
                }
                validate_records(&envelope.jobs)?;
                envelope.jobs
            }
        };
        Ok(Self {
            path,
            jobs: parking_lot::Mutex::new(jobs),
        })
    }
    pub fn snapshot(&self) -> Vec<RemoteJobRecord> {
        self.jobs.lock().clone()
    }
    pub fn upsert(&self, record: &RemoteJobRecord) -> Result<(), RegistryError> {
        let mut jobs = self.jobs.lock();
        let mut next = jobs.clone();
        if let Some(existing) = next.iter_mut().find(|r| r.job_id == record.job_id) {
            check_update(existing, record)?;
            *existing = record.clone();
        } else {
            next.push(record.clone());
        }
        self.persist(&next)?;
        *jobs = next;
        Ok(())
    }
    pub fn remove(&self, job_id: Uuid) -> Result<(), RegistryError> {
        let mut jobs = self.jobs.lock();
        let next: Vec<_> = jobs
            .iter()
            .filter(|r| r.job_id != job_id)
            .cloned()
            .collect();
        if jobs.len() == next.len() {
            return Ok(());
        }
        self.persist(&next)?;
        *jobs = next;
        Ok(())
    }
    fn persist(&self, jobs: &[RemoteJobRecord]) -> Result<(), RegistryError> {
        validate_records(jobs)?;
        let plain = serde_json::to_vec(&serde_json::json!({"version":1,"jobs":jobs}))
            .map_err(|_| RegistryError)?;
        if plain.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(RegistryError);
        }
        let encrypted = crate::secure::dpapi::protect(&plain).map_err(|_| RegistryError)?;
        if encrypted.len() as u64 > MAX_REGISTRY_BYTES {
            return Err(RegistryError);
        }
        let parent = self.path.parent().ok_or(RegistryError)?;
        fs::create_dir_all(parent).map_err(|_| RegistryError)?;
        let temporary = parent.join(format!("colab-jobs.bin.{}.tmp", Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&encrypted)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(RegistryError);
        }
        Ok(())
    }
}

const MAX_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    version: u32,
    jobs: Vec<RemoteJobRecord>,
}

fn validate_records(records: &[RemoteJobRecord]) -> Result<(), RegistryError> {
    let mut jobs = HashSet::new();
    let mut outputs = HashSet::new();
    let mut remote = HashSet::new();
    let workspace_ids: HashSet<_> = records.iter().flat_map(|r| r.workspace_ids.all()).collect();
    for record in records {
        validate_record(record)?;
        if !jobs.insert(record.job_id)
            || !outputs.insert(super::manifest::output_key(&record.local_output))
        {
            return Err(RegistryError);
        }
        for id in record.remote_files.all() {
            if workspace_ids.contains(id) || !remote.insert(id) {
                return Err(RegistryError);
            }
        }
    }
    Ok(())
}

fn validate_record(record: &RemoteJobRecord) -> Result<(), RegistryError> {
    use super::media::{parse_ratio, validate_input_limits};
    use serde_json::json;
    let (id, name, size) =
        super::drive::manifest_input(&record.manifest_json).map_err(|_| RegistryError)?;
    if id != record.job_id
        || !record.local_input.is_absolute()
        || !record.local_output.is_absolute()
        || record.local_output.extension().and_then(|s| s.to_str()) != Some("mp4")
        || super::manifest::output_key(&record.local_input)
            == super::manifest::output_key(&record.local_output)
        || record.input_sha256.len() != 64
        || !record
            .input_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(RegistryError);
    }
    super::drive::validate_distinct_ids(
        record
            .workspace_ids
            .all()
            .into_iter()
            .chain(record.remote_files.all()),
    )
    .map_err(|_| RegistryError)?;
    let media = &record.source_media;
    validate_input_limits(size, media.duration_seconds).map_err(|_| RegistryError)?;
    if media.width == 0 || media.height == 0 || !(0..360).contains(&media.rotation) {
        return Err(RegistryError);
    }
    parse_ratio(&media.time_base).map_err(|_| RegistryError)?;
    let (source_n, source_d) = parse_ratio(&media.nominal_fps).map_err(|_| RegistryError)?;
    let value = &record.manifest_json;
    let created = value["created_at"]
        .as_str()
        .filter(|s| s.ends_with('Z'))
        .ok_or(RegistryError)?;
    chrono::DateTime::parse_from_rfc3339(created).map_err(|_| RegistryError)?;
    let interpolation = value["interpolation"].as_str().ok_or(RegistryError)?;
    if interpolation != "off" {
        if ![
            "24000/1001",
            "24/1",
            "25/1",
            "30000/1001",
            "30/1",
            "48/1",
            "50/1",
            "60000/1001",
            "60/1",
        ]
        .contains(&interpolation)
        {
            return Err(RegistryError);
        }
        let (target_n, target_d) = parse_ratio(interpolation).map_err(|_| RegistryError)?;
        if u128::from(target_n) * u128::from(source_d)
            <= u128::from(source_n) * u128::from(target_d)
        {
            return Err(RegistryError);
        }
    }
    let mute = value["mute_audio"].as_bool().ok_or(RegistryError)?;
    // Compare against the complete wire contract: extra fields or silently
    // missing options cannot be replayed into a later worker session.
    let upscale = value.get("upscale").ok_or(RegistryError)?;
    let model = upscale["model"].as_str().ok_or(RegistryError)?;
    let scale = upscale["scale"].as_u64().ok_or(RegistryError)?;
    if !matches!((model, scale),
        ("nanovsr-644k", 4) |
        ("realesrgan-x2plus", 2) |
        ("realesrgan-x4plus", 4)
    ) {
        return Err(RegistryError);
    }
    let expected = json!({"schema_version":2,"worker_version":"0.2.0","exchange_protocol":"drive-slots-v1","job_id":id,"created_at":created,
        "input":{"name":name,"extension":value["input"]["extension"],"size_bytes":size,"sha256":record.input_sha256},
        "source":{"width":media.width,"height":media.height,"duration_seconds":media.duration_seconds,"time_base":media.time_base,"nominal_fps":media.nominal_fps,"has_audio":media.has_audio},
        "upscale":upscale,"interpolation":interpolation,"mute_audio":mute,"output":{"format":"mp4","video_codec":"h264","suffix":"-colab"}});
    if *value != expected {
        return Err(RegistryError);
    }
    if let Some(session) = &record.upload_session {
        let url = reqwest::Url::parse(session).map_err(|_| RegistryError)?;
        if session.len() > 8192
            || session
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            || url.scheme() != "https"
            || url.host_str() != Some("www.googleapis.com")
            || url.port_or_known_default() != Some(443)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.path() != format!("/upload/drive/v3/files/{}", record.remote_files.input_id)
            || url
                .query_pairs()
                .filter(|(k, v)| k == "upload_id" && !v.is_empty())
                .count()
                != 1
        {
            return Err(RegistryError);
        }
    }
    if let Some(output) = &record.completed_output {
        output.validate().map_err(|_| RegistryError)?;
        if record.last_revision == 0 || record.upload_session.is_some() {
            return Err(RegistryError);
        }
    } else if matches!(
        record.state,
        RemotePhase::Downloading | RemotePhase::Verifying | RemotePhase::Completed
    ) {
        return Err(RegistryError);
    }
    Ok(())
}

fn check_update(old: &RemoteJobRecord, new: &RemoteJobRecord) -> Result<(), RegistryError> {
    if old.job_id != new.job_id
        || old.local_input != new.local_input
        || old.local_output != new.local_output
        || old.input_sha256 != new.input_sha256
        || old.source_media != new.source_media
        || old.manifest_json != new.manifest_json
        || old.workspace_ids != new.workspace_ids
        || old.remote_files != new.remote_files
        || old.keep_drive_files != new.keep_drive_files
        || new.last_revision < old.last_revision
        || new.control_revision < old.control_revision
        || (new.control_revision == old.control_revision
            && new.desired_control != old.desired_control)
        || (old.completed_output.is_some() && old.completed_output != new.completed_output)
        || (old.state == RemotePhase::Completed && new.state != RemotePhase::Completed)
    {
        return Err(RegistryError);
    }
    Ok(())
}

#[cfg(all(test, windows))]
#[path = "registry_tests.rs"]
mod tests;
