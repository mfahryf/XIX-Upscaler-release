pub mod client;
pub mod device;
pub mod error;
pub mod models;
pub mod storage;
pub mod usage;

pub use client::{
    canonical_json, canonical_lease_bytes, canonical_trial_token_bytes, verify_lease_signature,
    verify_trial_token_signature, LicenseClient, TrialClaimResponse,
};
pub use usage::{UsageLedger, UsageRecord};

pub use device::{DeviceIdentity, DeviceIdentityStore};
pub use models::{
    AccessDecision, GatewayStatus, LeasePayload, LicenseState, LicenseStatus, LocalLicenseState,
    TrialState, TrialToken,
};
pub use storage::LicenseStore;

use crate::licensing::error::LicenseError;
use crate::licensing::models::{LicenseStatus as Status, PRODUCT_ID};
use crate::licensing::usage::unix_now;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct LicenseManager {
    dir: PathBuf,
    store: LicenseStore,
    client: LicenseClient,
    identity: Mutex<Option<DeviceIdentity>>,
    state: Mutex<LocalLicenseState>,
}

impl LicenseManager {
    pub fn production(dir: &Path) -> Result<Self, LicenseError> {
        Self::with_client(dir, LicenseClient::production()?)
    }

    pub fn with_client(dir: &Path, client: LicenseClient) -> Result<Self, LicenseError> {
        let store = LicenseStore::new(dir);
        let mut state = store.load()?;
        if state.trial.migrate_legacy() {
            store.save(&state)?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            store,
            client,
            identity: Mutex::new(None),
            state: Mutex::new(state),
        })
    }

    fn load_identity(&self) -> Result<DeviceIdentity, LicenseError> {
        if let Some(identity) = self.identity.lock().as_ref() {
            return Ok(identity.clone());
        }
        let identity = DeviceIdentityStore::load(&self.dir)?;
        *self.identity.lock() = Some(identity.clone());
        Ok(identity)
    }

    fn ensure_identity(&self) -> Result<DeviceIdentity, LicenseError> {
        match self.load_identity() {
            Ok(identity) => Ok(identity),
            Err(LicenseError::DeviceIdentityLost) => {
                if self.has_cached_state(&self.state.lock().clone()) {
                    return Err(LicenseError::DeviceIdentityLost);
                }
                let identity = DeviceIdentityStore::create(&self.dir)?;
                *self.identity.lock() = Some(identity.clone());
                Ok(identity)
            }
            Err(error) => Err(error),
        }
    }

    fn has_cached_state(&self, local: &LocalLicenseState) -> bool {
        self.store.path().exists()
            || local.trial.claimed_at.is_some()
            || local.trial_token.is_some()
            || local.lease.is_some()
            || !local.usage.records.is_empty()
    }

    pub async fn status(&self) -> Result<Status, LicenseError> {
        let local = self.state.lock().clone();
        let identity = match self.load_identity() {
            Ok(identity) => identity,
            Err(LicenseError::DeviceIdentityLost) => {
                if !self.has_cached_state(&local) {
                    return Ok(Status::unactivated_default());
                }
                let mut status = Status::trial_default();
                status.license_state = LicenseState::DeviceIdentityLost;
                status.device_state = "identity-lost".into();
                status.reason = Some("buat pemulihan perangkat melalui admin".into());
                let source = local
                    .lease
                    .as_ref()
                    .map(|lease| lease.device_fingerprint.as_str())
                    .or_else(|| {
                        local
                            .trial_token
                            .as_ref()
                            .and_then(|token| token.payload.get("device_id"))
                            .and_then(serde_json::Value::as_str)
                    })
                    .unwrap_or("identity-lost");
                status.recovery_request_code = Some(recovery_request_code(
                    source,
                    local.last_server_time,
                ));
                status.recovery_contact = Some("hubungi admin lisensi XIXLabs".into());
                return Ok(status);
            }
            Err(error) => return Err(error),
        };
        let now = unix_now();
        let clock_rollback = local
            .last_server_time
            .is_some_and(|last_server_time| !clock_is_trusted(last_server_time, now));
        if let Some(token) = local.trial_token.as_ref() {
            self.client.validate_trial_token(token, &identity)?;
            if !local.trial.matches_token(token) {
                return Err(LicenseError::InvalidTrialToken(
                    "counter lokal melebihi token trial yang ditandatangani".into(),
                ));
            }
        }
        if let Some(lease) = local.lease.as_ref() {
            self.client.validate_cached_lease(lease, &identity)?;
        }
        if DeviceIdentityStore::path(&self.dir).exists() || self.has_cached_state(&local) {
            match self.refresh_from_server(&identity).await {
                Ok(status) => {
                    let refreshed = self.state.lock().clone();
                    if clock_rollback
                        && refreshed
                            .last_server_time
                            .is_some_and(|server_time| {
                                !online_clock_recovery_is_trusted(unix_now(), server_time)
                            })
                    {
                        return Ok(clock_rollback_status());
                    }
                    return Ok(status);
                }
                Err(LicenseError::ClockRollback) if clock_rollback => {
                    return Ok(clock_rollback_status());
                }
                Err(error)
                    if matches!(
                        error,
                        LicenseError::Unauthorized
                            | LicenseError::DeviceConflict
                            | LicenseError::SubscriptionExpired
                            | LicenseError::Revoked
                            | LicenseError::InvalidLease(_)
                            | LicenseError::InvalidTrialToken(_)
                            | LicenseError::ClockRollback
                    ) => return Err(error),
                Err(_) => {}
            }
        }
        if clock_rollback {
            return Ok(clock_rollback_status());
        }
        if local.trial.claimed_at.is_some() && local.trial_token.is_none() {
            return Err(LicenseError::InvalidTrialToken(
                "cache trial lama tidak memiliki token bertanda tangan".into(),
            ));
        }
        self.status_inner(false).await
    }

    async fn refresh_from_server(&self, identity: &DeviceIdentity) -> Result<Status, LicenseError> {
        let mut response = self
            .client
            .status(identity, env!("CARGO_PKG_VERSION"))
            .await?;
        let renew_needed = matches!(
            response.license_state.as_deref(),
            Some("active" | "licensed" | "licensed-online" | "lease_expired")
        ) && response
            .lease
            .as_ref()
            .is_some_and(|lease| lease.lease_expires_at <= unix_now());
        if renew_needed {
            response = self
                .client
                .renew(identity, env!("CARGO_PKG_VERSION"))
                .await?;
        }
        self.store_gateway_status(response)?;
        self.local_status_without_refresh().await
    }

    async fn local_status_without_refresh(&self) -> Result<Status, LicenseError> {
        self.status_inner(false).await
    }

    async fn status_inner(&self, _allow_refresh: bool) -> Result<Status, LicenseError> {
        let identity = self.load_identity()?;
        let local = self.state.lock().clone();
        let now = unix_now();
        let mut status = Status::trial_default();
        status.device_state = local
            .server_device_state
            .clone()
            .unwrap_or_else(|| if local.lease.is_some() { "bound" } else { "registered" }.into());
        status.trial_remaining_by_engine = local.trial.remaining_by_engine();
        status.trial_remaining = local.trial.total_remaining();
        status.license_key_fingerprint = local.license_key_fingerprint;
        status.reason = local.server_reason.clone();
        status.server_time = local.last_server_time;
        status.provider_status = local.server_provider_status.clone();
        status.subscription_status = local.server_subscription_status.clone();
        status.access_status = local.server_access_status.clone();
        if let Some(server_state) = local.server_license_state.as_deref() {
            match map_gateway_state(server_state) {
                Some(LicenseState::Revoked) => {
                    status.license_state = LicenseState::Revoked;
                    return Ok(status);
                }
                Some(LicenseState::DeviceConflict) => {
                    status.license_state = LicenseState::DeviceConflict;
                    return Ok(status);
                }
                Some(LicenseState::SubscriptionExpired) => {
                    status.license_state = LicenseState::SubscriptionExpired;
                    return Ok(status);
                }
                Some(LicenseState::ProviderInactive) => {
                    status.license_state = LicenseState::ProviderInactive;
                    return Ok(status);
                }
                Some(LicenseState::ExpiredOffline) => {
                    status.license_state = LicenseState::ExpiredOffline;
                    return Ok(status);
                }
                Some(LicenseState::Trial) => status.license_state = LicenseState::Trial,
                Some(LicenseState::Unactivated) => status.license_state = LicenseState::Unactivated,
                _ => {}
            }
        }
        if let Some(lease) = local.lease {
            status.subscription_expires_at = Some(lease.subscription_expires_at);
            status.lease_expires_at = Some(lease.lease_expires_at);
            status.server_time = Some(lease.server_time);
            status.key_id = Some(lease.key_id.clone());
            status.signature = Some(lease.signature.clone());
            if local.lease_verified && lease.is_valid_for(PRODUCT_ID, &identity.fingerprint(), now) {
                status.license_state = if local
                    .last_server_time
                    .map(|time| now.saturating_sub(time) <= 300)
                    .unwrap_or(false)
                {
                    LicenseState::Licensed
                } else {
                    LicenseState::LicensedOffline
                };
                status.offline_days_remaining =
                    Some(((lease.lease_expires_at - now).max(0) as u64).div_ceil(86_400) as u16);
            } else if lease.subscription_expires_at <= now {
                status.license_state = LicenseState::SubscriptionExpired;
                status.reason = Some("perpanjang langganan untuk melanjutkan pemrosesan".into());
            } else if matches!(lease.license_state.as_str(), "revoked" | "revoke") {
                status.license_state = LicenseState::Revoked;
                status.reason = Some("hubungi admin untuk bantuan lisensi".into());
            } else {
                status.license_state = LicenseState::ExpiredOffline;
                status.reason = Some("hubungkan internet untuk memvalidasi lisensi".into());
            }
        } else if local.trial.claimed_at.is_some() {
            status.license_state = LicenseState::Trial;
        } else {
            status.license_state = LicenseState::Unactivated;
        }
        Ok(status)
    }

    pub async fn checkout_url(&self) -> Result<String, LicenseError> {
        self.client.checkout_url().await
    }

    pub async fn activate(&self, license_key: String) -> Result<Status, LicenseError> {
        let identity = self.ensure_identity()?;
        let response = self
            .client
            .activate(&identity, env!("CARGO_PKG_VERSION"), &license_key)
            .await?;
        {
            let mut state = self.state.lock();
            state.license_key_fingerprint = Some(fingerprint_secret(&license_key));
            drop(state);
        }
        self.store_gateway_status(response)?;
        self.local_status_without_refresh().await
    }

    pub async fn refresh(&self) -> Result<Status, LicenseError> {
        let identity = self.load_identity()?;
        self.refresh_from_server(&identity).await
    }

    pub async fn preflight(
        &self,
        engine_id: &str,
        requested_files: usize,
    ) -> Result<AccessDecision, LicenseError> {
        if !models::ENGINE_IDS.contains(&engine_id) {
            return Err(LicenseError::InvalidEngine(engine_id.into()));
        }
        let status = self.status().await?;
        match status.license_state {
            LicenseState::Licensed | LicenseState::LicensedOffline => {
                return Ok(AccessDecision::allowed_with_state(
                    engine_id,
                    status.license_state,
                ));
            }
            LicenseState::Trial | LicenseState::Unactivated => {}
            state => {
                return Ok(AccessDecision::denied_with_state(
                    engine_id,
                    state,
                    status
                        .reason
                        .unwrap_or_else(|| "pemrosesan terkunci".into()),
                ))
            }
        }
        if self.state.lock().trial.claimed_at.is_none() {
            let identity = self.ensure_identity()?;
            let claim = self
                .client
                .claim_trial(&identity, env!("CARGO_PKG_VERSION"))
                .await?;
            let mut state = self.state.lock();
            state.trial.claimed_at = Some(claim.claimed_at.unwrap_or_else(unix_now));
            state.trial_token = claim.trial_token;
            if let Some(server_time) = claim.status.server_time {
                self.ensure_server_time_is_monotonic(&state, server_time)?;
                state.last_server_time = Some(server_time);
            }
            state.server_license_state = claim.status.license_state.clone();
            state.server_device_state = claim.status.device_state.clone();
            state.server_reason = claim.status.reason.clone();
            state.server_provider_status = claim.status.provider_status.clone();
            state.server_subscription_status = claim.status.subscription_status.clone();
            state.server_access_status = claim.status.access_status.clone();
            if let Some(remaining) = claim.status.trial_remaining {
                state.trial.set_remaining(remaining);
            } else if !claim.status.trial_remaining_by_engine.is_empty() {
                state
                    .trial
                    .set_remaining_by_engine(&claim.status.trial_remaining_by_engine);
            }
            self.store.save(&state)?;
        }
        Ok(self
            .state
            .lock()
            .trial
            .preflight(engine_id, requested_files))
    }

    fn ensure_server_time_is_monotonic(
        &self,
        state: &LocalLicenseState,
        server_time: i64,
    ) -> Result<(), LicenseError> {
        if state
            .last_server_time
            .is_some_and(|previous| {
                server_time < previous
                    && !clock_recovery_is_trusted(unix_now(), server_time)
            })
        {
            return Err(LicenseError::ClockRollback);
        }
        Ok(())
    }

    fn store_gateway_status(&self, response: GatewayStatus) -> Result<(), LicenseError> {
        let mut state = self.state.lock();
        if let Some(server_time) = response.server_time {
            self.ensure_server_time_is_monotonic(&state, server_time)?;
            state.last_server_time = Some(server_time);
        }
        state.server_license_state = response.license_state;
        state.server_device_state = response.device_state;
        state.server_reason = response.reason;
        state.server_provider_status = response.provider_status;
        state.server_subscription_status = response.subscription_status;
        state.server_access_status = response.access_status;
        let server_reports_active = matches!(
            state.server_license_state.as_deref(),
            Some("active" | "licensed" | "licensed-online")
        );
        if server_reports_active {
            if let Some(lease) = response.lease {
                state.lease_verified = true;
                state.lease = Some(lease);
            } else {
                state.lease = None;
                state.lease_verified = false;
            }
        } else {
            state.lease = None;
            state.lease_verified = false;
        }
        if let Some(remaining) = response.trial_remaining {
            state.trial.merge_remaining(remaining);
        } else if !response.trial_remaining_by_engine.is_empty() {
            state
                .trial
                .merge_remaining_by_engine(&response.trial_remaining_by_engine);
        }
        self.store.save(&state)
    }

    pub fn record_success(
        &self,
        engine_id: &str,
        input: &Path,
        output: &Path,
    ) -> Result<(), LicenseError> {
        let record = UsageRecord::from_paths(engine_id, input, output)?;
        self.record_usage_record(record)
    }

    pub fn record_success_with_event_id(
        &self,
        engine_id: &str,
        input: &Path,
        output: &Path,
        event_id: &str,
    ) -> Result<(), LicenseError> {
        let record = UsageRecord::from_paths_with_event_id(engine_id, input, output, event_id)?;
        self.record_usage_record(record)
    }

    fn record_usage_record(&self, record: UsageRecord) -> Result<(), LicenseError> {
        let device_fingerprint = self.load_identity()?.fingerprint();
        let mut state = self.state.lock();
        if state
            .usage
            .records
            .iter()
            .any(|existing| existing.event_id == record.event_id)
        {
            return Ok(());
        }
        let now = unix_now();
        let paid_active = state.lease_verified
            && state
                .lease
                .as_ref()
                .map(|lease| lease.is_valid_for(PRODUCT_ID, &device_fingerprint, now))
                .unwrap_or(false);
        if !paid_active
            && !state
                .trial
                .record_success(&record.engine_id, &record.event_id)
        {
            return Err(LicenseError::Locked("trial ini sudah habis".into()));
        }
        state.usage.record(record);
        self.store.save(&state)
    }

    pub async fn sync_pending_usage(&self) -> Result<(), LicenseError> {
        let identity = self.load_identity()?;
        let pending = self.state.lock().usage.pending();
        if pending.is_empty() {
            return Ok(());
        }
        let mut synced = Vec::new();
        for record in pending {
            self.client
                .record_usage(&identity, env!("CARGO_PKG_VERSION"), &record)
                .await?;
            synced.push(record.event_id);
        }
        let mut state = self.state.lock();
        state.usage.mark_synced(synced);
        self.store.save(&state)
    }
}

pub struct LicensingState {
    pub manager: std::sync::Arc<LicenseManager>,
}

impl LicensingState {
    pub fn new(dir: &Path) -> Result<Self, LicenseError> {
        Ok(Self {
            manager: std::sync::Arc::new(LicenseManager::production(dir)?),
        })
    }
}

fn fingerprint_secret(value: &str) -> String {
    let digest = Sha256::digest(value.trim().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn clock_is_trusted(last_trusted_server_time: i64, now: i64) -> bool {
    now >= last_trusted_server_time
}

/// Allows an online repair when an old cache was written with a future clock,
/// while still rejecting a device clock that is materially behind the live
/// server time. The five-minute window covers normal clock skew only.
pub fn clock_recovery_is_trusted(local_now: i64, server_time: i64) -> bool {
    const MAX_CLOCK_SKEW_SECONDS: i64 = 300;
    server_time >= local_now.saturating_sub(MAX_CLOCK_SKEW_SECONDS)
        && server_time <= local_now.saturating_add(MAX_CLOCK_SKEW_SECONDS)
}

fn online_clock_recovery_is_trusted(local_now: i64, server_time: i64) -> bool {
    clock_recovery_is_trusted(local_now, server_time)
}

fn clock_rollback_status() -> Status {
    let mut status = Status::trial_default();
    status.license_state = LicenseState::ClockRollback;
    status.device_state = "clock-rollback".into();
    status.reason = Some("periksa waktu perangkat untuk melanjutkan".into());
    status
}

fn recovery_request_code(source: &str, last_server_time: Option<i64>) -> String {
    let digest = Sha256::digest(
        format!(
            "{PRODUCT_ID}:{source}:{}",
            last_server_time.unwrap_or_default()
        )
        .as_bytes(),
    );
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

fn map_gateway_state(value: &str) -> Option<LicenseState> {
    match value {
        "active" | "licensed" | "licensed-online" => Some(LicenseState::Licensed),
        "licensed-offline" => Some(LicenseState::LicensedOffline),
        "trial" | "trial-active" => Some(LicenseState::Trial),
        "unactivated" => Some(LicenseState::Unactivated),
        "expired-offline" => Some(LicenseState::ExpiredOffline),
        "subscription_expired" | "subscription-expired" | "expired" => {
            Some(LicenseState::SubscriptionExpired)
        }
        "provider_inactive" | "provider-inactive" | "provider_disabled" => {
            Some(LicenseState::ProviderInactive)
        }
        "revoked" | "revoke" => Some(LicenseState::Revoked),
        "device-conflict" => Some(LicenseState::DeviceConflict),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
