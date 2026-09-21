use crate::licensing::error::LicenseError;
use crate::licensing::storage::{read_protected, write_protected};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct DeviceIdentity {
    signing_key: SigningKey,
    public_key: VerifyingKey,
    registration_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SignedRequest {
    pub public_key: String,
    pub device_fingerprint: String,
    pub registration_id: String,
    pub nonce: String,
    pub signature: String,
}

#[derive(Serialize, Deserialize)]
struct DeviceIdentityBlob {
    private_key: String,
    public_key: String,
    registration_id: String,
}

impl DeviceIdentity {
    pub fn generate() -> Result<Self, LicenseError> {
        let signing_key = SigningKey::generate(&mut OsRng);
        Self::from_signing_key(signing_key, uuid::Uuid::new_v4().to_string())
    }

    fn from_signing_key(
        signing_key: SigningKey,
        registration_id: String,
    ) -> Result<Self, LicenseError> {
        let public_key = signing_key.verifying_key();
        Ok(Self {
            signing_key,
            public_key,
            registration_id,
        })
    }

    pub fn public_key_base64(&self) -> String {
        BASE64.encode(self.public_key.to_bytes())
    }

    pub fn fingerprint(&self) -> String {
        let digest = Sha256::digest(self.public_key.to_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub fn registration_id(&self) -> &str {
        &self.registration_id
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.public_key
    }

    /// Signs the gateway challenge exactly as specified by the desktop
    /// contract. The signed message is the recursively sorted JSON object,
    /// without a nonce, envelope, or transport headers.
    pub fn sign_challenge(&self, challenge: &str) -> String {
        let message = format!(
            "{{\"challenge\":{},\"device_id\":{}}}",
            serde_json::to_string(challenge).expect("challenge is always a string"),
            serde_json::to_string(&self.registration_id).expect("device id is always a string")
        );
        BASE64.encode(self.signing_key.sign(message.as_bytes()).to_bytes())
    }

    pub fn sign_request(&self, nonce: &str, payload: &[u8]) -> SignedRequest {
        let mut message = Vec::with_capacity(nonce.len() + payload.len() + 1);
        message.extend_from_slice(nonce.as_bytes());
        message.push(b'\n');
        message.extend_from_slice(payload);
        let signature = self.signing_key.sign(&message);
        SignedRequest {
            public_key: self.public_key_base64(),
            device_fingerprint: self.fingerprint(),
            registration_id: self.registration_id.clone(),
            nonce: nonce.to_string(),
            signature: BASE64.encode(signature.to_bytes()),
        }
    }

    #[cfg(test)]
    pub fn private_key_bytes(&self) -> Vec<u8> {
        self.signing_key.to_bytes().to_vec()
    }

    fn blob(&self) -> DeviceIdentityBlob {
        DeviceIdentityBlob {
            private_key: BASE64.encode(self.signing_key.to_bytes()),
            public_key: self.public_key_base64(),
            registration_id: self.registration_id.clone(),
        }
    }

    fn from_blob(blob: DeviceIdentityBlob) -> Result<Self, LicenseError> {
        let private = BASE64
            .decode(blob.private_key)
            .map_err(|_| LicenseError::DeviceIdentityLost)?;
        let bytes: [u8; 32] = private
            .try_into()
            .map_err(|_| LicenseError::DeviceIdentityLost)?;
        let signing_key = SigningKey::from_bytes(&bytes);
        let identity = Self::from_signing_key(signing_key, blob.registration_id)?;
        if identity.public_key_base64() != blob.public_key {
            return Err(LicenseError::DeviceIdentityLost);
        }
        Ok(identity)
    }
}

pub struct DeviceIdentityStore;

impl DeviceIdentityStore {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("device-identity.dat")
    }

    pub fn load(dir: &Path) -> Result<DeviceIdentity, LicenseError> {
        let path = Self::path(dir);
        if let Some(blob) = read_protected::<DeviceIdentityBlob>(&path)
            .map_err(|_| LicenseError::DeviceIdentityLost)?
        {
            return DeviceIdentity::from_blob(blob);
        }
        Err(LicenseError::DeviceIdentityLost)
    }

    pub fn create(dir: &Path) -> Result<DeviceIdentity, LicenseError> {
        let path = Self::path(dir);
        if path.exists() {
            return Self::load(dir);
        }
        let identity = DeviceIdentity::generate()?;
        write_protected(&path, &identity.blob())?;
        Ok(identity)
    }
}
