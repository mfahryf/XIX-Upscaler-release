use crate::licensing::error::LicenseError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub event_id: String,
    pub engine_id: String,
    pub input_fingerprint: String,
    pub output_fingerprint: String,
    pub recorded_at: i64,
    #[serde(default)]
    pub synced: bool,
}

impl UsageRecord {
    pub fn from_paths(engine_id: &str, input: &Path, output: &Path) -> Result<Self, LicenseError> {
        let input_fingerprint = fingerprint_file(input)?;
        let output_fingerprint = fingerprint_file(output)?;
        let event_id = stable_event_id(engine_id, &input_fingerprint, &output_fingerprint);
        Self::from_fingerprints(engine_id, input_fingerprint, output_fingerprint, event_id)
    }

    pub fn from_paths_with_event_id(
        engine_id: &str,
        input: &Path,
        output: &Path,
        event_id: &str,
    ) -> Result<Self, LicenseError> {
        let input_fingerprint = fingerprint_file(input)?;
        let output_fingerprint = fingerprint_file(output)?;
        Self::from_fingerprints(
            engine_id,
            input_fingerprint,
            output_fingerprint,
            event_id.to_string(),
        )
    }

    fn from_fingerprints(
        engine_id: &str,
        input_fingerprint: String,
        output_fingerprint: String,
        event_id: String,
    ) -> Result<Self, LicenseError> {
        Ok(Self {
            event_id,
            engine_id: engine_id.to_string(),
            input_fingerprint,
            output_fingerprint,
            recorded_at: unix_now(),
            synced: false,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageLedger {
    pub records: Vec<UsageRecord>,
}

impl UsageLedger {
    pub fn record(&mut self, record: UsageRecord) -> bool {
        if self
            .records
            .iter()
            .any(|existing| existing.event_id == record.event_id)
        {
            return false;
        }
        self.records.push(record);
        true
    }

    pub fn record_success(
        &mut self,
        engine_id: &str,
        input: &Path,
        output: &Path,
    ) -> Result<bool, LicenseError> {
        let record = UsageRecord::from_paths(engine_id, input, output)?;
        Ok(self.record(record))
    }

    pub fn record_success_with_event_id(
        &mut self,
        engine_id: &str,
        input: &Path,
        output: &Path,
        event_id: &str,
    ) -> Result<bool, LicenseError> {
        let record = UsageRecord::from_paths_with_event_id(engine_id, input, output, event_id)?;
        Ok(self.record(record))
    }

    pub fn pending(&self) -> Vec<UsageRecord> {
        self.records
            .iter()
            .filter(|record| !record.synced)
            .cloned()
            .collect()
    }

    pub fn mark_synced<I, S>(&mut self, event_ids: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let ids: std::collections::BTreeSet<String> = event_ids
            .into_iter()
            .map(|id| id.as_ref().to_string())
            .collect();
        for record in &mut self.records {
            if ids.contains(&record.event_id) {
                record.synced = true;
            }
        }
    }
}

pub fn fingerprint_file(path: &Path) -> Result<String, LicenseError> {
    let data = std::fs::read(path).map_err(|error| LicenseError::Storage(error.to_string()))?;
    let digest = Sha256::digest(data);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn stable_event_id(
    engine_id: &str,
    input_fingerprint: &str,
    output_fingerprint: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(engine_id.as_bytes());
    hasher.update([0]);
    hasher.update(input_fingerprint.as_bytes());
    hasher.update([0]);
    hasher.update(output_fingerprint.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
