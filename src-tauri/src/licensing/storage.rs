use crate::licensing::error::LicenseError;
use crate::licensing::models::LocalLicenseState;
use crate::secure::dpapi::{protect, unprotect};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{de::DeserializeOwned, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

pub fn write_protected<T: Serialize>(path: &Path, value: &T) -> Result<(), LicenseError> {
    let json =
        serde_json::to_vec(value).map_err(|error| LicenseError::Storage(error.to_string()))?;
    let encrypted = protect(&json).map_err(LicenseError::Storage)?;
    let encoded = BASE64.encode(encrypted);
    let parent = path
        .parent()
        .ok_or_else(|| LicenseError::Storage("folder penyimpanan tidak ditemukan".into()))?;
    fs::create_dir_all(parent).map_err(|error| LicenseError::Storage(error.to_string()))?;
    let temp = temporary_path(path);
    let write_result = (|| -> Result<(), LicenseError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| LicenseError::Storage(error.to_string()))?;
        file.write_all(encoded.as_bytes())
            .map_err(|error| LicenseError::Storage(error.to_string()))?;
        file.flush()
            .map_err(|error| LicenseError::Storage(error.to_string()))?;
        file.sync_all()
            .map_err(|error| LicenseError::Storage(error.to_string()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }

    let backup = backup_path(path);
    if path.exists() {
        if backup.exists() {
            fs::remove_file(&backup).map_err(|error| {
                let _ = fs::remove_file(&temp);
                LicenseError::Storage(error.to_string())
            })?;
        }
        if let Err(error) = fs::rename(path, &backup) {
            let _ = fs::remove_file(&temp);
            return Err(LicenseError::Storage(error.to_string()));
        }
    }

    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        if !path.exists() && backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        return Err(LicenseError::Storage(error.to_string()));
    }
    Ok(())
}

pub fn read_protected<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, LicenseError> {
    if !path.exists() {
        return Ok(None);
    }
    let encoded =
        std::fs::read_to_string(path).map_err(|error| LicenseError::Storage(error.to_string()))?;
    let encrypted = BASE64
        .decode(encoded.trim())
        .map_err(|error| LicenseError::Storage(error.to_string()))?;
    let json = unprotect(&encrypted).map_err(|error| LicenseError::Storage(error))?;
    serde_json::from_slice(&json)
        .map(Some)
        .map_err(|error| LicenseError::Storage(error.to_string()))
}

#[derive(Clone)]
pub struct LicenseStore {
    dir: std::path::PathBuf,
}

impl LicenseStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.dir.join("license-cache.lease")
    }

    pub fn load(&self) -> Result<LocalLicenseState, LicenseError> {
        let primary = self.path();
        match read_protected(&primary) {
            Ok(Some(state)) => Ok(state),
            Ok(None) => self.load_backup(None),
            Err(error) => self.load_backup(Some(error)),
        }
    }

    pub fn save(&self, state: &LocalLicenseState) -> Result<(), LicenseError> {
        write_protected(&self.path(), state)
    }

    fn load_backup(&self, primary_error: Option<LicenseError>) -> Result<LocalLicenseState, LicenseError> {
        let backup = backup_path(&self.path());
        match read_protected(&backup) {
            Ok(Some(state)) => {
                let _ = fs::copy(&backup, self.path());
                Ok(state)
            }
            Ok(None) => primary_error.map_or_else(|| Ok(LocalLicenseState::default()), Err),
            Err(backup_error) => primary_error.map_or(Err(backup_error), Err),
        }
    }
}

fn temporary_path(path: &Path) -> std::path::PathBuf {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("license-cache");
    path.with_file_name(format!("{name}.tmp-{}", uuid::Uuid::new_v4()))
}

fn backup_path(path: &Path) -> std::path::PathBuf {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("license-cache");
    path.with_file_name(format!("{name}.bak"))
}
