use super::{
    media::{self, SourceMedia, VideoProbe},
    ColabError,
};
use crate::engines::EngineOptions;
use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug)]
pub struct PreparedJob {
    pub job_id: Uuid,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub input_sha256: String,
    pub source_media: SourceMedia,
    pub manifest_json: serde_json::Value,
}

pub fn prepare_job(
    input: &Path,
    output_dir: &Path,
    options: &EngineOptions,
    ffprobe: &Path,
    now: DateTime<Utc>,
    reserved_outputs: &mut HashSet<PathBuf>,
) -> Result<PreparedJob, ColabError> {
    prepare(
        input,
        output_dir,
        options,
        &VideoProbe {
            executable: ffprobe,
            run: &media::run_probe_command,
        },
        now,
        reserved_outputs,
    )
}

fn prepare(
    input: &Path,
    output_dir: &Path,
    options: &EngineOptions,
    probe: &VideoProbe<'_>,
    now: DateTime<Utc>,
    reserved_outputs: &mut HashSet<PathBuf>,
) -> Result<PreparedJob, ColabError> {
    let options = ColabOptions::parse(options)?;
    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .ok_or(ColabError("Ekstensi video tidak didukung"))?
        .to_ascii_lowercase();
    if !["mp4", "mov", "webm", "mkv", "m4v", "ts"].contains(&extension.as_str()) {
        return Err(ColabError("Ekstensi video tidak didukung"));
    }
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or(ColabError("Nama video tidak valid"))?;
    let input_path = input
        .canonicalize()
        .map_err(|_| ColabError("Video input tidak dapat dibaca"))?;
    let mut file =
        File::open(&input_path).map_err(|_| ColabError("Video input tidak dapat dibaca"))?;
    let before = file
        .metadata()
        .map_err(|_| ColabError("Ukuran video tidak dapat dibaca"))?;
    if !before.is_file() {
        return Err(ColabError("Input harus berupa berkas video"));
    }
    media::validate_input_size(before.len())?;
    let output_dir = output_dir
        .canonicalize()
        .map_err(|_| ColabError("Folder hasil tidak tersedia"))?;
    if !output_dir.is_dir() {
        return Err(ColabError("Folder hasil tidak tersedia"));
    }

    let source_media = probe.read(&input_path)?;
    media::validate_input_limits(before.len(), source_media.duration_seconds)?;
    if options.interpolation != "off" {
        let (target_n, target_d) = media::parse_ratio(options.interpolation)?;
        let (source_n, source_d) = media::parse_ratio(&source_media.nominal_fps)?;
        if u128::from(target_n) * u128::from(source_d)
            <= u128::from(source_n) * u128::from(target_d)
        {
            return Err(ColabError("Target FPS harus lebih tinggi dari FPS sumber"));
        }
    }
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut total = 0;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| ColabError("Video input tidak dapat dibaca"))?;
        if count == 0 {
            break;
        }
        total += count as u64;
        media::validate_input_size(total)?;
        digest.update(&buffer[..count]);
    }
    let after = file
        .metadata()
        .map_err(|_| ColabError("Ukuran video tidak dapat dibaca"))?;
    if total != before.len()
        || after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
    {
        return Err(ColabError(
            "Video input berubah selama pemeriksaan; pilih ulang video",
        ));
    }
    let input_sha256 = format!("{:x}", digest.finalize());
    let output_path = choose_output(&output_dir, stem, options.local_suffix, reserved_outputs)?;
    let job_id = Uuid::new_v4();
    // Deliberately enumerate the wire fields: rotation, local paths and retention
    // policy belong on the desktop, never in the strict worker manifest.
    let manifest_json = serde_json::json!({
        "schema_version": 2,
        "worker_version": "0.2.0",
        "exchange_protocol": "drive-slots-v1",
        "job_id": job_id.to_string(),
        "created_at": now.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        "input": {"name": format!("input.{extension}"), "extension": extension,
            "size_bytes": total, "sha256": input_sha256},
        "source": {"width": source_media.width, "height": source_media.height,
            "duration_seconds": source_media.duration_seconds, "time_base": source_media.time_base,
            "nominal_fps": source_media.nominal_fps, "has_audio": source_media.has_audio},
        "upscale": {"model": options.upscale_model, "scale": options.upscale_scale},
        "interpolation": options.interpolation,
        "mute_audio": options.mute_audio,
        // Worker 0.2 fixes this profile; a custom suffix changes only output_path.
        "output": {"format": "mp4", "video_codec": "h264", "suffix": "-colab"},
    });
    reserved_outputs.insert(output_path.clone());
    Ok(PreparedJob {
        job_id,
        input_path,
        output_path,
        input_sha256,
        source_media,
        manifest_json,
    })
}

struct ColabOptions<'a> {
    interpolation: &'static str,
    mute_audio: bool,
    local_suffix: &'a str,
    upscale_model: &'static str,
    upscale_scale: u8,
}

impl<'a> ColabOptions<'a> {
    fn parse(options: &'a EngineOptions) -> Result<Self, ColabError> {
        let text = |key| {
            options
                .get(key)
                .and_then(|value| value.as_str())
                .ok_or(ColabError(
                    "Opsi Colab wajib berupa teks dan tidak boleh hilang",
                ))
        };
        let boolean = |key| {
            options
                .get(key)
                .and_then(|value| value.as_bool())
                .ok_or(ColabError(
                    "Opsi Colab wajib berupa boolean dan tidak boleh hilang",
                ))
        };
        if text("format")? != "mp4" {
            return Err(ColabError("Profil video Colab tidak didukung"));
        }
        // Shared UI transport fields (fit/proxy) are not Colab worker options.
        let engine = options
            .get("engine")
            .map(|value| value.as_str().ok_or(ColabError("Engine Colab tidak valid")))
            .transpose()?
            .unwrap_or("video-colab");
        let scale = text("scale")?;
        let (upscale_model, upscale_scale) = match (engine, scale) {
            ("video-colab", "2") => ("realesrgan-x2plus", 2),
            ("video-colab", "4") => ("nanovsr-644k", 4),
            _ => return Err(ColabError("Profil video Colab tidak didukung")),
        };
        let interpolation = match text("interpolation")? {
            "off" => "off",
            label => target_ratio(label).ok_or(ColabError("Pilihan interpolasi tidak didukung"))?,
        };
        let mute_audio = boolean("mute_audio")?;
        boolean("keep_drive_files")?;
        let local_suffix = text("suffix")?;
        if local_suffix.len() > 64
            || local_suffix.ends_with(['.', ' '])
            || local_suffix
                .chars()
                .any(|ch| ch.is_control() || "<>:\"/\\|?*".contains(ch))
        {
            return Err(ColabError("Suffix output tidak aman"));
        }
        Ok(Self {
            interpolation,
            mute_audio,
            local_suffix,
            upscale_model,
            upscale_scale,
        })
    }
}

fn target_ratio(label: &str) -> Option<&'static str> {
    match label {
        "23.976" => Some("24000/1001"),
        "24" => Some("24/1"),
        "25" => Some("25/1"),
        "29.97" => Some("30000/1001"),
        "30" => Some("30/1"),
        "48" => Some("48/1"),
        "50" => Some("50/1"),
        "59.94" => Some("60000/1001"),
        "60" => Some("60/1"),
        _ => None,
    }
}

fn path_occupied(path: &Path) -> Result<bool, ColabError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(ColabError("Nama output tidak dapat diperiksa")),
    }
}

pub(super) fn output_key(path: &Path) -> PathBuf {
    // Canonicalize the parent because reserved output files do not exist yet.
    // This also unifies normal, verbatim and directory-alias paths on Windows.
    let normalized = path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(path.file_name())
        .map(|(parent, name)| parent.join(name))
        .unwrap_or_else(|| path.to_path_buf());
    #[cfg(windows)]
    {
        PathBuf::from(normalized.to_string_lossy().to_lowercase())
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn choose_output(
    directory: &Path,
    stem: &str,
    suffix: &str,
    reserved: &HashSet<PathBuf>,
) -> Result<PathBuf, ColabError> {
    let reserved: HashSet<_> = reserved.iter().map(|path| output_key(path)).collect();
    for ordinal in 1u64.. {
        let number = if ordinal == 1 {
            String::new()
        } else {
            format!("-{ordinal}")
        };
        let name = format!("{stem}{suffix}{number}.mp4");
        // Leave room for the sibling .<name>.part used during verified download.
        if name.encode_utf16().count() + 6 > 255 {
            return Err(ColabError("Nama output terlalu panjang"));
        }
        let path = directory.join(&name);
        let is_reserved = reserved.contains(&output_key(&path));
        if !is_reserved
            && !path_occupied(&path)?
            && !path_occupied(&directory.join(format!(".{name}.part")))?
        {
            return Ok(path);
        }
    }
    Err(ColabError("Nama output yang tersedia tidak ditemukan"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colab::test_support::{probe_json, TestDir};
    use crate::engines::{video_colab::VideoColabEngine, Engine};
    use serde_json::{json, Value};
    use std::{fs, process::Command};

    struct Fixture {
        root: TestDir,
        input: PathBuf,
        output: PathBuf,
        options: EngineOptions,
        metadata: Value,
        reserved: HashSet<PathBuf>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = TestDir::new();
            let input = root.0.join("private clip & footage.mp4");
            let output = root.0.join("results");
            fs::write(&input, vec![0; 1_048_576]).unwrap();
            fs::create_dir(&output).unwrap();
            Self {
                root,
                input,
                output,
                options: [
                    ("format", json!("mp4")),
                    ("scale", json!("4")),
                    ("interpolation", json!("59.94")),
                    ("mute_audio", json!(false)),
                    ("suffix", json!("-colab")),
                    ("keep_drive_files", json!(false)),
                ]
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
                metadata: probe_json(),
                reserved: HashSet::new(),
            }
        }

        fn prepare(&mut self) -> Result<PreparedJob, ColabError> {
            let expected_input = fs::canonicalize(&self.input).ok();
            let run = |command: &mut Command| {
                assert_eq!(command.get_program(), "fixture-ffprobe.exe");
                let args: Vec<_> = command.get_args().collect();
                assert_eq!(
                    args.last().copied(),
                    expected_input.as_deref().map(Path::as_os_str)
                );
                assert!(
                    !args.contains(&std::ffi::OsStr::new("-select_streams")),
                    "audio must remain visible"
                );
                assert!(args.contains(&std::ffi::OsStr::new("json")));
                Ok(serde_json::to_vec(&self.metadata).unwrap())
            };
            prepare(
                &self.input,
                &self.output,
                &self.options,
                &VideoProbe {
                    executable: Path::new("fixture-ffprobe.exe"),
                    run: &run,
                },
                "2026-09-02T12:34:56Z".parse().unwrap(),
                &mut self.reserved,
            )
        }
    }

    // Exact wire contract catches extra private fields as well as wrong values.
    #[test]
    fn manifest_uses_schema_two_exact_ntsc_ratio_and_only_worker_fields() {
        let mut fixture = Fixture::new();
        let prepared = fixture.prepare().unwrap();
        assert_eq!(
            prepared.manifest_json,
            json!({
                "schema_version": 2, "worker_version": "0.2.0", "exchange_protocol": "drive-slots-v1",
                "job_id": prepared.job_id.to_string(), "created_at": "2026-09-02T12:34:56Z",
                "input": {"name": "input.mp4", "extension": "mp4", "size_bytes": 1_048_576,
                    "sha256": "30e14955ebf1352266dc2ff8067e68104607e750abb9d3b36582b8af909fcb58"},
                "source": {"width": 1920, "height": 1080, "duration_seconds": 60.0,
                    "time_base": "1/24000", "nominal_fps": "24000/1001", "has_audio": true},
                "upscale": {"model": "nanovsr-644k", "scale": 4},
                "interpolation": "60000/1001", "mute_audio": false,
                "output": {"format": "mp4", "video_codec": "h264", "suffix": "-colab"}
            })
        );
        assert_eq!(prepared.job_id.get_version_num(), 4);
        assert_eq!(prepared.source_media.rotation, 270);
        assert_eq!(
            prepared.input_sha256,
            "30e14955ebf1352266dc2ff8067e68104607e750abb9d3b36582b8af909fcb58"
        );
        assert_eq!(
            prepared.input_path,
            fs::canonicalize(&fixture.input).unwrap()
        );
        assert!(prepared.output_path.is_absolute());
        assert!(fixture.reserved.contains(&prepared.output_path));
        assert!(!prepared.output_path.exists());
    }

    #[test]
    fn all_supported_fps_labels_emit_exact_ratios() {
        let mut fixture = Fixture::new();
        fixture.metadata["streams"][1]["avg_frame_rate"] = json!("1/1");
        for (label, ratio) in [
            ("23.976", "24000/1001"),
            ("24", "24/1"),
            ("25", "25/1"),
            ("29.97", "30000/1001"),
            ("30", "30/1"),
            ("48", "48/1"),
            ("50", "50/1"),
            ("59.94", "60000/1001"),
            ("60", "60/1"),
        ] {
            fixture.options.insert("interpolation".into(), json!(label));
            assert_eq!(
                fixture.prepare().unwrap().manifest_json["interpolation"],
                ratio
            );
        }
    }

    #[test]
    fn rejects_equal_or_lower_target_using_exact_fraction_comparison() {
        for source in [
            "60000/1001",
            "120000/2002",
            "60/1",
            "18446744073709551615/1",
        ] {
            let mut fixture = Fixture::new();
            fixture.metadata["streams"][1]["avg_frame_rate"] = json!(source);
            assert!(fixture.prepare().unwrap_err().to_string().contains("FPS"));
            assert!(fixture.reserved.is_empty());
        }
        let mut fixture = Fixture::new();
        fixture.metadata["streams"][1]["avg_frame_rate"] = json!("24000/1001");
        fixture.options.insert("interpolation".into(), json!("24"));
        assert_eq!(
            fixture.prepare().unwrap().manifest_json["interpolation"],
            "24/1"
        );
    }

    #[test]
    fn off_preserves_source_fps_and_mute_does_not_erase_source_audio() {
        let mut fixture = Fixture::new();
        fixture.options.insert("interpolation".into(), json!("off"));
        fixture.options.insert("mute_audio".into(), json!(true));
        fixture
            .options
            .insert("keep_drive_files".into(), json!(true));
        fixture.metadata["streams"][1]["avg_frame_rate"] = json!("120/1");
        let prepared = fixture.prepare().unwrap();
        assert_eq!(prepared.manifest_json["interpolation"], "off");
        assert_eq!(prepared.manifest_json["mute_audio"], true);
        assert_eq!(prepared.manifest_json["source"]["has_audio"], true);
        assert!(prepared.manifest_json.get("keep_drive_files").is_none());
    }

    #[test]
    fn missing_wrong_type_or_unsupported_options_are_rejected() {
        let mut fixture = Fixture::new();
        let valid = fixture.options.clone();
        for key in [
            "format",
            "scale",
            "interpolation",
            "mute_audio",
            "suffix",
            "keep_drive_files",
        ] {
            fixture.options = valid.clone();
            fixture.options.remove(key);
            assert!(fixture.prepare().is_err(), "missing {key}");
            for bad in [Value::Null, json!([]), json!({}), json!(4)] {
                fixture.options = valid.clone();
                fixture.options.insert(key.into(), bad);
                assert!(fixture.prepare().is_err(), "wrong type for {key}");
            }
        }
        for (key, bad) in [
            ("format", json!("mkv")),
            ("scale", json!("3")),
            ("interpolation", json!("60000/1001")),
            ("interpolation", json!("61")),
            ("mute_audio", json!("false")),
            ("keep_drive_files", json!("true")),
            ("engine", json!("upscale-v1")),
        ] {
            fixture.options = valid.clone();
            fixture.options.insert(key.into(), bad);
            assert!(fixture.prepare().is_err(), "unsupported {key}");
        }
        fixture.options = valid;
        fixture.options.insert("interpolation".into(), json!("bad"));
        assert!(fixture.prepare().is_err());
        assert!(fixture.reserved.is_empty());
    }

    #[test]
    fn engine_defaults_and_shared_ui_fields_are_compatible() {
        let mut fixture = Fixture::new();
        fixture.options = VideoColabEngine::new()
            .options_schema()
            .into_iter()
            .map(|option| (option.id, option.default))
            .collect();
        fixture.options.extend([
            ("engine".into(), json!("video-colab")),
            ("fit".into(), json!("contain")),
            ("proxy_mode".into(), json!("direct")),
            ("proxy_list".into(), json!("private proxy")),
        ]);
        let prepared = fixture.prepare().unwrap();
        assert_eq!(prepared.manifest_json["interpolation"], "off");
        assert!(!prepared.manifest_json.to_string().contains("private"));
    }

    #[test]
    fn scale_selects_the_native_model_profile_without_a_model_option() {
        let mut fixture = Fixture::new();
        fixture.options.insert("scale".into(), json!("2"));
        let prepared = fixture.prepare().unwrap();
        assert_eq!(
            prepared.manifest_json["upscale"],
            json!({"model": "realesrgan-x2plus", "scale": 2})
        );

        fixture.options.insert("scale".into(), json!("4"));
        let prepared = fixture.prepare().unwrap();
        assert_eq!(
            prepared.manifest_json["upscale"],
            json!({"model": "nanovsr-644k", "scale": 4})
        );
    }

    #[test]
    fn safe_custom_suffix_only_changes_local_output_name() {
        let mut fixture = Fixture::new();
        fixture.options.insert("suffix".into(), json!("-final_v2"));
        let prepared = fixture.prepare().unwrap();
        assert_eq!(
            prepared.output_path.file_name().unwrap(),
            "private clip & footage-final_v2.mp4"
        );
        assert_eq!(prepared.manifest_json["output"]["suffix"], "-colab");
    }

    #[test]
    fn unsafe_suffixes_cannot_escape_or_alias_output_directory() {
        let mut fixture = Fixture::new();
        for suffix in [
            "../escape",
            "\\escape",
            ":stream",
            "?",
            "*",
            "\"",
            "<",
            ">",
            "|",
            "-bad\n",
            "-bad\0",
            "-bad.",
            "-bad ",
        ] {
            fixture.options.insert("suffix".into(), json!(suffix));
            assert!(fixture.prepare().is_err(), "unsafe suffix {suffix:?}");
        }
        fixture
            .options
            .insert("suffix".into(), json!("x".repeat(256)));
        assert!(fixture.prepare().is_err());
        assert!(fixture.reserved.is_empty());
    }

    #[test]
    fn collisions_include_existing_files_reserved_names_and_partial_downloads() {
        let mut fixture = Fixture::new();
        let existing = fixture.output.join("private clip & footage-colab.mp4");
        fs::write(&existing, b"preserve existing output").unwrap();
        let reserved = fs::canonicalize(&fixture.output)
            .unwrap()
            .join("private clip & footage-colab-2.mp4");
        fixture.reserved.insert(reserved);
        let partial = fixture
            .output
            .join(".private clip & footage-colab-3.mp4.part");
        fs::write(&partial, b"preserve existing partial download").unwrap();
        let first = fixture.prepare().unwrap();
        let second = fixture.prepare().unwrap();
        assert_eq!(
            first.output_path.file_name().unwrap(),
            "private clip & footage-colab-4.mp4"
        );
        assert_eq!(
            second.output_path.file_name().unwrap(),
            "private clip & footage-colab-5.mp4"
        );
        assert_ne!(first.job_id, second.job_id);
        assert_eq!(fs::read(existing).unwrap(), b"preserve existing output");
        assert_eq!(
            fs::read(partial).unwrap(),
            b"preserve existing partial download"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_reserved_names_are_case_insensitive() {
        let mut fixture = Fixture::new();
        let path = fs::canonicalize(&fixture.output)
            .unwrap()
            .join("PRIVATE CLIP & FOOTAGE-COLAB.MP4");
        fixture.reserved.insert(path);
        assert_eq!(
            fixture.prepare().unwrap().output_path.file_name().unwrap(),
            "private clip & footage-colab-2.mp4"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_reserved_names_match_regular_and_canonical_path_spellings() {
        let mut fixture = Fixture::new();
        fixture
            .reserved
            .insert(fixture.output.join("private clip & footage-colab.mp4"));
        assert_eq!(
            fixture.prepare().unwrap().output_path.file_name().unwrap(),
            "private clip & footage-colab-2.mp4"
        );
    }

    #[test]
    fn checksum_includes_bytes_after_the_first_one_mib_read() {
        use std::io::Write;
        let mut fixture = Fixture::new();
        fs::OpenOptions::new()
            .append(true)
            .open(&fixture.input)
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        let prepared = fixture.prepare().unwrap();
        assert_eq!(
            prepared.input_sha256,
            "3b6404689f6fb3b40515a0385428aa00378bf8008a3ba2ba2ea03a2209fc80b7"
        );
        assert_eq!(prepared.manifest_json["input"]["size_bytes"], 1_048_579);
    }

    #[test]
    fn invalid_files_and_missing_output_are_rejected_before_probe() {
        let mut fixture = Fixture::new();
        let run = |_: &mut Command| -> Result<Vec<u8>, ColabError> {
            panic!("invalid input reached FFprobe")
        };
        for input in [
            fixture.root.0.join("missing.mp4"),
            fixture.root.0.join("clip.gif"),
            fixture.root.0.clone(),
        ] {
            if input.extension().is_some_and(|ext| ext == "gif") {
                fs::write(&input, b"GIF").unwrap();
            }
            assert!(prepare(
                &input,
                &fixture.output,
                &fixture.options,
                &VideoProbe {
                    executable: Path::new("unused"),
                    run: &run
                },
                Utc::now(),
                &mut fixture.reserved
            )
            .is_err());
        }
        for size in [0, 104_857_601] {
            fs::File::create(&fixture.input)
                .unwrap()
                .set_len(size)
                .unwrap();
            assert!(prepare(
                &fixture.input,
                &fixture.output,
                &fixture.options,
                &VideoProbe {
                    executable: Path::new("unused"),
                    run: &run
                },
                Utc::now(),
                &mut fixture.reserved
            )
            .is_err());
        }
        fs::write(&fixture.input, b"abc").unwrap();
        for output in [fixture.root.0.join("missing"), fixture.input.clone()] {
            assert!(prepare(
                &fixture.input,
                &output,
                &fixture.options,
                &VideoProbe {
                    executable: Path::new("unused"),
                    run: &run
                },
                Utc::now(),
                &mut fixture.reserved
            )
            .is_err());
        }
        assert!(fixture.reserved.is_empty());
    }

    #[test]
    fn duration_limit_and_invalid_probe_do_not_reserve_output() {
        let mut fixture = Fixture::new();
        fixture.metadata["streams"][1]["duration"] = json!("60.001");
        assert!(fixture
            .prepare()
            .unwrap_err()
            .to_string()
            .contains("1 menit"));
        fixture.metadata = json!({});
        assert!(fixture.prepare().is_err());
        assert!(fixture.reserved.is_empty());
    }

    #[test]
    fn all_supported_extensions_normalize_remote_input_names() {
        let mut fixture = Fixture::new();
        for extension in ["MP4", "mov", "webm", "mkv", "m4v", "ts"] {
            let next = fixture.input.with_extension(extension);
            if next != fixture.input {
                fs::rename(&fixture.input, &next).unwrap();
            }
            fixture.input = next;
            let prepared = fixture.prepare().unwrap();
            assert_eq!(
                prepared.manifest_json["input"]["name"],
                format!("input.{}", extension.to_ascii_lowercase())
            );
        }
    }

    // Verifies the actual public Rust entry point and Python's strict parser,
    // using a generated video and the bundled FFmpeg/FFprobe executables.
    #[test]
    fn real_probe_manifest_is_accepted_by_python_worker() {
        use std::io::Write;
        use std::process::Stdio;

        let mut fixture = Fixture::new();
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let ffprobe = media::resolve_ffprobe(None, crate_dir).unwrap();
        let ffmpeg = ffprobe.with_file_name("ffmpeg.exe");
        let mut command = Command::new(ffmpeg);
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=32x24:rate=24000/1001:duration=0.5",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=1000:sample_rate=44100:duration=0.5",
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&fixture.input);
        media::run_probe_command(&mut command)
            .expect("generate representative video with bundled FFmpeg");
        fixture.options.insert("suffix".into(), json!("-custom"));
        let prepared = prepare_job(
            &fixture.input,
            &fixture.output,
            &fixture.options,
            &ffprobe,
            "2026-09-02T12:34:56Z".parse().unwrap(),
            &mut fixture.reserved,
        )
        .unwrap();
        assert_eq!(
            (prepared.source_media.width, prepared.source_media.height),
            (32, 24)
        );
        assert_eq!(prepared.source_media.nominal_fps, "24000/1001");
        assert!(prepared.source_media.has_audio);
        assert_eq!(
            prepared.source_media,
            media::probe_video(&ffprobe, &fixture.input).unwrap()
        );
        assert!(
            prepared.source_media.duration_seconds > 0.49
                && prepared.source_media.duration_seconds < 0.52
        );
        assert_eq!(
            prepared.output_path.file_name().unwrap(),
            "private clip & footage-custom.mp4"
        );

        let script = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
from fractions import Fraction
from xix_colab_worker.manifest import validate_manifest_dict
raw = json.load(sys.stdin)
manifest = validate_manifest_dict(raw, '0.2.0')
assert manifest.schema_version == 2
assert manifest.exchange_protocol == 'drive-slots-v1'
assert manifest.source.nominal_fps == Fraction(24000, 1001)
assert manifest.interpolation == Fraction(60000, 1001)
assert manifest.source.time_base > 0
assert manifest.output.suffix == '-colab'
print('worker 0.2 accepted the Rust manifest')
"#;
        let mut command = Command::new("python");
        command
            .args(["-B", "-c", script])
            .arg(crate_dir.join("../../colab/worker/src"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let mut python = command
            .spawn()
            .expect("Python required for worker contract verification");
        python
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&prepared.manifest_json).unwrap())
            .unwrap();
        let result = python.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().trim(),
            "worker 0.2 accepted the Rust manifest"
        );
    }
}
