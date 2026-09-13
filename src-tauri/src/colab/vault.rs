//! DPAPI-backed storage for the refresh credential only.

use super::auth::{AuthError, DRIVE_FILE_SCOPE};
use crate::secure::dpapi;
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MAX_VAULT_BYTES: u64 = 64 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SavedLogin {
    version: u32,
    pub(super) refresh_token: String,
    pub(super) scope: String,
}

impl fmt::Debug for SavedLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SavedLogin([redacted])")
    }
}

impl SavedLogin {
    fn validate(&self) -> Result<(), AuthError> {
        if self.version != 1
            || self.scope != DRIVE_FILE_SCOPE
            || self.refresh_token.trim().is_empty()
            || self.refresh_token.len() > 16 * 1024
            || self.refresh_token.chars().any(char::is_control)
        {
            return Err(AuthError::Storage);
        }
        Ok(())
    }
}

pub struct TokenVault {
    path: PathBuf,
}

impl TokenVault {
    pub fn new(app_data_dir: &Path) -> Self {
        Self {
            path: app_data_dir.join("colab").join("google-login.dpapi"),
        }
    }

    pub fn load(&self) -> Result<Option<SavedLogin>, AuthError> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AuthError::Storage),
        };
        let mut encrypted = Vec::new();
        file.take(MAX_VAULT_BYTES + 1)
            .read_to_end(&mut encrypted)
            .map_err(|_| AuthError::Storage)?;
        if encrypted.is_empty() || encrypted.len() as u64 > MAX_VAULT_BYTES {
            return Err(AuthError::Storage);
        }
        let plain = dpapi::unprotect(&encrypted).map_err(|_| AuthError::Storage)?;
        let login: SavedLogin = serde_json::from_slice(&plain).map_err(|_| AuthError::Storage)?;
        login.validate()?;
        Ok(Some(login))
    }

    pub fn save(&self, refresh_token: &str, scope: &str) -> Result<(), AuthError> {
        let login = SavedLogin {
            version: 1,
            refresh_token: refresh_token.into(),
            scope: scope.into(),
        };
        login.validate()?;
        let plain = serde_json::to_vec(&login).map_err(|_| AuthError::Storage)?;
        let encrypted = dpapi::protect(&plain).map_err(|_| AuthError::Storage)?;
        let parent = self.path.parent().ok_or(AuthError::Storage)?;
        fs::create_dir_all(parent).map_err(|_| AuthError::Storage)?;
        let temporary = parent.join(format!(".google-login-{}.tmp", Uuid::new_v4()));
        let write_result = (|| -> std::io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&encrypted)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &self.path)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(AuthError::Storage);
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<(), AuthError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AuthError::Storage),
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::fs;
    use uuid::Uuid;

    struct TestDirectory(std::path::PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("xix-colab-vault-test-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn vault_roundtrip_encrypts_refresh_and_saves_no_access_token() {
        let directory = TestDirectory::new();
        let vault = TokenVault::new(&directory.0);
        assert!(vault.load().unwrap().is_none());
        vault.save("test-refresh-secret", DRIVE_FILE_SCOPE).unwrap();
        let bytes = fs::read(&vault.path).unwrap();
        assert!(!bytes.windows(19).any(|w| w == b"test-refresh-secret"));
        let plain = crate::secure::dpapi::unprotect(&bytes).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 3);
        assert_eq!(json["version"], 1);
        assert_eq!(json["refresh_token"], "test-refresh-secret");
        assert_eq!(json["scope"], "https://www.googleapis.com/auth/drive.file");
        assert!(json.get("access_token").is_none());
        let saved = vault.load().unwrap().unwrap();
        assert_eq!(saved.refresh_token, "test-refresh-secret");
        assert!(!format!("{saved:?}").contains("test-refresh-secret"));
    }

    #[test]
    fn vault_replace_and_disconnect_leave_no_temporary_or_plaintext_files() {
        let directory = TestDirectory::new();
        let vault = TokenVault::new(&directory.0);
        vault.save("refresh-one", DRIVE_FILE_SCOPE).unwrap();
        vault.save("refresh-two", DRIVE_FILE_SCOPE).unwrap();
        assert_eq!(vault.load().unwrap().unwrap().refresh_token, "refresh-two");
        assert_eq!(
            fs::read_dir(vault.path.parent().unwrap()).unwrap().count(),
            1
        );
        vault.clear().unwrap();
        vault.clear().unwrap();
        assert!(vault.load().unwrap().is_none());
    }

    #[test]
    fn failed_atomic_replace_keeps_previous_login_and_removes_temporary_blob() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = TestDirectory::new();
        let vault = TokenVault::new(&directory.0);
        vault.save("previous-login", DRIVE_FILE_SCOPE).unwrap();
        let previous = fs::read(&vault.path).unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&vault.path)
            .unwrap();
        assert!(vault.save("new-login", DRIVE_FILE_SCOPE).is_err());
        drop(lock);
        assert_eq!(fs::read(&vault.path).unwrap(), previous);
        assert_eq!(
            vault.load().unwrap().unwrap().refresh_token,
            "previous-login"
        );
        assert_eq!(
            fs::read_dir(vault.path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn vault_refuses_empty_tokens_and_broader_scopes_without_overwriting_saved_login() {
        let directory = TestDirectory::new();
        let vault = TokenVault::new(&directory.0);
        vault.save("keep-me", DRIVE_FILE_SCOPE).unwrap();
        for (token, scope) in [
            ("", DRIVE_FILE_SCOPE),
            (" ", DRIVE_FILE_SCOPE),
            ("refresh", "https://www.googleapis.com/auth/drive"),
            (
                "refresh",
                "https://www.googleapis.com/auth/drive.file email",
            ),
        ] {
            assert!(vault.save(token, scope).is_err());
        }
        assert_eq!(vault.load().unwrap().unwrap().refresh_token, "keep-me");
    }

    #[test]
    fn unreadable_or_wrong_version_vault_is_never_treated_as_valid_login() {
        let directory = TestDirectory::new();
        let vault = TokenVault::new(&directory.0);
        fs::create_dir_all(vault.path.parent().unwrap()).unwrap();
        fs::write(&vault.path, b"plaintext-secret").unwrap();
        let error = vault.load().unwrap_err();
        assert!(!format!("{error:?} {error}").contains("plaintext-secret"));
        for payload in [
            serde_json::json!({"version":2,"refresh_token":"private","scope":DRIVE_FILE_SCOPE}),
            serde_json::json!({"version":1,"refresh_token":"private","scope":"email"}),
            serde_json::json!({"version":1,"refresh_token":"private","scope":DRIVE_FILE_SCOPE,"access_token":"private-access"}),
        ] {
            let encrypted =
                crate::secure::dpapi::protect(&serde_json::to_vec(&payload).unwrap()).unwrap();
            fs::write(&vault.path, encrypted).unwrap();
            assert!(vault.load().is_err());
        }
    }
}
