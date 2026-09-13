use super::*;
use crate::colab::test_support::TestDir;
use std::fs;

const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn record(root: &Path) -> RemoteJobRecord {
    use crate::colab::{
        drive::{RemoteFiles, WorkspaceIds},
        media::SourceMedia,
        registry::ControlAction,
        remote_status::RemotePhase,
    };
    let job_id = Uuid::new_v4();
    RemoteJobRecord {
        job_id,
        local_input: root.join("input.mp4"),
        local_output: root.join("result.mp4"),
        input_sha256: ABC_SHA256.into(),
        source_media: SourceMedia {
            width: 32,
            height: 24,
            duration_seconds: 1.0,
            time_base: "1/24".into(),
            nominal_fps: "24/1".into(),
            has_audio: false,
            rotation: 0,
        },
        manifest_json: serde_json::json!({"schema_version":2,"worker_version":"0.2.0","exchange_protocol":"drive-slots-v1","job_id":job_id,"created_at":"2026-09-03T00:00:00Z","input":{"name":"input.mp4","extension":"mp4","size_bytes":3,"sha256":ABC_SHA256},"source":{"width":32,"height":24,"duration_seconds":1.0,"time_base":"1/24","nominal_fps":"24/1","has_audio":false},"upscale":{"model":"nanovsr-644k","scale":4},"interpolation":"off","mute_audio":false,"output":{"format":"mp4","video_codec":"h264","suffix":"-colab"}}),
        workspace_ids: WorkspaceIds {
            root_id: "root".into(),
            marker_id: "marker".into(),
            jobs_id: "jobs".into(),
        },
        remote_files: RemoteFiles {
            folder_id: "folder".into(),
            input_id: "input".into(),
            manifest_id: "manifest".into(),
            status_a_id: "a".into(),
            status_b_id: "b".into(),
            control_id: "control".into(),
            output_id: "output".into(),
        },
        upload_session: None,
        state: RemotePhase::Uploading,
        last_revision: 0,
        control_revision: 0,
        desired_control: ControlAction::Run,
        keep_drive_files: false,
        completed_output: None,
    }
}

#[test]
fn installation_identity_survives_reloading_and_is_persisted() {
    let dir = TestDir::new();
    let first = install_id(&dir.0).unwrap();
    assert!(!first.is_nil());
    assert_eq!(install_id(&dir.0).unwrap(), first);
    let stored = fs::read_to_string(dir.0.join("colab/desktop-install-id")).unwrap();
    assert_eq!(Uuid::parse_str(stored.trim()).unwrap(), first);
}

#[test]
fn concurrent_installation_initialization_returns_one_identity() {
    let dir = TestDir::new();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let root = dir.0.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                install_id(&root).unwrap()
            })
        })
        .collect();
    let identities: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert!(identities.iter().all(|id| *id == identities[0]));
    assert_eq!(install_id(&dir.0).unwrap(), identities[0]);
    assert_eq!(fs::read_dir(dir.0.join("colab")).unwrap().count(), 1);
}

#[test]
fn corrupt_installation_identity_is_never_replaced() {
    let dir = TestDir::new();
    fs::create_dir(dir.0.join("colab")).unwrap();
    let path = dir.0.join("colab/desktop-install-id");
    for bytes in [
        b"".as_slice(),
        b"not-a-uuid",
        b"00000000-0000-0000-0000-000000000000",
        &[0xff],
    ] {
        fs::write(&path, bytes).unwrap();
        assert!(install_id(&dir.0).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn resumed_input_must_still_match_immutable_manifest_bytes() {
    let dir = TestDir::new();
    let record = record(&dir.0);
    fs::write(&record.local_input, b"abc").unwrap();
    verify_input(&record).unwrap();
    for bytes in [b"abd".as_slice(), b"ab", b"abcd", b""] {
        fs::write(&record.local_input, bytes).unwrap();
        assert!(verify_input(&record).is_err());
        assert_eq!(fs::read(&record.local_input).unwrap(), bytes);
    }
}

#[test]
fn resumed_input_rejects_inconsistent_manifest_or_record() {
    let dir = TestDir::new();
    let original = record(&dir.0);
    fs::write(&original.local_input, b"abc").unwrap();
    let mut changed = original.clone();
    changed.manifest_json["input"]["size_bytes"] = serde_json::json!(2);
    assert!(verify_input(&changed).is_err());
    changed = original.clone();
    changed.manifest_json["input"]["sha256"] = serde_json::json!("0".repeat(64));
    assert!(verify_input(&changed).is_err());
    changed = original.clone();
    changed.input_sha256 = "0".repeat(64);
    assert!(verify_input(&changed).is_err());
    changed = original.clone();
    changed.job_id = Uuid::new_v4();
    assert!(verify_input(&changed).is_err());
    fs::remove_file(&original.local_input).unwrap();
    assert!(verify_input(&original).is_err());
    fs::create_dir(&original.local_input).unwrap();
    assert!(verify_input(&original).is_err());
}

fn media_tool(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("ffmpeg-runtime/bin")
        .join(format!("{name}.exe"))
}

fn run_ffmpeg(args: &[&std::ffi::OsStr]) {
    let mut command = std::process::Command::new(media_tool("ffmpeg"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let result = command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-n"])
        .args(args)
        .output()
        .expect("bundled FFmpeg must be present");
    assert!(
        result.status.success(),
        "FFmpeg fixture: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn fixture_hash(path: &Path) -> String {
    // Fixture bytes are hashed before any mutations. Metadata expectations below
    // are literals from the FFmpeg inputs, never obtained via verify_output.
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

fn video_fixture(
    root: &Path,
    audio: bool,
    rate: &str,
    duration: &str,
) -> (PathBuf, CompletedOutput) {
    let path = root.join(".result.mp4.part");
    let video = format!("testsrc2=size=32x24:rate={rate}:duration={duration}");
    let sound = format!("sine=frequency=1000:sample_rate=48000:duration={duration}");
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "-f".as_ref(),
        "lavfi".as_ref(),
        "-i".as_ref(),
        video.as_ref(),
    ];
    if audio {
        args.extend(["-f", "lavfi", "-i", &sound, "-c:a", "aac"].map(std::ffi::OsStr::new));
    } else {
        args.push("-an".as_ref());
    }
    // The bundled LGPL FFmpeg provides MPEG-4, not the GPL libx264 encoder.
    // Verification's contract checks media properties, not a codec field.
    args.extend([
        "-c:v".as_ref(),
        "mpeg4".as_ref(),
        "-pix_fmt".as_ref(),
        "yuv420p".as_ref(),
        "-f".as_ref(),
        "mp4".as_ref(),
        path.as_os_str(),
    ]);
    run_ffmpeg(&args);
    let expected = CompletedOutput {
        name: "output.mp4".into(),
        size_bytes: fs::metadata(&path).unwrap().len(),
        sha256: fixture_hash(&path),
        width: 32,
        height: 24,
        duration_seconds: duration.parse().unwrap(),
        fps: rate.into(),
        has_audio: audio,
    };
    (path, expected)
}

#[test]
fn verifies_actual_video_and_rejects_each_changed_output_metadata_field() {
    let dir = TestDir::new();
    let (path, expected) = video_fixture(&dir.0, true, "24/1", "1");
    let ffprobe = media_tool("ffprobe");
    verify_output(&ffprobe, &path, &expected, 0).unwrap();
    for field in 0..8 {
        let mut wrong = expected.clone();
        match field {
            0 => wrong.size_bytes += 1,
            1 => wrong.sha256 = "0".repeat(64),
            2 => wrong.width += 1,
            3 => wrong.height += 1,
            4 => wrong.duration_seconds += 0.01,
            5 => wrong.fps = "24000/1001".into(),
            6 => wrong.has_audio = false,
            _ => wrong.name = "other.mp4".into(),
        }
        assert!(
            verify_output(&ffprobe, &path, &wrong, 0).is_err(),
            "field {field}"
        );
    }
    assert!(verify_output(&ffprobe, &path, &expected, 90).is_err());
    assert!(verify_output(&dir.0.join("missing-ffprobe.exe"), &path, &expected, 0).is_err());
    assert_eq!(fixture_hash(&path), expected.sha256);
}

#[test]
fn rotated_output_is_verified_against_source_rotation() {
    let dir = TestDir::new();
    let (path, mut expected) = video_fixture(&dir.0, false, "24/1", "1");
    let rotated = dir.0.join("rotated.mp4");
    run_ffmpeg(&[
        "-display_rotation".as_ref(),
        "90".as_ref(),
        "-i".as_ref(),
        path.as_os_str(),
        "-c".as_ref(),
        "copy".as_ref(),
        rotated.as_os_str(),
    ]);
    expected.sha256 = fixture_hash(&rotated);
    expected.size_bytes = fs::metadata(&rotated).unwrap().len();
    let ffprobe = media_tool("ffprobe");
    assert_eq!(
        crate::colab::media::probe_video(&ffprobe, &rotated)
            .unwrap()
            .rotation,
        90
    );
    verify_output(&ffprobe, &rotated, &expected, 90).unwrap();
    assert!(verify_output(&ffprobe, &rotated, &expected, 0).is_err());
}

#[test]
fn output_can_exceed_input_duration_limit_by_a_frame_and_fps_is_rational() {
    let dir = TestDir::new();
    let (path, mut expected) = video_fixture(&dir.0, false, "24/1", "60.04");
    // 1,441 frames at 24fps; the probe reports seconds to six decimal places.
    expected.duration_seconds = 60.041667;
    expected.fps = "48/2".into();
    verify_output(&media_tool("ffprobe"), &path, &expected, 0).unwrap();
}

#[test]
fn publishes_verified_part_without_overwriting_and_recovers_after_promotion() {
    let dir = TestDir::new();
    let (part, expected) = video_fixture(&dir.0, false, "24/1", "1");
    let dest = dir.0.join("result.mp4");
    let probe = media_tool("ffprobe");
    assert_eq!(part_path(&dest), part);
    promote_output(&probe, &part, &dest, &expected, 0).unwrap();
    assert!(!part.exists());
    assert_eq!(fixture_hash(&dest), expected.sha256);
    promote_output(&probe, &part, &dest, &expected, 0).unwrap();
    fs::write(&part, b"new incoming partial bytes").unwrap();
    let before = fs::read(&dest).unwrap();
    let mut other = expected.clone();
    other.sha256 = "0".repeat(64);
    assert!(promote_output(&probe, &part, &dest, &other, 0).is_err());
    assert_eq!(fs::read(&dest).unwrap(), before);
    assert!(part.exists());
}
#[test]
fn corrupted_part_is_retained_and_unrelated_final_file_is_never_replaced() {
    let dir = TestDir::new();
    let (part, expected) = video_fixture(&dir.0, false, "24/1", "1");
    let dest = dir.0.join("result.mp4");
    let probe = media_tool("ffprobe");
    fs::write(&dest, b"owner's earlier video").unwrap();
    assert!(promote_output(&probe, &part, &dest, &expected, 0).is_err());
    assert_eq!(fs::read(&dest).unwrap(), b"owner's earlier video");
    assert!(part.exists());
    let another = dir.0.join("fresh.mp4");
    fs::write(&part, b"broken").unwrap();
    assert!(promote_output(&probe, &part, &another, &expected, 0).is_err());
    assert!(!another.exists());
    assert_eq!(fs::read(&part).unwrap(), b"broken");
}

#[test]
fn partial_download_uses_a_hidden_sibling_and_never_claims_the_legacy_name() {
    let dir = TestDir::new();
    let dest = dir.0.join("result.mp4");
    let legacy = dir.0.join("result.mp4.part");
    fs::write(&legacy, b"owner data").unwrap();
    assert_eq!(part_path(&dest), dir.0.join(".result.mp4.part"));
    assert_eq!(fs::read(legacy).unwrap(), b"owner data");
}

#[test]
fn download_destination_rejects_a_link_in_its_parent_chain() {
    let dir = TestDir::new();
    let real = dir.0.join("real");
    let linked = dir.0.join("linked");
    fs::create_dir(&real).unwrap();
    #[cfg(windows)]
    let linked_ok = std::os::windows::fs::symlink_dir(&real, &linked).is_ok();
    #[cfg(unix)]
    let linked_ok = std::os::unix::fs::symlink(&real, &linked).is_ok();
    if !linked_ok {
        return;
    }
    assert!(validate_download_path(&linked.join("result.mp4.part")).is_err());
    assert!(validate_download_path(&real.join("result.mp4.part")).is_ok());
}
