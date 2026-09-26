use super::*;
use crate::licensing::models::{ENGINE_IDS, PRODUCT_ID};
use crate::licensing::{
    AccessDecision, LeasePayload, LicenseManager, LicenseState, LicenseStore, LocalLicenseState,
    TrialState, UsageLedger, UsageRecord,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, Verifier, SigningKey};
use rand_core::OsRng;
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

#[test]
fn trial_uses_real_engine_ids_with_one_shared_counter() {
    let mut trial = TrialState::new(["vectorize-v1", "vectorize-v2", "pngtosvg"]);
    assert_eq!(trial.remaining("pngtosvg"), 10);
    for index in 0..5 {
        assert!(trial.record_success("pngtosvg", &format!("usage-{index}")));
    }
    assert_eq!(trial.remaining("pngtosvg"), 5);
    assert!(!trial.is_locked("pngtosvg"));
    assert_eq!(trial.remaining("vectorize-v1"), 5);
    assert_eq!(trial.state(), LicenseState::Trial);
}

#[test]
fn trial_usage_is_idempotent_and_never_goes_negative() {
    let mut trial = TrialState::new(["vectorize-v1"]);
    assert!(trial.record_success("vectorize-v1", "same-event"));
    assert!(!trial.record_success("vectorize-v1", "same-event"));
    for index in 0..10 {
        let _ = trial.record_success("vectorize-v1", &format!("event-{index}"));
    }
    assert_eq!(trial.remaining("vectorize-v1"), 0);
    assert!(trial.is_locked("vectorize-v1"));
}

#[test]
fn trial_uses_one_total_counter_across_engines() {
    let mut trial = TrialState::new(["vectorize-v1", "vectorize-v2", "pngtosvg"]);

    for index in 0..5 {
        assert!(trial.record_success("vectorize-v1", &format!("v1-{index}")));
    }
    for index in 0..5 {
        assert!(trial.record_success("vectorize-v2", &format!("v2-{index}")));
    }

    assert_eq!(trial.remaining("pngtosvg"), 0);
    assert!(!trial.preflight("pngtosvg", 1).allowed);
    assert!(!trial.record_success("pngtosvg", "v3-1"));
}

#[test]
fn legacy_engine_counters_are_migrated_once_into_the_shared_counter() {
    let mut trial = TrialState::new(["vectorize-v1", "vectorize-v2", "pngtosvg"]);
    trial.engines.get_mut("vectorize-v1").unwrap().successful_files = 5;
    trial.engines.get_mut("vectorize-v2").unwrap().successful_files = 5;

    assert!(trial.migrate_legacy());
    assert_eq!(trial.total_remaining(), 0);
    assert!(!trial.preflight("pngtosvg", 1).allowed);
    assert!(!trial.migrate_legacy());
}

#[test]
fn preflight_uses_the_shared_trial_quota() {
    let mut trial = TrialState::new(["vectorize-v1", "vectorize-v2", "pngtosvg"]);
    for index in 0..3 {
        assert!(trial.record_success("vectorize-v2", &format!("event-{index}")));
    }
    assert!(trial.preflight("vectorize-v2", 7).allowed);
    let denied = trial.preflight("vectorize-v2", 8);
    assert!(!denied.allowed);
    assert_eq!(denied.remaining, 7);
    assert!(trial.preflight("pngtosvg", 5).allowed);
}

#[test]
fn trial_status_merge_does_not_restore_unsynced_local_successes() {
    let mut trial = TrialState::new(["vectorize-v1"]);
    assert!(trial.record_success("vectorize-v1", "local-success"));

    trial.merge_remaining(10);

    assert_eq!(trial.remaining("vectorize-v1"), 9);
}

#[test]
fn lease_validation_rejects_expired_or_mismatched_payload() {
    let lease = LeasePayload {
        product_id: "xix-vectorizer".into(),
        device_fingerprint: "device-a".into(),
        license_state: "active".into(),
        subscription_expires_at: 2_000,
        lease_expires_at: 1_500,
        issued_at: 1_000,
        server_time: 1_000,
        key_id: "gateway-2026-01".into(),
        signature: String::new(),
    };
    assert!(lease.is_valid_for("xix-vectorizer", "device-a", 1_200));
    assert!(!lease.is_valid_for("xix-vectorizer", "device-a", 1_500));
    assert!(!lease.is_valid_for("other-product", "device-a", 1_200));
    assert!(!lease.is_valid_for("xix-vectorizer", "device-b", 1_200));
}

#[test]
fn canonical_request_signature_changes_when_payload_changes() {
    let identity = DeviceIdentity::generate().expect("identity generation");
    let first = identity.sign_request("nonce-a", br#"{"action":"status"}"#);
    let second = identity.sign_request("nonce-b", br#"{"action":"status"}"#);
    let changed_payload = identity.sign_request("nonce-a", br#"{"action":"renew"}"#);
    assert_ne!(first.signature, second.signature);
    assert_ne!(first.signature, changed_payload.signature);
    assert_eq!(first.public_key, identity.public_key_base64());
}

#[test]
fn signed_payload_canonicalization_sorts_nested_objects_recursively() {
    let value = json!({
        "z": {"b": 1, "a": [{"d": 4, "c": 3}]},
        "a": 2
    });

    assert_eq!(
        crate::licensing::canonical_json(&value).unwrap(),
        br#"{"a":2,"z":{"a":[{"c":3,"d":4}],"b":1}}"#
    );
}

#[test]
fn challenge_signature_covers_sorted_device_id_and_challenge_only() {
    let identity = DeviceIdentity::generate().expect("identity generation");
    let signature = identity.sign_challenge("challenge-123");
    let signature = BASE64.decode(signature).expect("base64 signature");
    let signature = ed25519_dalek::Signature::from_slice(&signature).unwrap();
    let message = crate::licensing::canonical_json(&json!({
        "device_id": identity.registration_id(),
        "challenge": "challenge-123"
    }))
    .unwrap();

    ed25519_dalek::Verifier::verify(&identity.verifying_key(), &message, &signature).unwrap();
}

#[tokio::test]
async fn claim_uses_device_challenge_and_flat_signed_request_body() {
    let identity = DeviceIdentity::generate().expect("identity generation");
    let gateway_key = SigningKey::generate(&mut OsRng);
    let gateway_public_key = BASE64.encode(gateway_key.verifying_key().to_bytes());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let expected_identity = identity.clone();
    let server = thread::spawn(move || {
        let (mut challenge_stream, _) = listener.accept().unwrap();
        let (path, body) = read_json_request(&mut challenge_stream);
        assert_eq!(path, "/v1/desktop/device/challenge");
        assert_eq!(body["device_id"], expected_identity.registration_id());
        assert_eq!(body["public_key"], expected_identity.public_key_base64());
        assert_eq!(body["public_key_fingerprint"], expected_identity.fingerprint());
        assert_eq!(body["platform"], std::env::consts::OS);
        assert_eq!(body["app_version"], "0.1.0");
        write_json_response(&mut challenge_stream, &json!({
            "challenge": "fresh-challenge",
            "expires_at": 1_800_000_000_i64
        }));

        let (mut claim_stream, _) = listener.accept().unwrap();
        let (path, body) = read_json_request(&mut claim_stream);
        assert_eq!(path, "/v1/desktop/trial/claim");
        let object = body.as_object().unwrap();
        assert_eq!(object.len(), 7);
        assert!(object.get("license_key").is_none());
        assert_eq!(body["challenge"], "fresh-challenge");
        let signature = BASE64.decode(body["signature"].as_str().unwrap()).unwrap();
        let signature = ed25519_dalek::Signature::from_slice(&signature).unwrap();
        let signed = canonical_json(&json!({
            "device_id": expected_identity.registration_id(),
            "challenge": "fresh-challenge"
        }))
        .unwrap();
        expected_identity
            .verifying_key()
            .verify(&signed, &signature)
            .unwrap();

        let trial_counters = || {
            ENGINE_IDS
                .iter()
                .map(|engine_id| ((*engine_id).to_string(), json!(5)))
                .collect::<serde_json::Map<String, serde_json::Value>>()
        };
        let payload = json!({
            "product_id": PRODUCT_ID,
            "device_id": expected_identity.registration_id(),
            "trial_remaining_by_engine": trial_counters(),
            "key_id": "gateway-test"
        });
        let signature = gateway_key.sign(&canonical_trial_token_bytes(&payload));
        write_json_response(&mut claim_stream, &json!({
            "license_state": "trial",
            "device_state": "registered",
            "trial_remaining_by_engine": trial_counters(),
            "server_time": 1_700_000_000_i64,
            "trial_token": {
                "payload": payload,
                "key_id": "gateway-test",
                "signature": BASE64.encode(signature.to_bytes())
            }
        }));
    });

    let client = LicenseClient::new(
        format!("http://{address}"),
        Some(&gateway_public_key),
    )
    .unwrap();
    let response = client.claim_trial(&identity, "0.1.0").await.unwrap();
    assert_eq!(response.status.license_state.as_deref(), Some("trial"));
    assert!(response.trial_token.is_some());
    server.join().unwrap();
}

fn read_json_request(stream: &mut TcpStream) -> (String, serde_json::Value) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end;
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "client closed request before headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = index + 4;
            break;
        }
    }
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:").or_else(|| line.strip_prefix("Content-Length:")))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "client closed request before body");
        bytes.extend_from_slice(&buffer[..read]);
    }
    let request_line = headers.lines().next().unwrap();
    let path = request_line.split_whitespace().nth(1).unwrap().to_string();
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
    (path, body)
}

fn write_json_response(stream: &mut TcpStream, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).unwrap();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(&body).unwrap();
}

#[test]
fn status_does_not_expose_private_material() {
    let status = LicenseStatus::trial_default();
    let json = serde_json::to_string(&status).unwrap();
    assert!(!json.contains("private"));
    assert!(!json.contains("secret"));
    assert!(!json.contains("raw_key"));
}

#[test]
fn device_identity_roundtrips_through_protected_storage() {
    let dir = tempfile_dir("identity");
    let first = DeviceIdentityStore::create(&dir).expect("create identity");
    let second = DeviceIdentityStore::load(&dir).expect("load identity");
    assert_eq!(first.fingerprint(), second.fingerprint());
    assert_eq!(first.public_key_base64(), second.public_key_base64());
    assert_ne!(
        std::fs::read(dir.join("device-identity.dat")).unwrap(),
        first.private_key_bytes()
    );
}

#[test]
fn usage_ledger_counts_each_successful_input_once_and_queues_sync() {
    let dir = tempfile_dir("usage");
    let input = dir.join("input.png");
    let output = dir.join("output.svg");
    std::fs::write(&input, b"input-content").unwrap();
    std::fs::write(&output, b"output-content").unwrap();

    let mut ledger = UsageLedger::default();
    let first = ledger
        .record_success("vectorize-v1", &input, &output)
        .unwrap();
    let second = ledger
        .record_success("vectorize-v1", &input, &output)
        .unwrap();
    assert!(first);
    assert!(!second);
    assert_eq!(ledger.pending().len(), 1);
    assert_eq!(ledger.pending()[0].engine_id, "vectorize-v1");
    assert!(!ledger.pending()[0].input_fingerprint.is_empty());
}

#[test]
fn usage_ledger_counts_repeated_attempts_with_distinct_event_ids() {
    let dir = tempfile_dir("usage-attempts");
    let input = dir.join("input.png");
    let output = dir.join("output.svg");
    std::fs::write(&input, b"same-input").unwrap();
    std::fs::write(&output, b"same-output").unwrap();

    let mut ledger = UsageLedger::default();
    let first = ledger
        .record_success_with_event_id("vectorize-v1", &input, &output, "attempt-1")
        .unwrap();
    let second = ledger
        .record_success_with_event_id("vectorize-v1", &input, &output, "attempt-2")
        .unwrap();

    assert!(first);
    assert!(second);
    assert_eq!(ledger.pending().len(), 2);
}

#[test]
fn usage_record_event_id_is_stable_for_same_file_contents() {
    let dir = tempfile_dir("usage-stable");
    let input = dir.join("input.png");
    let output = dir.join("output.svg");
    std::fs::write(&input, b"same-input").unwrap();
    std::fs::write(&output, b"same-output").unwrap();

    let first = UsageRecord::from_paths("pngtosvg", &input, &output).unwrap();
    let second = UsageRecord::from_paths("pngtosvg", &input, &output).unwrap();
    assert_eq!(first.event_id, second.event_id);
    assert_eq!(first.input_fingerprint, second.input_fingerprint);
}

#[test]
fn lease_signature_verification_rejects_tampering() {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;

    let signing_key = SigningKey::generate(&mut OsRng);
    let mut lease = LeasePayload {
        product_id: "xix-vectorizer".into(),
        device_fingerprint: "device-a".into(),
        license_state: "active".into(),
        subscription_expires_at: 2_000,
        lease_expires_at: 1_500,
        issued_at: 1_000,
        server_time: 1_000,
        key_id: "gateway-2026-01".into(),
        signature: String::new(),
    };
    let signature = signing_key.sign(&crate::licensing::canonical_lease_bytes(&lease));
    lease.signature = BASE64.encode(signature.to_bytes());
    assert!(crate::licensing::verify_lease_signature(
        &lease,
        &signing_key.verifying_key()
    ));
    lease.lease_expires_at = 1_400;
    assert!(!crate::licensing::verify_lease_signature(
        &lease,
        &signing_key.verifying_key()
    ));
}

#[test]
fn local_license_state_roundtrips_without_raw_license_key() {
    let dir = tempfile_dir("state");
    let store = LicenseStore::new(&dir);
    let state = LocalLicenseState::default();
    store.save(&state).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded, state);
    let raw = std::fs::read_to_string(store.path()).unwrap();
    assert!(!raw.contains("license_key"));
    assert!(!raw.contains("private_key"));
}

#[test]
fn protected_store_recovers_from_valid_backup_when_primary_is_missing() {
    let dir = tempfile_dir("state-backup-recovery");
    let store = LicenseStore::new(&dir);
    let mut state = LocalLicenseState::default();
    state.server_license_state = Some("active".into());
    state.last_server_time = Some(1_700_000_000);
    store.save(&state).unwrap();

    let primary = store.path();
    let backup = primary.with_file_name("license-cache.lease.bak");
    std::fs::copy(&primary, &backup).unwrap();
    std::fs::remove_file(&primary).unwrap();

    assert_eq!(store.load().unwrap(), state);
}

#[test]
fn current_status_time_is_not_replaced_by_historical_lease_time() {
    let dir = tempfile_dir("clock-lease-watermark");
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();
    let now = crate::licensing::usage::unix_now();
    let old_lease_time = now - 600;
    let response = crate::licensing::GatewayStatus {
        license_state: Some("active".into()),
        device_state: Some("registered".into()),
        server_time: Some(now),
        lease: Some(LeasePayload {
            product_id: "xix-vectorizer".into(),
            device_fingerprint: "device-a".into(),
            license_state: "active".into(),
            subscription_expires_at: now + 86_400,
            lease_expires_at: now + 3_600,
            issued_at: old_lease_time,
            server_time: old_lease_time,
            key_id: "gateway-2026-01".into(),
            signature: "test-signature".into(),
        }),
        ..Default::default()
    };

    manager.store_gateway_status(response).unwrap();
    assert_eq!(manager.state.lock().last_server_time, Some(now));
}

#[test]
fn server_reset_clears_cached_lease() {
    let dir = tempfile_dir("server-reset-clears-lease");
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();
    let now = crate::licensing::usage::unix_now();
    manager.state.lock().lease = Some(LeasePayload {
        product_id: "xix-vectorizer".into(),
        device_fingerprint: "device-a".into(),
        license_state: "active".into(),
        subscription_expires_at: now + 86_400,
        lease_expires_at: now + 3_600,
        issued_at: now,
        server_time: now,
        key_id: "gateway-2026-01".into(),
        signature: "test-signature".into(),
    });
    manager.state.lock().lease_verified = true;

    manager
        .store_gateway_status(crate::licensing::GatewayStatus {
            license_state: Some("unactivated".into()),
            device_state: Some("registered".into()),
            server_time: Some(now + 1),
            lease: Some(LeasePayload {
                product_id: "xix-vectorizer".into(),
                device_fingerprint: "device-a".into(),
                license_state: "active".into(),
                subscription_expires_at: now + 86_400,
                lease_expires_at: now + 3_600,
                issued_at: now,
                server_time: now,
                key_id: "gateway-2026-01".into(),
                signature: "stale-signature".into(),
            }),
            ..Default::default()
        })
        .unwrap();

    let state = manager.state.lock();
    assert!(state.lease.is_none());
    assert!(!state.lease_verified);
}

#[tokio::test]
async fn fresh_manager_reports_trial_without_network_call() {
    let dir = tempfile_dir("manager");
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();
    let status = manager.status().await.unwrap();
    assert_eq!(status.license_state, LicenseState::Unactivated);
    assert_eq!(status.trial_remaining, 10);
    assert!(status.trial_remaining_by_engine.is_empty());
    assert!(!DeviceIdentityStore::path(&dir).exists());
}

#[test]
fn signed_trial_token_verification_rejects_counter_or_device_tampering() {
    let signing_key = SigningKey::generate(&mut OsRng);
    let mut payload = json!({
        "product_id": "xix-vectorizer",
        "device_id": "device-1",
        "claimed_at": "2026-09-14T00:00:00+00:00",
        "trial_remaining_by_engine": {"pngtosvg": 5},
        "key_id": "xix-license-test"
    });
    let signature = signing_key.sign(&crate::licensing::canonical_trial_token_bytes(&payload));
    let signature = BASE64.encode(signature.to_bytes());

    assert!(crate::licensing::verify_trial_token_signature(
        &payload,
        &signature,
        &signing_key.verifying_key()
    ));
    payload["trial_remaining_by_engine"]["pngtosvg"] = json!(6);
    assert!(!crate::licensing::verify_trial_token_signature(
        &payload,
        &signature,
        &signing_key.verifying_key()
    ));
}

#[tokio::test]
async fn local_trial_state_rejects_a_tampered_signed_token() {
    let dir = tempfile_dir("trial-token-state");
    let identity = DeviceIdentityStore::create(&dir).unwrap();
    let signing_key = SigningKey::generate(&mut OsRng);
    let payload = json!({
        "product_id": "xix-vectorizer",
        "device_id": identity.registration_id(),
        "claimed_at": "2026-09-14T00:00:00+00:00",
        "trial_remaining_by_engine": {
            "vectorize-v1": 5,
            "vectorize-v2": 5,
            "pngtosvg": 5
        },
        "key_id": "xix-license-test"
    });
    let signature = signing_key.sign(&crate::licensing::canonical_trial_token_bytes(&payload));
    let token = crate::licensing::TrialToken {
        payload: json!({
            "product_id": "xix-vectorizer",
            "device_id": identity.registration_id(),
            "claimed_at": "2026-09-14T00:00:00+00:00",
            "trial_remaining_by_engine": {
                "vectorize-v1": 6,
                "vectorize-v2": 5,
                "pngtosvg": 5
            },
            "key_id": "xix-license-test"
        }),
        key_id: "xix-license-test".into(),
        signature: BASE64.encode(signature.to_bytes()),
    };
    let mut state = LocalLicenseState::default();
    state.trial.claimed_at = Some(1_789_000_000);
    state.trial_token = Some(token);
    LicenseStore::new(&dir).save(&state).unwrap();
    let public_key = BASE64.encode(signing_key.verifying_key().to_bytes());
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", Some(&public_key)).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();

    let error = manager.status().await.unwrap_err();

    assert!(matches!(error, crate::licensing::LicenseError::InvalidTrialToken(_)));
}

#[tokio::test]
async fn missing_identity_with_cached_state_reports_recovery_without_recreating_device() {
    let dir = tempfile_dir("identity-lost");
    let store = LicenseStore::new(&dir);
    let mut local = LocalLicenseState::default();
    local.trial.claimed_at = Some(1_700_000_000);
    store.save(&local).unwrap();
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();

    let status = manager.status().await.unwrap();

    assert_eq!(status.license_state, LicenseState::DeviceIdentityLost);
    assert_eq!(status.device_state, "identity-lost");
    assert!(status.recovery_request_code.is_some());
    assert!(status.recovery_contact.is_some());
    assert!(!DeviceIdentityStore::path(&dir).exists());
}

#[tokio::test]
async fn activation_does_not_replace_a_lost_identity() {
    let dir = tempfile_dir("activation-identity-lost");
    let store = LicenseStore::new(&dir);
    let mut local = LocalLicenseState::default();
    local.trial.claimed_at = Some(1_700_000_000);
    store.save(&local).unwrap();
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();

    let error = manager.activate("XIX-test-key".into()).await.unwrap_err();

    assert_eq!(error, crate::licensing::LicenseError::DeviceIdentityLost);
    assert!(!DeviceIdentityStore::path(&dir).exists());
}

#[tokio::test]
async fn first_processing_preflight_claims_online_before_allowing_trial() {
    let dir = tempfile_dir("first-processing");
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();

    let error = manager.preflight(ENGINE_IDS[0], 1).await.unwrap_err();

    assert!(matches!(error, crate::licensing::LicenseError::Network(_)));
    assert!(DeviceIdentityStore::path(&dir).exists());
    assert!(!LicenseStore::new(&dir).path().exists());
}

#[tokio::test]
async fn refresh_on_a_fresh_install_creates_the_device_instead_of_failing_locally() {
    let dir = tempfile_dir("refresh-fresh");
    let client = crate::licensing::LicenseClient::new("http://127.0.0.1:1", None).unwrap();
    let manager = LicenseManager::with_client(&dir, client).unwrap();

    let error = manager.refresh().await.unwrap_err();

    // The gateway is unreachable here, so the failure has to be a network
    // problem. It must not be the local "device identity lost" that never
    // reaches the server, which is what a fresh install used to hit.
    assert!(matches!(error, crate::licensing::LicenseError::Network(_)));
    assert!(DeviceIdentityStore::path(&dir).exists());
}

#[test]
fn local_clock_before_last_trusted_server_time_is_rejected() {
    assert!(!crate::licensing::clock_is_trusted(2_000, 1_999));
    assert!(crate::licensing::clock_is_trusted(2_000, 2_000));
    assert!(crate::licensing::clock_is_trusted(2_000, 2_001));
}

#[test]
fn online_clock_recovery_accepts_a_current_server_after_a_stale_future_cache() {
    assert!(crate::licensing::clock_recovery_is_trusted(2_000, 2_001));
    assert!(crate::licensing::clock_recovery_is_trusted(2_000, 1_999));
    assert!(!crate::licensing::clock_recovery_is_trusted(1_000, 2_000));
    assert!(!crate::licensing::clock_recovery_is_trusted(2_000, 2_400));
}

#[test]
fn online_refresh_accepts_small_server_clock_skew_after_startup_rollback() {
    assert!(super::online_clock_recovery_is_trusted(2_000, 2_001));
    assert!(!super::online_clock_recovery_is_trusted(2_000, 2_301));
}

#[test]
fn gateway_provider_and_subscription_lock_states_are_mapped_before_cached_lease() {
    assert_eq!(
        crate::licensing::map_gateway_state("provider_inactive"),
        Some(crate::licensing::models::LicenseState::ProviderInactive)
    );
    assert_eq!(
        crate::licensing::map_gateway_state("subscription_expired"),
        Some(crate::licensing::models::LicenseState::SubscriptionExpired)
    );
}

#[test]
fn licensed_access_decision_is_not_limited_by_trial_counter() {
    let decision = AccessDecision::allowed_with_state("pngtosvg", LicenseState::Licensed);
    assert!(decision.allowed);
    assert_eq!(decision.state, LicenseState::Licensed);
}

#[test]
fn file_done_contains_input_for_usage_deduplication() {
    let event = crate::batch::BatchEvent::FileDone {
        input: "C:/in/a.png".into(),
        name: "a.png".into(),
        output: "C:/out/a.svg".into(),
        usage_event_id: "attempt-1".into(),
    };
    match event {
        crate::batch::BatchEvent::FileDone { input, output, .. } => {
            assert_eq!(input, "C:/in/a.png");
            assert!(output.ends_with("a.svg"));
        }
        _ => unreachable!(),
    }
}

fn tempfile_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "xix-vectorizer-license-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
