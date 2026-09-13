//! Typed catalog for every bundled ONNX model.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ModelId {
    EsrganSlim2x,
    EsrganSlim4x,
}

impl ModelId {
    pub fn filename(self) -> &'static str {
        match self {
            Self::EsrganSlim2x => "esrgan-slim-x2.onnx",
            Self::EsrganSlim4x => "esrgan-slim-x4.onnx",
        }
    }
    pub fn manifest_id(self) -> &'static str {
        match self {
            Self::EsrganSlim2x => "esrgan-slim-x2",
            Self::EsrganSlim4x => "esrgan-slim-x4",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendPolicy { PreferDirectMl, RequireDirectMl, CpuOnly }
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InferenceBackend { DirectMl, Cpu }

#[derive(Clone, Debug)]
pub struct ModelSpec { pub id: ModelId, pub path: PathBuf, pub sha256: String, pub scale: u32, pub tile_size: u32, pub overlap: u32 }

#[derive(Clone, Debug, Deserialize)]
pub struct ManifestModel { pub id: String, pub file: String, pub scale: u32, pub bytes: u64, pub sha256: String }
#[derive(Clone, Debug, Deserialize)]
pub struct ModelManifest { pub version: u32, pub models: Vec<ManifestModel>, #[serde(skip)] root: PathBuf }

impl ModelManifest {
    pub fn bundled() -> Result<Self, String> {
        let mut roots = Vec::new();
        if let Some(path) = std::env::var_os("XIX_MODEL_DIR") { roots.push(PathBuf::from(path)); }
        roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models"));
        if let Ok(exe) = std::env::current_exe() { if let Some(parent) = exe.parent() { roots.push(parent.join("models")); roots.push(parent.join("resources").join("models")); } }
        let root = roots.into_iter().find(|root| root.join("MODELS.json").is_file()).ok_or_else(|| "MODELS.json tidak ditemukan".to_string())?;
        let mut manifest: Self = serde_json::from_slice(&fs::read(root.join("MODELS.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        manifest.root = root;
        manifest.validate()?;
        Ok(manifest)
    }
    fn validate(&self) -> Result<(), String> {
        if self.version != 1 || self.models.len() != 2 { return Err(format!("manifest model tidak lengkap: {}", self.models.len())); }
        let mut ids = HashSet::new();
        for model in &self.models {
            if !ids.insert(model.id.as_str()) { return Err(format!("ID model duplikat: {}", model.id)); }
            if model.sha256.len() != 64 || !model.sha256.bytes().all(|b| b.is_ascii_hexdigit()) { return Err(format!("hash model tidak valid: {}", model.id)); }
            let path = self.root.join(&model.file);
            let data = fs::read(&path).map_err(|e| format!("model {} gagal dibaca: {e}", path.display()))?;
            if data.len() as u64 != model.bytes || format!("{:x}", Sha256::digest(&data)) != model.sha256 { return Err(format!("integritas model tidak cocok: {}", model.id)); }
        }
        let expected: HashSet<_> = all_model_ids().iter().map(|id| id.manifest_id()).collect();
        if ids != expected { return Err("matriks ID model tidak cocok".into()); }
        Ok(())
    }
    pub fn model_spec(&self, id: ModelId) -> Result<ModelSpec, String> {
        let entry = self.models.iter().find(|m| m.id == id.manifest_id()).ok_or_else(|| format!("model {} tidak ada", id.manifest_id()))?;
        Ok(ModelSpec { id, path: self.root.join(&entry.file), sha256: entry.sha256.clone(), scale: entry.scale, tile_size: 128, overlap: 16 })
    }
}

pub fn all_model_ids() -> [ModelId; 2] {
    [ModelId::EsrganSlim2x, ModelId::EsrganSlim4x]
}

pub fn model_spec(id: ModelId) -> Result<ModelSpec, String> { ModelManifest::bundled()?.model_spec(id) }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_only_esrgan() {
        assert_eq!(
            all_model_ids().as_slice(),
            &[ModelId::EsrganSlim2x, ModelId::EsrganSlim4x]
        );
        assert_eq!(
            ModelId::EsrganSlim2x.filename(),
            "esrgan-slim-x2.onnx"
        );
        assert_eq!(
            ModelId::EsrganSlim4x.filename(),
            "esrgan-slim-x4.onnx"
        );
    }

    #[test]
    fn bundled_manifest_contains_only_two_esrgan_models() {
        let manifest = ModelManifest::bundled().unwrap();
        let ids: Vec<_> = manifest
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(ids, ["esrgan-slim-x2", "esrgan-slim-x4"]);
    }
}
