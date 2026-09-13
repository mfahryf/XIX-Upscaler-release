//! Filesystem checks at the local recovery and publication boundary.

use super::{registry::RemoteJobRecord, remote_status::CompletedOutput, ColabError};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

const ID_ERROR: ColabError =
    ColabError("Identitas instalasi Colab tidak dapat dibaca atau disimpan dengan aman");
const FILE_ERROR: ColabError =
    ColabError("Berkas Colab tidak tersedia, berubah, atau bukan berkas biasa yang aman");

/// Publish a fully written UUID once. A corrupt existing identity is an error,
/// never permission to generate a different desktop/Drive ownership identity.
pub fn install_id(app_data: &Path) -> Result<Uuid, ColabError> {
    let directory = safe_path(&app_data.join("colab"), true).map_err(|_| ID_ERROR)?;
    fs::create_dir_all(&directory).map_err(|_| ID_ERROR)?;
    safe_path(&directory, false).map_err(|_| ID_ERROR)?;
    let path = directory.join("desktop-install-id");
    match fs::symlink_metadata(&path) {
        Ok(_) => return read_install_id(&path),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => return Err(ID_ERROR),
    }
    let id = Uuid::new_v4();
    let temporary = directory.join(format!(".desktop-install-id-{id}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| ID_ERROR)?;
    let written = file
        .write_all(id.to_string().as_bytes())
        .and_then(|_| file.sync_all());
    drop(file);
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(ID_ERROR);
    }
    // hard_link is create-if-absent on Windows and Unix. rename would replace
    // another process's identity on Unix; create_new on the final path could
    // expose an empty/partial UUID to a concurrent reader or after a crash.
    let published = fs::hard_link(&temporary, &path);
    let cleaned = fs::remove_file(&temporary);
    match published {
        Ok(()) => {
            cleaned.map_err(|_| ID_ERROR)?;
            sync_directory(&directory).map_err(|_| ID_ERROR)?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            cleaned.map_err(|_| ID_ERROR)?;
        }
        Err(_) => return Err(ID_ERROR),
    }
    read_install_id(&path)
}

fn read_install_id(path: &Path) -> Result<Uuid, ColabError> {
    let mut file = open_regular(path).map_err(|_| ID_ERROR)?;
    let mut value = String::new();
    Read::by_ref(&mut file)
        .take(129)
        .read_to_string(&mut value)
        .map_err(|_| ID_ERROR)?;
    if value.len() > 128 || value.trim().len() != 36 {
        return Err(ID_ERROR);
    }
    Uuid::parse_str(value.trim())
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(ID_ERROR)
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Reject every reparse point, including directory junctions.
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

/// Check ancestors as well as the leaf; canonicalize alone hides symlinks.
fn safe_path(path: &Path, allow_missing: bool) -> Result<PathBuf, ColabError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(FILE_ERROR);
    }
    #[cfg(windows)]
    if path.components().any(|part| match part {
        Component::Normal(name) => name.to_string_lossy().contains(':'),
        _ => false,
    }) {
        // NTFS alternate data streams are not standalone regular files.
        return Err(FILE_ERROR);
    }
    let absolute = std::path::absolute(path).map_err(|_| FILE_ERROR)?;
    for ancestor in absolute.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_link(&metadata) || (ancestor != absolute && !metadata.is_dir()) => {
                return Err(FILE_ERROR);
            }
            Ok(_) => {}
            Err(error) if allow_missing && error.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err(FILE_ERROR),
        }
    }
    Ok(absolute)
}

pub(super) fn validate_download_path(path: &Path) -> Result<(), ColabError> {
    let path = safe_path(path, true)?;
    safe_path(path.parent().ok_or(FILE_ERROR)?, false)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !is_link(&metadata) => Ok(()),
        Ok(_) => Err(FILE_ERROR),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(_) => Err(FILE_ERROR),
    }
}

fn open_regular(path: &Path) -> Result<File, ColabError> {
    let path = safe_path(path, false)?;
    if !fs::symlink_metadata(&path)
        .map_err(|_| FILE_ERROR)?
        .is_file()
    {
        return Err(FILE_ERROR);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Open the reparse point itself, and deny concurrent writes/deletion
        // for the lifetime of this read/verification handle.
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    let file = options.open(&path).map_err(|_| FILE_ERROR)?;
    let metadata = file.metadata().map_err(|_| FILE_ERROR)?;
    if !metadata.is_file() || is_link(&metadata) {
        return Err(FILE_ERROR);
    }
    safe_path(&path, false)?;
    Ok(file)
}

fn sync_directory(path: &Path) -> Result<(), ColabError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| FILE_ERROR)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Revalidate bytes, not a cached checksum, before resuming an input upload.
pub fn verify_input(record: &RemoteJobRecord) -> Result<(), ColabError> {
    let error = ColabError("Video input tidak lagi cocok dengan manifest pekerjaan Colab");
    let (job_id, _, size) =
        super::drive::manifest_input(&record.manifest_json).map_err(|_| error.clone())?;
    let hash = record.manifest_json["input"]["sha256"]
        .as_str()
        .ok_or(error.clone())?;
    if job_id != record.job_id || hash != record.input_sha256 {
        return Err(error);
    }
    verify_bytes(&record.local_input, size, hash)?;
    Ok(())
}

struct VerifiedFile {
    file: File,
    path: PathBuf,
    metadata: Metadata,
}

impl VerifiedFile {
    fn unchanged(&self) -> Result<(), ColabError> {
        safe_path(&self.path, false)?;
        let opened = self.file.metadata().map_err(|_| FILE_ERROR)?;
        let named = fs::symlink_metadata(&self.path).map_err(|_| FILE_ERROR)?;
        if !same_metadata(&self.metadata, &opened) || !same_metadata(&self.metadata, &named) {
            return Err(FILE_ERROR);
        }
        Ok(())
    }
}

fn same_metadata(before: &Metadata, after: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return false;
        }
    }
    after.is_file()
        && !is_link(after)
        && before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.created().ok() == after.created().ok()
}

fn verify_bytes(path: &Path, size: u64, hash: &str) -> Result<VerifiedFile, ColabError> {
    let mismatch = ColabError("Ukuran atau SHA-256 berkas tidak cocok dengan pekerjaan Colab");
    if size == 0
        || hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(mismatch);
    }
    let path = safe_path(path, false)?;
    let mut file = open_regular(&path)?;
    let metadata = file.metadata().map_err(|_| FILE_ERROR)?;
    if metadata.len() != size {
        return Err(mismatch);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = match file.read(&mut buffer) {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            result => result.map_err(|_| FILE_ERROR)?,
        };
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .filter(|n| *n <= size)
            .ok_or(mismatch.clone())?;
        digest.update(&buffer[..count]);
    }
    if total != size || format!("{:x}", digest.finalize()) != hash {
        return Err(mismatch);
    }
    let verified = VerifiedFile {
        file,
        path,
        metadata,
    };
    verified.unchanged()?;
    Ok(verified)
}

pub fn verify_output(
    ffprobe: &Path,
    path: &Path,
    expected: &CompletedOutput,
    expected_rotation: i32,
) -> Result<(), ColabError> {
    verified_output(ffprobe, path, expected, expected_rotation).map(|_| ())
}

fn verified_output(
    ffprobe: &Path,
    path: &Path,
    expected: &CompletedOutput,
    rotation: i32,
) -> Result<VerifiedFile, ColabError> {
    let mismatch = ColabError("Metadata video hasil tidak cocok dengan pekerjaan Colab");
    expected.validate().map_err(|_| mismatch.clone())?;
    if !(0..360).contains(&rotation) {
        return Err(mismatch);
    }
    let verified = verify_bytes(path, expected.size_bytes, &expected.sha256)?;
    let media = super::media::probe_video(ffprobe, path)?;
    let (an, ad) = super::media::parse_ratio(&media.nominal_fps)?;
    let (bn, bd) = super::media::parse_ratio(&expected.fps)?;
    if media.width != expected.width
        || media.height != expected.height
        || media.rotation != rotation
        || media.has_audio != expected.has_audio
        || u128::from(an) * u128::from(bd) != u128::from(bn) * u128::from(ad)
        || (media.duration_seconds - expected.duration_seconds).abs() > 0.002
    {
        return Err(mismatch);
    }
    verified.unchanged()?;
    Ok(verified)
}

pub fn promote_output(
    ffprobe: &Path,
    part: &Path,
    dest: &Path,
    expected: &CompletedOutput,
    expected_rotation: i32,
) -> Result<(), ColabError> {
    if part != part_path(dest) {
        return Err(FILE_ERROR);
    }
    let dest = safe_path(dest, true)?;
    match fs::symlink_metadata(&dest) {
        Ok(_) => return verify_output(ffprobe, &dest, expected, expected_rotation),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => return Err(FILE_ERROR),
    }
    let verified = verified_output(ffprobe, part, expected, expected_rotation)?;
    verified.unchanged()?;
    drop(verified);
    publish_without_replace(part, &dest)?;
    // Also catches a concurrent replacement between the final read and move.
    verify_output(ffprobe, &dest, expected, expected_rotation)?;
    sync_directory(dest.parent().ok_or(FILE_ERROR)?)
}

fn publish_without_replace(part: &Path, dest: &Path) -> Result<(), ColabError> {
    #[cfg(windows)]
    {
        use windows::{core::HSTRING, Win32::Storage::FileSystem::MoveFileW};
        // Unlike std::fs::rename, MoveFileW never replaces an existing target.
        // The two siblings are on the same volume, including removable drives.
        unsafe {
            MoveFileW(
                &HSTRING::from(part.as_os_str()),
                &HSTRING::from(dest.as_os_str()),
            )
        }
        .map_err(|_| {
            ColabError(
                "Hasil tidak disimpan karena nama sudah dipakai atau folder tidak dapat ditulis",
            )
        })?;
    }
    #[cfg(not(windows))]
    {
        fs::hard_link(part, dest).map_err(|_| FILE_ERROR)?;
        fs::remove_file(part).map_err(|_| FILE_ERROR)?;
    }
    Ok(())
}

pub fn part_path(dest: &Path) -> PathBuf {
    let Some(name) = dest.file_name() else {
        return dest.join(".output.part");
    };
    let mut hidden = std::ffi::OsString::from(".");
    hidden.push(name);
    hidden.push(".part");
    dest.with_file_name(hidden)
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
