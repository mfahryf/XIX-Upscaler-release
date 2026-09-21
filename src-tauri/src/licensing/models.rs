use super::usage::UsageLedger;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const PRODUCT_ID: &str = "xix-upscaler";
pub const ENGINE_IDS: [&str; 3] = ["upscale-v1", "upscale-esrgan", "video-colab"];
pub const TRIAL_FILE_LIMIT: u8 = 10;
pub const TRIAL_TOTAL_LIMIT: u8 = 10;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LicenseState {
    Trial,
    Licensed,
    LicensedOffline,
    ExpiredOffline,
    SubscriptionExpired,
    ProviderInactive,
    Revoked,
    DeviceConflict,
    DeviceIdentityLost,
    ClockRollback,
    Unavailable,
    Unactivated,
}

impl LicenseState {
    pub fn is_processing_allowed(&self) -> bool {
        matches!(self, Self::Trial | Self::Licensed | Self::LicensedOffline)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTrial {
    pub successful_files: u8,
    #[serde(default)]
    pub usage_event_ids: BTreeSet<String>,
}

impl Default for EngineTrial {
    fn default() -> Self {
        Self {
            successful_files: 0,
            usage_event_ids: BTreeSet::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialState {
    pub engines: BTreeMap<String, EngineTrial>,
    pub claimed_at: Option<i64>,
    #[serde(default)]
    pub successful_files: u8,
    #[serde(default)]
    pub usage_event_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialToken {
    pub payload: Value,
    pub key_id: String,
    pub signature: String,
}

/// Flat status returned by the canonical desktop gateway contract. The
/// optional fields let the client distinguish a valid active lease from
/// server states such as revoked or device-conflict without inventing a
/// wrapper around the gateway response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayStatus {
    #[serde(default)]
    pub license_state: Option<String>,
    #[serde(default)]
    pub device_state: Option<String>,
    #[serde(default)]
    pub trial_remaining_by_engine: BTreeMap<String, u8>,
    #[serde(default)]
    pub trial_remaining: Option<u8>,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub subscription_expires_at: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub lease_expires_at: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub server_time: Option<i64>,
    #[serde(default)]
    pub key_id: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub lease: Option<LeasePayload>,
    #[serde(default)]
    pub provider_status: Option<String>,
    #[serde(default)]
    pub subscription_status: Option<String>,
    #[serde(default)]
    pub access_status: Option<String>,
}

impl TrialState {
    pub fn new<I, S>(engine_ids: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let engines = engine_ids
            .into_iter()
            .map(|id| (id.as_ref().to_string(), EngineTrial::default()))
            .collect();
        Self {
            engines,
            claimed_at: None,
            successful_files: 0,
            usage_event_ids: BTreeSet::new(),
        }
    }

    pub fn remaining(&self, engine_id: &str) -> u8 {
        if self.engines.contains_key(engine_id) {
            self.total_remaining()
        } else {
            0
        }
    }

    pub fn total_remaining(&self) -> u8 {
        TRIAL_TOTAL_LIMIT.saturating_sub(self.successful_files)
    }

    /// Convert the old per-engine counters into the new shared counter once.
    /// The old fields remain in the file so existing installations can be
    /// upgraded without losing their trial history.
    pub fn migrate_legacy(&mut self) -> bool {
        let legacy_successes = self
            .engines
            .values()
            .map(|engine| engine.successful_files)
            .fold(0_u8, |total, value| total.saturating_add(value))
            .min(TRIAL_TOTAL_LIMIT);
        let previous_events = self.usage_event_ids.len();
        for engine in self.engines.values() {
            self.usage_event_ids
                .extend(engine.usage_event_ids.iter().cloned());
        }
        let previous_successes = self.successful_files;
        self.successful_files = self.successful_files.max(legacy_successes);
        self.successful_files != previous_successes || self.usage_event_ids.len() != previous_events
    }

    pub fn is_locked(&self, engine_id: &str) -> bool {
        self.engines.contains_key(engine_id) && self.remaining(engine_id) == 0
    }

    pub fn record_success(&mut self, engine_id: &str, usage_event_id: &str) -> bool {
        if !self.engines.contains_key(engine_id) {
            return false;
        }
        if self.usage_event_ids.contains(usage_event_id)
            || self.successful_files >= TRIAL_TOTAL_LIMIT
        {
            return false;
        }
        let Some(engine) = self.engines.get_mut(engine_id) else {
            return false;
        };
        if engine.usage_event_ids.contains(usage_event_id) {
            return false;
        }
        self.usage_event_ids.insert(usage_event_id.to_string());
        engine.usage_event_ids.insert(usage_event_id.to_string());
        engine.successful_files = engine.successful_files.saturating_add(1);
        self.successful_files = self.successful_files.saturating_add(1);
        true
    }

    pub fn preflight(&self, engine_id: &str, requested_files: usize) -> AccessDecision {
        let Some(_) = self.engines.get(engine_id) else {
            return AccessDecision::denied(engine_id, 0, "engine lisensi tidak dikenal");
        };
        let remaining = self.remaining(engine_id);
        if requested_files == 0 {
            return AccessDecision::denied(engine_id, remaining, "pilih setidaknya satu file");
        }
        if requested_files > remaining as usize {
            return AccessDecision::denied(
                engine_id,
                remaining,
                format!("trial tersisa {remaining} file berhasil"),
            );
        }
        AccessDecision::allowed(engine_id, remaining)
    }

    pub fn state(&self) -> LicenseState {
        LicenseState::Trial
    }

    pub fn set_remaining_by_engine(&mut self, remaining: &BTreeMap<String, u8>) {
        let total_remaining = remaining
            .values()
            .copied()
            .fold(0_u8, u8::saturating_add)
            .min(TRIAL_TOTAL_LIMIT);
        self.set_remaining(total_remaining);
    }

    pub fn set_remaining(&mut self, remaining: u8) {
        self.successful_files = self
            .successful_files
            .max(TRIAL_TOTAL_LIMIT.saturating_sub(remaining.min(TRIAL_TOTAL_LIMIT)));
    }

    /// Merge a server snapshot without undoing successful files that were
    /// completed locally but are still waiting to be synchronized.
    pub fn merge_remaining_by_engine(&mut self, remaining: &BTreeMap<String, u8>) {
        let total_remaining = remaining
            .values()
            .copied()
            .fold(0_u8, u8::saturating_add)
            .min(TRIAL_TOTAL_LIMIT);
        self.merge_remaining(total_remaining);
    }

    pub fn merge_remaining(&mut self, remaining: u8) {
        self.successful_files = self
            .successful_files
            .max(TRIAL_TOTAL_LIMIT.saturating_sub(remaining.min(TRIAL_TOTAL_LIMIT)));
    }

    pub fn remaining_by_engine(&self) -> BTreeMap<String, u8> {
        BTreeMap::new()
    }

    pub fn matches_token(&self, token: &TrialToken) -> bool {
        if let Some(remaining) = token.payload.get("trial_remaining").and_then(Value::as_u64) {
            return u64::from(self.total_remaining()) <= remaining;
        }
        let Some(counters) = token
            .payload
            .get("trial_remaining_by_engine")
            .and_then(Value::as_object)
        else {
            return false;
        };
        let remaining = counters
            .values()
            .filter_map(Value::as_u64)
            .fold(0_u64, u64::saturating_add)
            .min(u64::from(TRIAL_TOTAL_LIMIT));
        u64::from(self.total_remaining()) <= remaining
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessDecision {
    pub allowed: bool,
    pub state: LicenseState,
    pub engine_id: String,
    pub remaining: u8,
    pub message: String,
}

impl AccessDecision {
    pub fn allowed(engine_id: &str, remaining: u8) -> Self {
        Self {
            allowed: true,
            state: LicenseState::Trial,
            engine_id: engine_id.to_string(),
            remaining,
            message: "trial masih tersedia".into(),
        }
    }

    pub fn denied(engine_id: &str, remaining: u8, message: impl Into<String>) -> Self {
        Self {
            allowed: false,
            state: LicenseState::Trial,
            engine_id: engine_id.to_string(),
            remaining,
            message: message.into(),
        }
    }

    pub fn allowed_with_state(engine_id: &str, state: LicenseState) -> Self {
        Self {
            allowed: true,
            state,
            engine_id: engine_id.to_string(),
            remaining: 0,
            message: "lisensi aktif".into(),
        }
    }

    pub fn denied_with_state(
        engine_id: &str,
        state: LicenseState,
        message: impl Into<String>,
    ) -> Self {
        Self {
            allowed: false,
            state,
            engine_id: engine_id.to_string(),
            remaining: 0,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeasePayload {
    pub product_id: String,
    pub device_fingerprint: String,
    pub license_state: String,
    pub subscription_expires_at: i64,
    pub lease_expires_at: i64,
    pub issued_at: i64,
    pub server_time: i64,
    pub key_id: String,
    pub signature: String,
}

impl LeasePayload {
    pub fn is_valid_for(&self, product_id: &str, device_fingerprint: &str, now: i64) -> bool {
        self.product_id == product_id
            && self.device_fingerprint == device_fingerprint
            && self.lease_expires_at > now
            && self.lease_expires_at <= self.subscription_expires_at
            && self.issued_at <= self.server_time
            && matches!(self.license_state.as_str(), "active" | "licensed")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseStatus {
    pub license_state: LicenseState,
    pub subscription_expires_at: Option<i64>,
    pub lease_expires_at: Option<i64>,
    pub device_state: String,
    pub trial_remaining_by_engine: BTreeMap<String, u8>,
    #[serde(default)]
    pub trial_remaining: u8,
    pub reason: Option<String>,
    pub server_time: Option<i64>,
    pub key_id: Option<String>,
    pub signature: Option<String>,
    pub license_key_fingerprint: Option<String>,
    pub offline_days_remaining: Option<u16>,
    pub recovery_request_code: Option<String>,
    pub recovery_contact: Option<String>,
    pub provider_status: Option<String>,
    pub subscription_status: Option<String>,
    pub access_status: Option<String>,
}

impl LicenseStatus {
    pub fn unactivated_default() -> Self {
        let mut status = Self::trial_default();
        status.license_state = LicenseState::Unactivated;
        status.device_state = "unregistered".into();
        status
    }

    pub fn trial_default() -> Self {
        let trial = TrialState::new(ENGINE_IDS);
        Self {
            license_state: LicenseState::Trial,
            subscription_expires_at: None,
            lease_expires_at: None,
            device_state: "unregistered".into(),
            trial_remaining_by_engine: trial.remaining_by_engine(),
            trial_remaining: TRIAL_TOTAL_LIMIT,
            reason: None,
            server_time: None,
            key_id: None,
            signature: None,
            license_key_fingerprint: None,
            offline_days_remaining: None,
            recovery_request_code: None,
            recovery_contact: None,
            provider_status: None,
            subscription_status: None,
            access_status: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalLicenseState {
    pub trial: TrialState,
    #[serde(default)]
    pub trial_token: Option<TrialToken>,
    pub lease: Option<LeasePayload>,
    pub lease_verified: bool,
    pub last_server_time: Option<i64>,
    pub license_key_fingerprint: Option<String>,
    pub usage: UsageLedger,
    #[serde(default)]
    pub server_license_state: Option<String>,
    #[serde(default)]
    pub server_device_state: Option<String>,
    #[serde(default)]
    pub server_reason: Option<String>,
    #[serde(default)]
    pub server_provider_status: Option<String>,
    #[serde(default)]
    pub server_subscription_status: Option<String>,
    #[serde(default)]
    pub server_access_status: Option<String>,
}

impl Default for LocalLicenseState {
    fn default() -> Self {
        Self {
            trial: TrialState::new(ENGINE_IDS),
            trial_token: None,
            lease: None,
            lease_verified: false,
            last_server_time: None,
            license_key_fingerprint: None,
            usage: UsageLedger::default(),
            server_license_state: None,
            server_device_state: None,
            server_reason: None,
            server_provider_status: None,
            server_subscription_status: None,
            server_access_status: None,
        }
    }
}

fn deserialize_optional_timestamp<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.as_ref().and_then(parse_timestamp_value))
}

fn parse_timestamp_value(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str().and_then(parse_rfc3339))
}

fn parse_rfc3339(value: &str) -> Option<i64> {
    let (date, time_and_zone) = value.split_once('T').or_else(|| value.split_once(' '))?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let (time, zone) = if let Some((time, _)) = time_and_zone.split_once('Z') {
        (time, "Z".to_string())
    } else if let Some(index) = time_and_zone.rfind(['+', '-']) {
        (&time_and_zone[..index], time_and_zone[index..].to_string())
    } else {
        (time_and_zone, "Z".to_string())
    };
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = time_parts.next()?.split('.').next()?.parse().ok()?;
    let offset_seconds = if zone == "Z" {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let offset = zone.get(1..)?.split_once(':')?;
        sign * (offset.0.parse::<i64>().ok()? * 3_600 + offset.1.parse::<i64>().ok()? * 60)
    };
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let month_index = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds)
}
