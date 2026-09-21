use crate::licensing::device::DeviceIdentity;
use crate::licensing::error::LicenseError;
use crate::licensing::models::{GatewayStatus, LeasePayload, TrialToken, PRODUCT_ID, TRIAL_FILE_LIMIT, TRIAL_TOTAL_LIMIT};
use crate::licensing::usage::UsageRecord;
use base64::engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD};
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub const DEFAULT_GATEWAY_URL: &str = "https://payment.xixlabs.net";
/// Public verification key for the current gateway signing key.
///
/// This is intentionally public material: it verifies signed leases and trial
/// tokens but cannot issue them. `XIX_GATEWAY_PUBLIC_KEY_B64` remains available
/// as a build-time override for a planned key rotation.
pub const PINNED_GATEWAY_PUBLIC_KEY_B64: &str =
    "zvIgnq2_0OK8YdmMS64_9zeguRLJm4pz0C30bGYjUOo";

#[derive(Clone)]
pub struct LicenseClient {
    base_url: String,
    http: Client,
    gateway_public_key: Option<VerifyingKey>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct TrialClaimResponse {
    #[serde(flatten)]
    pub status: GatewayStatus,
    #[serde(default, deserialize_with = "deserialize_optional_timestamp")]
    pub claimed_at: Option<i64>,
    pub trial_token: Option<TrialToken>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct DeviceChallengeResponse {
    challenge: String,
    #[serde(default)]
    expires_at: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct PublicCheckoutResponse {
    product_id: String,
    checkout_url: String,
}

impl LicenseClient {
    pub fn new(
        base_url: impl Into<String>,
        gateway_public_key_b64: Option<&str>,
    ) -> Result<Self, LicenseError> {
        crate::net::http::ensure_crypto_provider();
        let gateway_public_key = gateway_public_key_b64
            .filter(|key| !key.trim().is_empty())
            .map(parse_public_key)
            .transpose()?;
        let http = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|error| LicenseError::Configuration(error.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
            gateway_public_key,
        })
    }

    pub fn production() -> Result<Self, LicenseError> {
        let gateway_public_key = option_env!("XIX_GATEWAY_PUBLIC_KEY_B64")
            .filter(|value| !value.trim().is_empty())
            .or(Some(PINNED_GATEWAY_PUBLIC_KEY_B64));
        let gateway_url = option_env!("XIX_GATEWAY_URL").unwrap_or(DEFAULT_GATEWAY_URL);
        Self::new(
            gateway_url,
            gateway_public_key,
        )
    }

    pub fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub fn validate_trial_token(
        &self,
        token: &TrialToken,
        identity: &DeviceIdentity,
    ) -> Result<(), LicenseError> {
        let key = self.gateway_public_key.as_ref().ok_or_else(|| {
            LicenseError::InvalidTrialToken(
                "verification key gateway belum dipasang pada build ini".into(),
            )
        })?;
        if token.key_id.trim().is_empty()
            || token
                .payload
                .get("key_id")
                .and_then(Value::as_str)
                .is_some_and(|key_id| key_id != token.key_id)
            || !verify_trial_token_signature(&token.payload, &token.signature, key)
        {
            return Err(LicenseError::InvalidTrialToken(
                "signature token trial tidak cocok".into(),
            ));
        }
        if token.payload.get("product_id").and_then(Value::as_str) != Some(PRODUCT_ID)
            || !token
                .payload
                .get("device_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == identity.registration_id() || id == identity.fingerprint())
            || !valid_trial_counters(&token.payload)
        {
            return Err(LicenseError::InvalidTrialToken(
                "isi token trial tidak sesuai aplikasi atau perangkat".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_cached_lease(
        &self,
        lease: &LeasePayload,
        identity: &DeviceIdentity,
    ) -> Result<(), LicenseError> {
        let key = self.gateway_public_key.as_ref().ok_or_else(|| {
            LicenseError::InvalidLease(
                "verification key gateway belum dipasang pada build ini".into(),
            )
        })?;
        if !verify_lease_signature(lease, key)
            || !lease.is_valid_for(PRODUCT_ID, &identity.fingerprint(), lease.server_time)
        {
            return Err(LicenseError::InvalidLease(
                "signature atau binding lease lokal tidak cocok".into(),
            ));
        }
        Ok(())
    }

    pub async fn claim_trial(
        &self,
        identity: &DeviceIdentity,
        app_version: &str,
    ) -> Result<TrialClaimResponse, LicenseError> {
        let value = self
            .post_state_changing("/v1/desktop/trial/claim", identity, app_version, [])
            .await?;
        let response: TrialClaimResponse = serde_json::from_value(value)
            .map_err(|error| LicenseError::InvalidTrialToken(error.to_string()))?;
        let token = response.trial_token.as_ref().ok_or_else(|| {
            LicenseError::InvalidTrialToken("server tidak mengembalikan token trial".into())
        })?;
        self.validate_trial_token(token, identity)?;
        Ok(response)
    }

    pub async fn checkout_url(&self) -> Result<String, LicenseError> {
        let response = self
            .http
            .get(self.endpoint(&format!("/v1/desktop/products/{PRODUCT_ID}")))
            .header("X-Desktop-Product", PRODUCT_ID)
            .send()
            .await
            .map_err(|error| LicenseError::Network(error.to_string()))?;
        let value = response_value(response).await?;
        let product: PublicCheckoutResponse = serde_json::from_value(value)
            .map_err(|error| LicenseError::Network(format!("checkout catalog tidak valid: {error}")))?;
        let parsed = reqwest::Url::parse(&product.checkout_url)
            .map_err(|error| LicenseError::Network(format!("checkout URL tidak valid: {error}")))?;
        if product.product_id != PRODUCT_ID
            || parsed.scheme() != "https"
            || parsed.host_str().is_none()
        {
            return Err(LicenseError::Network("checkout URL tidak valid".into()));
        }
        Ok(product.checkout_url)
    }

    pub async fn activate(
        &self,
        identity: &DeviceIdentity,
        app_version: &str,
        license_key: &str,
    ) -> Result<GatewayStatus, LicenseError> {
        if license_key.trim().is_empty() {
            return Err(LicenseError::InvalidLicenseKey);
        }
        let value = self
            .post_state_changing(
                "/v1/desktop/license/activate",
                identity,
                app_version,
                [("license_code", Value::String(license_key.trim().to_string()))],
            )
            .await?;
        self.parse_and_verify_lease(value, identity)
    }

    pub async fn status(
        &self,
        identity: &DeviceIdentity,
        app_version: &str,
    ) -> Result<GatewayStatus, LicenseError> {
        let challenge = self.obtain_challenge(identity, app_version).await?;
        let mut query = BTreeMap::new();
        query.insert("device_id", identity.registration_id().to_string());
        query.insert("public_key", identity.public_key_base64());
        query.insert("public_key_fingerprint", identity.fingerprint());
        query.insert("app_version", app_version.to_string());
        query.insert("challenge", challenge.challenge.clone());
        query.insert(
            "signature",
            identity.sign_challenge(&challenge.challenge),
        );
        let response = self
            .http
            .get(self.endpoint("/v1/desktop/license/status"))
            .header("X-Desktop-Product", PRODUCT_ID)
            .query(&query)
            .send()
            .await
            .map_err(|error| LicenseError::Network(error.to_string()))?;
        let value = response_value(response).await?;
        let status: GatewayStatus = serde_json::from_value(value)
            .map_err(|error| LicenseError::Network(format!("respons status tidak valid: {error}")))?;
        if status
            .license_state
            .as_deref()
            .is_some_and(|state| matches!(state, "active" | "licensed" | "licensed-online"))
            && status.lease.is_none()
        {
            return Err(LicenseError::InvalidLease(
                "status aktif tidak menyertakan lease".into(),
            ));
        }
        if let Some(lease) = status.lease.as_ref() {
            self.verify_lease(lease, identity)?;
        }
        Ok(status)
    }

    pub async fn renew(
        &self,
        identity: &DeviceIdentity,
        app_version: &str,
    ) -> Result<GatewayStatus, LicenseError> {
        let value = self
            .post_state_changing("/v1/desktop/license/renew", identity, app_version, [])
            .await?;
        self.parse_and_verify_lease(value, identity)
    }

    pub async fn record_usage(
        &self,
        identity: &DeviceIdentity,
        app_version: &str,
        record: &UsageRecord,
    ) -> Result<(), LicenseError> {
        let value = self
            .post_state_changing(
                "/v1/desktop/usage/record",
                identity,
                app_version,
                [
                    ("engine_id", Value::String(record.engine_id.clone())),
                    ("usage_event_id", Value::String(record.event_id.clone())),
                    (
                        "input_fingerprint",
                        Value::String(record.input_fingerprint.clone()),
                    ),
                    (
                        "output_fingerprint",
                        Value::String(record.output_fingerprint.clone()),
                    ),
                ],
            )
            .await?;
        if value.get("duplicate").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        if value.get("accepted").and_then(Value::as_bool) != Some(true) {
            let reason = value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("server menolak pencatatan usage");
            return Err(LicenseError::Network(reason.to_string()));
        }
        Ok(())
    }

    async fn obtain_challenge(
        &self,
        identity: &DeviceIdentity,
        app_version: &str,
    ) -> Result<DeviceChallengeResponse, LicenseError> {
        let body = serde_json::json!({
            "device_id": identity.registration_id(),
            "public_key": identity.public_key_base64(),
            "public_key_fingerprint": identity.fingerprint(),
            "platform": std::env::consts::OS,
            "app_version": app_version,
        });
        let response = self
            .http
            .post(self.endpoint("/v1/desktop/device/challenge"))
            .header("X-Desktop-Product", PRODUCT_ID)
            .json(&body)
            .send()
            .await
            .map_err(|error| LicenseError::Network(error.to_string()))?;
        let value = response_value(response).await?;
        let challenge: DeviceChallengeResponse = serde_json::from_value(value)
            .map_err(|error| LicenseError::Network(format!("challenge tidak valid: {error}")))?;
        if challenge.challenge.trim().is_empty() || challenge.expires_at.is_none() {
            return Err(LicenseError::Network("server mengembalikan challenge kosong".into()));
        }
        Ok(challenge)
    }

    async fn post_state_changing<const N: usize>(
        &self,
        path: &str,
        identity: &DeviceIdentity,
        app_version: &str,
        extra: [(&str, Value); N],
    ) -> Result<Value, LicenseError> {
        let challenge = self.obtain_challenge(identity, app_version).await?;
        let mut body = Map::new();
        body.insert("device_id".into(), Value::String(identity.registration_id().into()));
        body.insert("public_key".into(), Value::String(identity.public_key_base64()));
        body.insert(
            "public_key_fingerprint".into(),
            Value::String(identity.fingerprint()),
        );
        body.insert("platform".into(), Value::String(std::env::consts::OS.into()));
        body.insert("app_version".into(), Value::String(app_version.to_string()));
        body.insert("challenge".into(), Value::String(challenge.challenge.clone()));
        body.insert(
            "signature".into(),
            Value::String(identity.sign_challenge(&challenge.challenge)),
        );
        for (key, value) in extra {
            body.insert(key.into(), value);
        }
        let response = self
            .http
            .post(self.endpoint(path))
            .header("X-Desktop-Product", PRODUCT_ID)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|error| LicenseError::Network(error.to_string()))?;
        response_value(response).await
    }

    fn parse_and_verify_lease(
        &self,
        value: Value,
        identity: &DeviceIdentity,
    ) -> Result<GatewayStatus, LicenseError> {
        let status: GatewayStatus = serde_json::from_value(value)
            .map_err(|error| LicenseError::InvalidLease(error.to_string()))?;
        let lease = status.lease.as_ref().ok_or_else(|| {
            LicenseError::InvalidLease("server tidak mengembalikan lease".into())
        })?;
        self.verify_lease(lease, identity)?;
        Ok(status)
    }

    fn verify_lease(&self, lease: &LeasePayload, identity: &DeviceIdentity) -> Result<(), LicenseError> {
        let key = self.gateway_public_key.as_ref().ok_or_else(|| {
            LicenseError::InvalidLease(
                "verification key gateway belum dipasang pada build ini".into(),
            )
        })?;
        if !verify_lease_signature(lease, key) {
            return Err(LicenseError::InvalidLease("signature lease tidak cocok".into()));
        }
        if !lease.is_valid_for(PRODUCT_ID, &identity.fingerprint(), lease.server_time) {
            return Err(LicenseError::InvalidLease(
                "isi lease tidak sesuai perangkat atau masa berlaku".into(),
            ));
        }
        Ok(())
    }
}

fn parse_public_key(value: &str) -> Result<VerifyingKey, LicenseError> {
    let bytes = decode_signature(value)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| LicenseError::Configuration("verification key harus 32 byte".into()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|error| LicenseError::Configuration(error.to_string()))
}

async fn response_value(response: reqwest::Response) -> Result<Value, LicenseError> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| LicenseError::Network(error.to_string()))?;
    let value: Value = serde_json::from_str(&body)
        .unwrap_or_else(|_| serde_json::json!({ "message": "respons server bukan JSON" }));
    if !status.is_success() {
        return Err(match status {
            StatusCode::UNAUTHORIZED => LicenseError::Unauthorized,
            StatusCode::FORBIDDEN | StatusCode::CONFLICT => LicenseError::DeviceConflict,
            StatusCode::GONE => LicenseError::SubscriptionExpired,
            StatusCode::TOO_MANY_REQUESTS => {
                LicenseError::Network("permintaan terlalu sering".into())
            }
            _ => LicenseError::Network(response_error_message(&value)),
        });
    }
    Ok(value)
}

fn response_error_message(value: &Value) -> String {
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return message.into();
    }

    let Some(detail) = value.get("detail") else {
        return "server error".into();
    };

    if let Some(message) = detail.get("message").and_then(Value::as_str) {
        return message.into();
    }
    if let Some(message) = detail.as_str() {
        return message.into();
    }
    if let Some(message) = detail.as_array().and_then(|items| {
        items
            .iter()
            .find_map(|item| item.get("msg").and_then(Value::as_str))
    }) {
        return message.into();
    }

    "server error".into()
}

/// Canonical JSON for signatures. Object keys are sorted at every nesting
/// level; arrays preserve their order and scalar JSON uses serde_json's exact
/// representation.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, LicenseError> {
    let mut output = Vec::new();
    write_canonical_json(value, &mut output)?;
    Ok(output)
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) -> Result<(), LicenseError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => output.extend_from_slice(
            &serde_json::to_vec(value)
                .map_err(|error| LicenseError::Configuration(error.to_string()))?,
        ),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort();
            output.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    &serde_json::to_vec(key)
                        .map_err(|error| LicenseError::Configuration(error.to_string()))?,
                );
                output.push(b':');
                write_canonical_json(&values[key], output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

pub fn canonical_lease_bytes(lease: &LeasePayload) -> Vec<u8> {
    let mut value = serde_json::to_value(lease).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = value.as_object_mut() {
        object.remove("signature");
    }
    canonical_json(&value).unwrap_or_default()
}

pub fn verify_lease_signature(lease: &LeasePayload, key: &VerifyingKey) -> bool {
    let Ok(bytes) = decode_signature(&lease.signature) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&bytes) else {
        return false;
    };
    key.verify(&canonical_lease_bytes(lease), &signature).is_ok()
}

pub fn canonical_trial_token_bytes(payload: &Value) -> Vec<u8> {
    canonical_json(payload).unwrap_or_default()
}

pub fn verify_trial_token_signature(payload: &Value, signature: &str, key: &VerifyingKey) -> bool {
    let Ok(bytes) = decode_signature(signature) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&bytes) else {
        return false;
    };
    key.verify(&canonical_trial_token_bytes(payload), &signature).is_ok()
}

fn valid_trial_counters(payload: &Value) -> bool {
    if let Some(remaining) = payload.get("trial_remaining").and_then(Value::as_u64) {
        return remaining <= u64::from(TRIAL_TOTAL_LIMIT);
    }
    let Some(counters) = payload
        .get("trial_remaining_by_engine")
        .and_then(Value::as_object)
    else {
        return false;
    };
    crate::licensing::models::ENGINE_IDS.iter().all(|engine_id| {
        counters
            .get(*engine_id)
            .and_then(Value::as_u64)
            .is_some_and(|value| value <= u64::from(TRIAL_FILE_LIMIT))
    })
}

fn decode_signature(value: &str) -> Result<Vec<u8>, LicenseError> {
    BASE64
        .decode(value.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(value.trim()))
        .map_err(|error| LicenseError::Configuration(error.to_string()))
}

fn deserialize_optional_timestamp<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| value.as_i64()))
}

#[cfg(test)]
mod tests {
    use super::LicenseClient;
    use crate::licensing::error::LicenseError;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn production_client_has_a_pinned_gateway_public_key() {
        let client = LicenseClient::production().expect("production client should be valid");
        assert!(client.gateway_public_key.is_some());
    }

    #[tokio::test]
    async fn structured_gateway_error_exposes_detail_message() {
        crate::net::http::ensure_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut request = [0_u8; 4096];
            stream.read(&mut request).expect("read test request");
            let body = br#"{"detail":{"code":"provider_error","message":"Mayar rejected software license verification (HTTP 404)"}}"#;
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write test headers");
            stream.write_all(body).expect("write test body");
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}/error"))
            .send()
            .await
            .expect("request test server");
        let error = super::response_value(response).await.unwrap_err();

        assert_eq!(
            error,
            LicenseError::Network(
                "Mayar rejected software license verification (HTTP 404)".into()
            )
        );
        server.join().expect("test server join");
    }

    #[tokio::test]
    async fn fetches_public_checkout_url_from_gateway_catalog() {
        crate::net::http::ensure_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).expect("read test request");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("GET /v1/desktop/products/xix-vectorizer HTTP/1.1"));
            let body = br#"{"product_id":"xix-vectorizer","checkout_url":"https://xix-apps.myr.id/pl/xix-vectorizer-monthly-license"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write test headers");
            stream.write_all(body).expect("write test body");
        });

        let client = LicenseClient::new(format!("http://{address}"), None).expect("client");
        let checkout_url = client.checkout_url().await.expect("checkout URL");

        assert_eq!(checkout_url, "https://xix-apps.myr.id/pl/xix-vectorizer-monthly-license");
        server.join().expect("test server join");
    }
}
