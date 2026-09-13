use super::ColabError;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const MAX_INPUT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_DURATION_SECONDS: f64 = 60.0;
const MAX_PROBE_BYTES: usize = 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn run_probe_command(command: &mut Command) -> Result<Vec<u8>, ColabError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|_| ColabError("FFprobe tidak dapat dijalankan"))?;
    let mut stdout = child.stdout.take().expect("piped FFprobe stdout");
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = match std::thread::Builder::new()
        .name("colab-ffprobe".into())
        .spawn(move || {
            let result = (|| {
                let mut bytes = Vec::with_capacity(MAX_PROBE_BYTES);
                stdout
                    .by_ref()
                    .take(MAX_PROBE_BYTES as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|_| ColabError("Metadata FFprobe tidak dapat dibaca"))?;
                // Read one overflow byte separately; the captured JSON never exceeds 1 MiB.
                let mut overflow = [0];
                if stdout
                    .read(&mut overflow)
                    .map_err(|_| ColabError("Metadata FFprobe tidak dapat dibaca"))?
                    != 0
                {
                    return Err(ColabError("Metadata FFprobe melebihi 1 MiB"));
                }
                Ok(bytes)
            })();
            let _ = sender.send(result);
        }) {
        Ok(reader) => reader,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ColabError(
                "Pembaca metadata FFprobe tidak dapat dijalankan",
            ));
        }
    };
    let result = (|| {
        let mut output = None;
        loop {
            if started.elapsed() >= PROBE_TIMEOUT {
                return Err(ColabError("FFprobe melewati batas waktu 15 detik"));
            }
            match receiver.try_recv() {
                Ok(result) => output = Some(result?),
                Err(mpsc::TryRecvError::Disconnected) if output.is_none() => {
                    return Err(ColabError("Pembacaan metadata FFprobe terputus"));
                }
                _ => {}
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|_| ColabError("Status FFprobe tidak dapat dibaca"))?
            {
                if !status.success() {
                    return Err(ColabError("FFprobe gagal membaca video"));
                }
                if let Some(bytes) = output.take() {
                    return Ok(bytes);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = reader.join();
    result
}

/// Only executing the subprocess is replaceable; metadata parsing remains real.
pub(super) struct VideoProbe<'a> {
    pub executable: &'a Path,
    pub run: &'a dyn Fn(&mut Command) -> Result<Vec<u8>, ColabError>,
}

impl VideoProbe<'_> {
    pub(super) fn read(&self, input: &Path) -> Result<SourceMedia, ColabError> {
        let mut command = Command::new(self.executable);
        command.args([
            "-v", "error", "-show_entries",
            "stream=codec_type,width,height,duration,time_base,avg_frame_rate:stream_disposition=attached_pic:stream_tags=rotate:stream_side_data=rotation:format=duration",
            "-of", "json", "-i",
        ]).arg(input);
        parse_probe(&(self.run)(&mut command)?)
    }
}

pub fn probe_video(ffprobe: &Path, input: &Path) -> Result<SourceMedia, ColabError> {
    VideoProbe {
        executable: ffprobe,
        run: &run_probe_command,
    }
    .read(input)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMedia {
    pub width: u32,
    pub height: u32,
    pub duration_seconds: f64,
    pub time_base: String,
    pub nominal_fps: String,
    pub has_audio: bool,
    pub rotation: i32,
}

pub fn validate_input_limits(size: u64, duration: f64) -> Result<(), ColabError> {
    validate_input_size(size)?;
    if !duration.is_finite() || duration <= 0.0 {
        return Err(ColabError("Durasi video harus positif dan valid"));
    }
    if duration > MAX_DURATION_SECONDS {
        return Err(ColabError("Video melebihi batas 1 menit"));
    }
    Ok(())
}

pub(super) fn validate_input_size(size: u64) -> Result<(), ColabError> {
    if size == 0 {
        return Err(ColabError("Video input kosong"));
    }
    if size > MAX_INPUT_BYTES {
        return Err(ColabError("Video melebihi batas 100 MB"));
    }
    Ok(())
}

pub fn resolve_ffprobe(resource_root: Option<&Path>, cwd: &Path) -> Result<PathBuf, ColabError> {
    let roots = resource_root.into_iter().map(Path::to_path_buf).chain([
        cwd.join("src-tauri"),
        cwd.to_path_buf(),
        cwd.join("XIX-Upscaler/src-tauri"),
    ]);
    for root in roots {
        let executable = root.join("ffmpeg-runtime/bin/ffprobe.exe");
        if executable.is_file() {
            return executable
                .canonicalize()
                .map_err(|_| ColabError("Lokasi FFprobe tidak dapat dibaca"));
        }
    }
    Err(ColabError("FFprobe bawaan tidak ditemukan"))
}

#[derive(Deserialize)]
struct ProbeDocument {
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: ProbeFormat,
}

#[derive(Deserialize, Default)]
struct ProbeFormat {
    duration: Option<String>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: String,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<String>,
    time_base: Option<String>,
    avg_frame_rate: Option<String>,
    #[serde(default)]
    disposition: Disposition,
    #[serde(default)]
    tags: HashMap<String, String>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}

#[derive(Deserialize, Default)]
struct Disposition {
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Deserialize)]
struct SideData {
    rotation: Option<i64>,
}

pub(super) fn parse_ratio(value: &str) -> Result<(u64, u64), ColabError> {
    let parse = |part: &str| -> Option<u64> {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        part.parse::<u64>().ok().filter(|number| *number > 0)
    };
    let (numerator, denominator) = value
        .split_once('/')
        .ok_or(ColabError("Rasio metadata video tidak valid"))?;
    Ok((
        parse(numerator).ok_or(ColabError("Rasio metadata video tidak valid"))?,
        parse(denominator).ok_or(ColabError("Rasio metadata video tidak valid"))?,
    ))
}

fn parse_probe(bytes: &[u8]) -> Result<SourceMedia, ColabError> {
    if bytes.len() > MAX_PROBE_BYTES {
        return Err(ColabError("Metadata FFprobe melebihi 1 MiB"));
    }
    let document: ProbeDocument =
        serde_json::from_slice(bytes).map_err(|_| ColabError("Metadata FFprobe tidak valid"))?;
    let mut videos = document
        .streams
        .iter()
        .filter(|stream| stream.codec_type == "video" && stream.disposition.attached_pic == 0);
    let video = videos
        .next()
        .ok_or(ColabError("Video utama tidak ditemukan"))?;
    if videos.next().is_some() {
        return Err(ColabError(
            "Input harus memiliki tepat satu stream video utama",
        ));
    }
    let width = video
        .width
        .filter(|value| *value > 0)
        .ok_or(ColabError("Lebar video tidak valid"))?;
    let height = video
        .height
        .filter(|value| *value > 0)
        .ok_or(ColabError("Tinggi video tidak valid"))?;
    let nominal_fps = video
        .avg_frame_rate
        .as_deref()
        .ok_or(ColabError("FPS video tidak tersedia"))?;
    parse_ratio(nominal_fps)?;
    let time_base = video
        .time_base
        .as_deref()
        .ok_or(ColabError("Time base video tidak tersedia"))?;
    parse_ratio(time_base)?;
    let duration = video
        .duration
        .as_deref()
        .filter(|value| *value != "N/A")
        .or(document.format.duration.as_deref())
        .ok_or(ColabError("Durasi video tidak tersedia"))?;
    let duration_seconds = duration
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or(ColabError("Durasi video tidak valid"))?;
    let mut rotation = match video.tags.get("rotate") {
        Some(value) => value
            .parse::<i64>()
            .map_err(|_| ColabError("Rotasi video tidak valid"))?,
        None => 0,
    };
    for data in &video.side_data_list {
        if let Some(value) = data.rotation {
            rotation = value;
        }
    }
    Ok(SourceMedia {
        width,
        height,
        duration_seconds,
        time_base: time_base.into(),
        nominal_fps: nominal_fps.into(),
        has_audio: document
            .streams
            .iter()
            .any(|stream| stream.codec_type == "audio"),
        rotation: rotation.rem_euclid(360) as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colab::test_support::{probe_json, TestDir};
    use serde_json::{json, Value};
    use std::fs;

    fn parse(value: &Value) -> Result<SourceMedia, ColabError> {
        parse_probe(&serde_json::to_vec(value).unwrap())
    }

    // Catches off-by-one limits and invalid floats bypassing comparisons.
    #[test]
    fn input_limits_accept_exact_boundaries_and_reject_one_over() {
        validate_input_limits(104_857_600, 60.0).unwrap();
        assert!(validate_input_limits(104_857_601, 60.0)
            .unwrap_err()
            .to_string()
            .contains("100 MB"));
        assert!(validate_input_limits(1, 60.001)
            .unwrap_err()
            .to_string()
            .contains("1 menit"));
        for (size, duration) in [
            (0, 1.0),
            (1, 0.0),
            (1, -1.0),
            (1, f64::NAN),
            (1, f64::INFINITY),
        ] {
            assert!(validate_input_limits(size, duration).is_err());
        }
    }

    // Audio must not be lost by selecting only video at the subprocess boundary.
    #[test]
    fn parses_one_video_exact_ratios_audio_and_side_data_rotation() {
        let media = parse(&probe_json()).unwrap();
        assert_eq!((media.width, media.height), (1920, 1080));
        assert_eq!(media.duration_seconds, 60.0);
        assert_eq!(media.nominal_fps, "24000/1001");
        assert_eq!(media.time_base, "1/24000");
        assert!(media.has_audio);
        assert_eq!(media.rotation, 270);
    }

    #[test]
    fn parses_tag_rotation_silent_video_and_container_duration_fallback() {
        let mut raw = probe_json();
        raw["streams"].as_array_mut().unwrap().remove(0);
        let video = raw["streams"][0].as_object_mut().unwrap();
        video.remove("duration");
        video.remove("side_data_list");
        let media = parse(&raw).unwrap();
        assert_eq!(media.rotation, 90);
        assert_eq!(media.duration_seconds, 60.0);
        assert!(!media.has_audio);
        raw["streams"][0]["duration"] = json!("N/A");
        raw["streams"][0].as_object_mut().unwrap().remove("tags");
        assert_eq!(parse(&raw).unwrap().rotation, 0);
    }

    #[test]
    fn rejects_missing_or_ambiguous_video_streams_and_ignores_cover_art() {
        let mut raw = probe_json();
        let video = raw["streams"][1].clone();
        raw["streams"].as_array_mut().unwrap().push(video);
        assert!(parse(&raw).is_err());
        raw["streams"][2]["disposition"]["attached_pic"] = json!(1);
        raw["streams"][2]["width"] = json!(32);
        assert_eq!(parse(&raw).unwrap().width, 1920);
        raw["streams"][1]["disposition"]["attached_pic"] = json!(1);
        assert!(parse(&raw).is_err());
        raw["streams"] = json!([]);
        assert!(parse(&raw).is_err());
    }

    #[test]
    fn rejects_invalid_metadata_instead_of_guessing_fps_or_dimensions() {
        for (key, value) in [
            ("avg_frame_rate", json!("0/0")),
            ("avg_frame_rate", json!("0/1")),
            ("avg_frame_rate", json!("-24/1")),
            ("avg_frame_rate", json!(24)),
            ("avg_frame_rate", Value::Null),
            ("avg_frame_rate", json!("NaN")),
            ("avg_frame_rate", json!("18446744073709551616/1")),
            ("time_base", json!("1/0")),
            ("time_base", Value::Null),
            ("width", json!(0)),
            ("width", json!("1920")),
            ("height", json!(-1)),
            ("height", json!(true)),
            ("duration", json!("0")),
            ("duration", json!("NaN")),
            ("duration", json!("inf")),
            ("tags", json!({"rotate": "bad"})),
            ("side_data_list", json!([{"rotation": 1.5}])),
        ] {
            let mut raw = probe_json();
            raw["streams"][1][key] = value;
            assert!(parse(&raw).is_err(), "accepted invalid {key}: {raw}");
        }
        for bytes in [
            b"{".as_slice(),
            b"null",
            b"[]",
            b"{\"streams\":null}",
            &[255],
        ] {
            assert!(parse_probe(bytes).is_err());
        }
        assert!(parse_probe(&vec![b' '; 1_048_577]).is_err());
    }

    #[test]
    fn probe_json_accepts_exactly_one_mib_but_not_one_byte_more() {
        let mut bytes = serde_json::to_vec(&probe_json()).unwrap();
        bytes.resize(1_048_576, b' ');
        assert_eq!(parse_probe(&bytes).unwrap().nominal_fps, "24000/1001");
        bytes.push(b' ');
        assert!(parse_probe(&bytes)
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
    }

    #[test]
    fn resolver_prefers_packaged_then_supported_development_layouts() {
        let root = TestDir::new();
        let resource = root.0.join("packaged");
        let packaged = resource.join("ffmpeg-runtime/bin/ffprobe.exe");
        let dev = root.0.join("src-tauri/ffmpeg-runtime/bin/ffprobe.exe");
        for path in [&packaged, &dev] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"fixture").unwrap();
        }
        assert_eq!(
            resolve_ffprobe(Some(&resource), &root.0).unwrap(),
            fs::canonicalize(&packaged).unwrap()
        );
        fs::remove_file(&packaged).unwrap();
        assert_eq!(
            resolve_ffprobe(Some(&resource), &root.0).unwrap(),
            fs::canonicalize(&dev).unwrap()
        );
        assert_eq!(
            resolve_ffprobe(None, &root.0.join("src-tauri")).unwrap(),
            fs::canonicalize(&dev).unwrap()
        );
        fs::remove_file(&dev).unwrap();
        let workspace = root
            .0
            .join("XIX-Upscaler/src-tauri/ffmpeg-runtime/bin/ffprobe.exe");
        fs::create_dir_all(workspace.parent().unwrap()).unwrap();
        fs::write(&workspace, b"fixture").unwrap();
        assert_eq!(
            resolve_ffprobe(None, &root.0).unwrap(),
            fs::canonicalize(&workspace).unwrap()
        );
        fs::remove_file(&workspace).unwrap();
        fs::create_dir(&workspace).unwrap();
        assert!(resolve_ffprobe(None, &root.0).is_err());
    }

    // An actual child process tests the timeout, exit status and pipe handling.
    // It is this unit-test executable, never the desktop application.
    fn child(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "colab::media::tests::subprocess_fixture",
                "--nocapture",
            ])
            .env("XIX_COLAB_TEST_CHILD", mode);
        command
    }

    #[test]
    fn subprocess_fixture() {
        use std::io::Write;
        let Ok(mode) = std::env::var("XIX_COLAB_TEST_CHILD") else {
            return;
        };
        match mode.as_str() {
            "timeout" => std::thread::sleep(std::time::Duration::from_secs(60)),
            "overflow" => {
                let _ = std::io::stdout().write_all(&vec![b'x'; 1_048_577]);
            }
            "stderr" => {
                std::io::stderr().write_all(&vec![b'x'; 2_097_152]).unwrap();
                std::io::stdout().write_all(b"probe completed").unwrap();
            }
            "failure" => {
                eprintln!("private/local/path.mp4");
                std::process::exit(7);
            }
            _ => panic!("unknown subprocess fixture"),
        }
    }

    #[test]
    fn probe_subprocess_stops_after_fifteen_seconds() {
        let start = std::time::Instant::now();
        let error = run_probe_command(&mut child("timeout")).unwrap_err();
        assert!(error.to_string().contains("15 detik"));
        assert!(start.elapsed() >= std::time::Duration::from_secs(15));
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn probe_subprocess_caps_stdout_and_does_not_block_on_stderr() {
        assert!(run_probe_command(&mut child("overflow"))
            .unwrap_err()
            .to_string()
            .contains("1 MiB"));
        let output = run_probe_command(&mut child("stderr")).unwrap();
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("probe completed"));
    }

    #[test]
    fn probe_subprocess_rejects_failed_and_missing_executables_without_leaking_diagnostics() {
        let error = run_probe_command(&mut child("failure")).unwrap_err();
        assert!(!error.to_string().contains("private"));
        assert!(run_probe_command(&mut Command::new("missing-xix-ffprobe.exe")).is_err());
    }
}
