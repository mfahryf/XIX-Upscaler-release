use super::super::{auth::AuthError, test_support::TestDir};
use super::*;
use futures_util::future::BoxFuture;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    thread,
    time::Duration,
};

const INSTALL: &str = "11111111-1111-4111-8111-111111111111";
const JOB: &str = "22222222-2222-4222-8222-222222222222";

#[test]
fn input_metadata_accepts_drive_detected_video_mime() {
    let actual = json!({
        "trashed": false,
        "id": "input-id",
        "name": "input.mp4",
        "mimeType": "video/mp4",
        "appProperties": {"xix_kind": "input", "xix_version": "1"},
        "parents": ["folder-id"]
    });
    let expected = json!({
        "id": "input-id",
        "name": "input.mp4",
        "mimeType": "application/octet-stream",
        "appProperties": {"xix_kind": "input", "xix_version": "1"},
        "parents": ["folder-id"]
    });

    assert!(check_metadata(&actual, &expected).is_ok());
}

#[test]
fn non_input_metadata_keeps_mime_strict() {
    let actual = json!({
        "trashed": false,
        "id": "output-id",
        "name": "output.mp4",
        "mimeType": "video/webm",
        "appProperties": {"xix_kind": "output", "xix_version": "1"},
        "parents": ["folder-id"]
    });
    let expected = json!({
        "id": "output-id",
        "name": "output.mp4",
        "mimeType": "video/mp4",
        "appProperties": {"xix_kind": "output", "xix_version": "1"},
        "parents": ["folder-id"]
    });

    assert_eq!(check_metadata(&actual, &expected), Err(DriveError::Conflict));
}

#[derive(Default)]
struct Tokens {
    invalidations: AtomicUsize,
}
impl AccessTokenProvider for Tokens {
    fn access_token(&self) -> BoxFuture<'_, Result<String, AuthError>> {
        Box::pin(async move {
            Ok(format!(
                "private-token-{}",
                self.invalidations.load(Ordering::SeqCst)
            ))
        })
    }
    fn invalidate_access_token(&self) -> BoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            self.invalidations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    disconnect: bool,
    declared_length: Option<usize>,
}
impl Response {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: vec![],
            disconnect: false,
            declared_length: None,
        }
    }
    fn json(mut self, value: Value) -> Self {
        self.body = serde_json::to_vec(&value).unwrap();
        self
    }
    fn bytes(mut self, value: &[u8]) -> Self {
        self.body = value.to_vec();
        self
    }
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

struct Server {
    origin: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(responses: Vec<Response>) -> Self {
        let mut responses = std::collections::VecDeque::from(responses);
        Self::handler(move |_| responses.pop_front().unwrap_or(Response::new(599)))
    }
    fn handler(mut handler: impl FnMut(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("test listener: {error}"),
                };
                // Windows inherits the listener's nonblocking flag on accept.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut raw = Vec::new();
                let header_end = loop {
                    let mut block = [0; 8192];
                    let n = stream.read(&mut block).unwrap();
                    assert!(n > 0, "request ended before headers");
                    raw.extend_from_slice(&block[..n]);
                    if let Some(index) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let header = String::from_utf8(raw[..header_end].to_vec()).unwrap();
                let mut lines = header.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_string();
                let target = first.next().unwrap().to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let length: usize = headers
                    .get("content-length")
                    .map(|v| v.parse().unwrap())
                    .unwrap_or(0);
                while raw.len() < header_end + length {
                    let mut block = [0; 65536];
                    let n = stream.read(&mut block).unwrap();
                    assert!(n > 0, "request ended before body");
                    raw.extend_from_slice(&block[..n]);
                }
                let request = Request {
                    method,
                    target,
                    headers,
                    body: raw[header_end..header_end + length].to_vec(),
                };
                let response = handler(&request);
                captured.lock().unwrap().push(request);
                if response.disconnect {
                    continue;
                }
                let mut headers = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    response.status,
                    response.declared_length.unwrap_or(response.body.len())
                );
                for (name, value) in response.headers {
                    headers.push_str(&format!("{name}: {value}\r\n"));
                }
                headers.push_str("\r\n");
                let _ = stream
                    .write_all(headers.as_bytes())
                    .and_then(|_| stream.write_all(&response.body));
            }
        });
        Self {
            origin,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn client(&self) -> DriveClient {
        self.client_with(Arc::new(Tokens::default()))
    }
    fn client_with(&self, tokens: Arc<Tokens>) -> DriveClient {
        DriveClient::with_endpoints(
            tokens,
            Uuid::parse_str(INSTALL).unwrap(),
            DriveEndpoints::loopback(&self.origin).unwrap(),
        )
        .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.worker.take().unwrap().join();
        if !thread::panicking() {
            assert!(result.is_ok(), "test HTTP server failed");
        }
    }
}

fn progress() -> TransferProgress {
    Arc::new(|_, _| {})
}
fn sessions() -> SessionSink {
    Arc::new(|_| Ok(()))
}

#[test]
fn account_label_is_masked_and_diagnostics_never_contain_identity() {
    let user = DriveUser {
        display_name: Some("Private Name".into()),
        email_address: Some("fahry@example.com".into()),
        permission_id: Some("private-permission".into()),
    };
    assert_eq!(user.masked_email().as_deref(), Some("fa***@example.com"));
    let debug = format!("{user:?}");
    for secret in ["Private Name", "fahry", "private-permission"] {
        assert!(!debug.contains(secret));
    }
}

#[tokio::test]
async fn reservation_returns_all_ids_without_creating_any_file() {
    let server = Server::new(vec![Response::new(200).json(json!({"kind":"drive#generatedIds","space":"drive","ids":["folder","input","manifest","a","b","control","output"]}))]);
    let files = server.client().reserve_job_files().await.unwrap();
    assert_eq!(
        serde_json::to_value(files).unwrap(),
        json!({"folder_id":"folder","input_id":"input","manifest_id":"manifest","status_a_id":"a","status_b_id":"b","control_id":"control","output_id":"output"})
    );
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0]
        .target
        .starts_with("/drive/v3/files/generateIds?"));
    assert!(requests[0].target.contains("count=7"));
}

#[tokio::test]
async fn account_request_refreshes_once_and_returns_in_memory_identity() {
    let server = Server::new(vec![Response::new(401), Response::new(200).json(json!({"user":{"displayName":"Name","emailAddress":"fahry@example.com","permissionId":"opaque"}}))]);
    let tokens = Arc::new(Tokens::default());
    let user = server
        .client_with(tokens.clone())
        .about_user()
        .await
        .unwrap();
    assert_eq!(user.masked_email().as_deref(), Some("fa***@example.com"));
    assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1);
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer private-token-0"
    );
    assert_eq!(
        requests[1].headers["authorization"],
        "Bearer private-token-1"
    );
    let url = Url::parse(&format!(
        "{}{}",
        server.origin.trim_end_matches('/'),
        requests[1].target
    ))
    .unwrap();
    assert_eq!(
        url.query_pairs().find(|(k, _)| k == "fields").unwrap().1,
        "user(displayName,emailAddress,permissionId)"
    );
}

#[tokio::test]
async fn second_unauthorized_stops_without_leaking_server_payload() {
    let server = Server::new(vec![
        Response::new(401).bytes(b"private-token-secret"),
        Response::new(401).bytes(b"private-token-secret"),
    ]);
    let tokens = Arc::new(Tokens::default());
    let error = server
        .client_with(tokens.clone())
        .read_json("a")
        .await
        .unwrap_err();
    assert_eq!(error, DriveError::Authentication);
    assert_eq!(tokens.invalidations.load(Ordering::SeqCst), 1);
    assert!(!format!("{error} {error:?}").contains("private-token-secret"));
}

#[tokio::test]
async fn transient_http_failures_retry_but_permission_and_quota_do_not() {
    for status in [408, 429, 500, 503] {
        let server = Server::new(vec![
            Response::new(status),
            Response::new(200).bytes(b"{\"ok\":true}"),
        ]);
        assert_eq!(
            server.client().read_json("a").await.unwrap(),
            b"{\"ok\":true}"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
    for (reason, expected) in [
        ("insufficientFilePermissions", DriveError::Permission),
        ("storageQuotaExceeded", DriveError::Quota),
        ("accessNotConfigured", DriveError::ApiDisabled),
    ] {
        let server = Server::new(vec![Response::new(403).json(json!({"error":{"code":403,"message":"secret-path","errors":[{"domain":"global","reason":reason,"message":"secret-path"}]}}))]);
        assert_eq!(server.client().read_json("a").await.unwrap_err(), expected);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn transient_failures_stop_after_bounded_attempts() {
    let server = Server::handler(|_| Response::new(503));
    assert_eq!(
        server.client().read_json("a").await.unwrap_err(),
        DriveError::RetryExhausted
    );
    assert_eq!(server.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn redirects_are_not_followed_with_credentials() {
    let foreign = Server::new(vec![Response::new(200)]);
    let server = Server::new(vec![Response::new(307).header("Location", &foreign.origin)]);
    assert_eq!(
        server.client().read_json("a").await.unwrap_err(),
        DriveError::UnsafeUrl
    );
    assert!(foreign.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unsafe_file_ids_are_rejected_before_sending_credentials() {
    let server = Server::new(vec![]);
    for id in ["", "../about", "a?fields=*", "a/b", "a%2fb", "root"] {
        assert_eq!(
            server.client().read_json(id).await.unwrap_err(),
            DriveError::InvalidId
        );
    }
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn malformed_reservation_never_returns_partial_or_repeated_ids() {
    for ids in [
        json!(["a"]),
        json!(["a", "b", "c", "d", "e", "f", "f"]),
        json!(["a", "b", "c", "d", "e", "f", "../g"]),
    ] {
        let server =
            Server::new(vec![Response::new(200).json(
                json!({"kind":"drive#generatedIds","space":"drive","ids":ids}),
            )]);
        assert!(server.client().reserve_job_files().await.is_err());
    }
}

fn workspace_ids() -> WorkspaceIds {
    WorkspaceIds {
        root_id: "workspace".into(),
        marker_id: "marker".into(),
        jobs_id: "jobs".into(),
    }
}
fn remote_files() -> RemoteFiles {
    RemoteFiles {
        folder_id: "folder".into(),
        input_id: "input".into(),
        manifest_id: "manifest".into(),
        status_a_id: "a".into(),
        status_b_id: "b".into(),
        control_id: "control".into(),
        output_id: "output".into(),
    }
}
fn job() -> PreparedJob {
    PreparedJob {
        job_id: Uuid::parse_str(JOB).unwrap(),
        input_path: "unused.mp4".into(),
        output_path: "unused-output.mp4".into(),
        input_sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        source_media: super::super::media::SourceMedia {
            width: 16,
            height: 16,
            duration_seconds: 1.0,
            time_base: "1/24".into(),
            nominal_fps: "24/1".into(),
            has_audio: false,
            rotation: 0,
        },
        manifest_json: json!({"schema_version":2,"worker_version":"0.2.0","exchange_protocol":"drive-slots-v1","job_id":JOB,"created_at":"2026-09-02T12:00:00Z","input":{"name":"input.mp4","extension":"mp4","size_bytes":3,"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"},"source":{"width":16,"height":16,"duration_seconds":1.0,"time_base":"1/24","nominal_fps":"24/1","has_audio":false},"upscale":{"model":"nanovsr-644k","scale":4},"interpolation":"off","mute_audio":false,"output":{"format":"mp4","video_codec":"h264","suffix":"-colab"}}),
    }
}

#[derive(Clone)]
struct FakeFile {
    metadata: Value,
    bytes: Vec<u8>,
}
impl FakeFile {
    fn new(
        id: &str,
        name: &str,
        mime: &str,
        parent: &str,
        properties: Value,
        bytes: &[u8],
    ) -> Self {
        Self {
            metadata: json!({"kind":"drive#file","id":id,"name":name,"mimeType":mime,"parents":[parent],"trashed":false,"appProperties":properties,"size":bytes.len().to_string()}),
            bytes: bytes.to_vec(),
        }
    }
}
type Files = Arc<Mutex<HashMap<String, FakeFile>>>;

fn stored_workspace() -> Files {
    Arc::new(Mutex::new(HashMap::from([
        (
            "workspace".into(),
            FakeFile::new(
                "workspace",
                "XIX-Upscaler",
                "application/vnd.google-apps.folder",
                "my-drive",
                json!({"xix_kind":"workspace","xix_version":"1"}),
                b"",
            ),
        ),
        (
            "jobs".into(),
            FakeFile::new(
                "jobs",
                "jobs",
                "application/vnd.google-apps.folder",
                "workspace",
                json!({"xix_kind":"jobs","xix_version":"1"}),
                b"",
            ),
        ),
        (
            "marker".into(),
            FakeFile::new(
                "marker",
                "desktop-marker.json",
                "application/json",
                "workspace",
                json!({"xix_kind":"marker","xix_version":"1"}),
                format!("{{\"schema_version\":1,\"desktop_install_id\":\"{INSTALL}\"}}").as_bytes(),
            ),
        ),
    ])))
}

fn multipart(request: &Request) -> (Value, Vec<u8>) {
    let content_type = &request.headers["content-type"];
    assert!(content_type.starts_with("multipart/related;"));
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .unwrap()
        .trim_matches('"');
    let delimiter = format!("\r\n--{boundary}");
    let raw = &request.body;
    let json_start = raw.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
    let json_end = raw[json_start..]
        .windows(delimiter.len())
        .position(|v| v == delimiter.as_bytes())
        .unwrap()
        + json_start;
    let metadata = serde_json::from_slice(&raw[json_start..json_end]).unwrap();
    let media_start = raw[json_end + delimiter.len()..]
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .unwrap()
        + json_end
        + delimiter.len()
        + 4;
    let media_end = raw[media_start..]
        .windows(delimiter.len())
        .position(|v| v == delimiter.as_bytes())
        .unwrap()
        + media_start;
    (metadata, raw[media_start..media_end].to_vec())
}

fn file_server(files: Files) -> Server {
    let mut generated = 0;
    Server::handler(move |request| {
        let url = Url::parse(&format!("http://127.0.0.1{}", request.target)).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        if url.path() == "/drive/v3/files/generateIds" {
            let count: usize = params["count"].parse().unwrap();
            let ids: Vec<String> = (0..count)
                .map(|_| {
                    generated += 1;
                    format!("generated-{generated}")
                })
                .collect();
            return Response::new(200)
                .json(json!({"kind":"drive#generatedIds","space":"drive","ids":ids}));
        }
        let mut files = files.lock().unwrap();
        if request.method == "GET" && url.path() == "/drive/v3/files" {
            let query = &params["q"];
            assert!(query.contains("appProperties has"));
            assert!(query.contains("trashed = false"));
            let kind = ["workspace", "jobs", "marker"]
                .into_iter()
                .find(|kind| query.contains(&format!("value='{kind}'")))
                .unwrap();
            let selected: Vec<Value> = files
                .values()
                .filter(|f| {
                    f.metadata["appProperties"]["xix_kind"] == kind
                        && f.metadata["trashed"] == false
                })
                .map(|f| f.metadata.clone())
                .collect();
            return Response::new(200).json(json!({"kind":"drive#fileList","files":selected}));
        }
        if request.method == "POST" {
            let (mut metadata, bytes) = if request
                .headers
                .get("content-type")
                .is_some_and(|v| v.starts_with("multipart/related"))
            {
                multipart(request)
            } else {
                (
                    serde_json::from_slice::<Value>(&request.body).unwrap(),
                    vec![],
                )
            };
            let id = metadata["id"]
                .as_str()
                .expect("all creates need a reserved id")
                .to_owned();
            if files.contains_key(&id) {
                return Response::new(409);
            }
            metadata["size"] = json!(bytes.len().to_string());
            metadata["trashed"] = json!(false);
            if metadata["parents"] == json!(["root"]) {
                metadata["parents"] = json!(["my-drive"]);
            }
            files.insert(
                id,
                FakeFile {
                    metadata: metadata.clone(),
                    bytes,
                },
            );
            return Response::new(200).json(metadata);
        }
        let id = url.path().rsplit('/').next().unwrap();
        let Some(file) = files.get_mut(id) else {
            return Response::new(404);
        };
        if request.method == "GET" {
            return if params.get("alt").is_some_and(|v| v == "media") {
                Response::new(200).bytes(&file.bytes)
            } else {
                Response::new(200).json(file.metadata.clone())
            };
        }
        if request.method == "PATCH" {
            if params.get("uploadType").is_some_and(|v| v == "media") {
                file.bytes = request.body.clone();
                file.metadata["size"] = json!(file.bytes.len().to_string());
            } else {
                let change: Value = serde_json::from_slice(&request.body).unwrap();
                for (key, value) in change.as_object().unwrap() {
                    file.metadata[key] = value.clone();
                }
            }
            return Response::new(200).json(file.metadata.clone());
        }
        Response::new(400)
    })
}

#[tokio::test]
async fn create_files_checks_workspace_and_preserves_existing_exchange_on_resume() {
    let state = stored_workspace();
    let server = file_server(state.clone());
    let client = server.client();
    client
        .create_job_files(&workspace_ids(), &job(), &remote_files())
        .await
        .unwrap();
    {
        let mut state = state.lock().unwrap();
        assert!(!state.contains_key("manifest"));
        for id in ["a", "b"] {
            let value: Value = serde_json::from_slice(&state[id].bytes).unwrap();
            assert_eq!(
                value,
                json!({"schema_version":1,"revision":0,"state":"queued","session_id":null,"heartbeat_at":null,"progress":{"segment_index":0,"segment_count":0,"percent":0.0,"last_checkpoint":null},"message":"","metadata":{}})
            );
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&state["control"].bytes).unwrap(),
            json!({"schema_version":1,"revision":0,"action":"run"})
        );
        assert!(state["output"].bytes.is_empty());
        for id in ["input", "a", "b", "control", "output"] {
            assert_eq!(state[id].metadata["parents"], json!(["folder"]));
            state.get_mut(id).unwrap().bytes = format!("existing-{id}").into_bytes();
        }
    }
    client
        .create_job_files(&workspace_ids(), &job(), &remote_files())
        .await
        .unwrap();
    let state = state.lock().unwrap();
    for id in ["input", "a", "b", "control", "output"] {
        assert_eq!(state[id].bytes, format!("existing-{id}").as_bytes());
    }
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.iter().filter(|r| r.method == "POST").count(), 6);
    assert!(!requests.iter().any(|r| r.method == "PATCH"));
}

#[tokio::test]
async fn missing_or_foreign_workspace_stops_before_any_remote_creation() {
    for corrupt in ["root-missing", "wrong-kind", "wrong-parent", "wrong-marker"] {
        let state = stored_workspace();
        {
            let mut state = state.lock().unwrap();
            match corrupt {
                "root-missing" => { state.remove("workspace"); }
                "wrong-kind" => state.get_mut("workspace").unwrap().metadata["appProperties"]["xix_kind"] = json!("job"),
                "wrong-parent" => state.get_mut("jobs").unwrap().metadata["parents"] = json!(["other-root"]),
                _ => state.get_mut("marker").unwrap().bytes = br#"{"schema_version":1,"desktop_install_id":"33333333-3333-4333-8333-333333333333"}"#.to_vec(),
            }
        }
        let server = file_server(state);
        assert_eq!(
            server
                .client()
                .create_job_files(&workspace_ids(), &job(), &remote_files())
                .await
                .unwrap_err(),
            DriveError::WorkspaceMismatch
        );
        assert!(server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.method == "GET"));
    }
}

#[tokio::test]
async fn manifest_is_created_last_only_after_server_confirms_input_size() {
    let state = stored_workspace();
    let server = file_server(state.clone());
    let client = server.client();
    let files = remote_files();
    let job = job();
    let manifest = serde_json::to_vec(&job.manifest_json).unwrap();
    client
        .create_job_files(&workspace_ids(), &job, &files)
        .await
        .unwrap();
    assert_eq!(
        client
            .publish_manifest(&files, &manifest)
            .await
            .unwrap_err(),
        DriveError::SizeMismatch
    );
    assert!(!state.lock().unwrap().contains_key("manifest"));
    client.update_bytes("input", b"abc").await.unwrap();
    client.publish_manifest(&files, &manifest).await.unwrap();
    client.publish_manifest(&files, &manifest).await.unwrap();
    let state = state.lock().unwrap();
    assert_eq!(state["manifest"].bytes, manifest);
    let requests = server.requests.lock().unwrap();
    let creates: Vec<_> = requests.iter().filter(|r| r.method == "POST").collect();
    assert_eq!(creates.len(), 7);
    assert_eq!(multipart(creates.last().unwrap()).0["id"], "manifest");
}

#[tokio::test]
async fn published_manifest_cannot_be_changed_on_retry() {
    let state = stored_workspace();
    let server = file_server(state.clone());
    let client = server.client();
    let files = remote_files();
    let job = job();
    let mut manifest = job.manifest_json.clone();
    client
        .create_job_files(&workspace_ids(), &job, &files)
        .await
        .unwrap();
    client.update_bytes("input", b"abc").await.unwrap();
    client
        .publish_manifest(&files, &serde_json::to_vec(&manifest).unwrap())
        .await
        .unwrap();
    manifest["mute_audio"] = json!(true);
    assert_eq!(
        client
            .publish_manifest(&files, &serde_json::to_vec(&manifest).unwrap())
            .await
            .unwrap_err(),
        DriveError::Conflict
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&state.lock().unwrap()["manifest"].bytes).unwrap()
            ["mute_audio"],
        false
    );
}

#[tokio::test]
async fn trash_refuses_broad_folders_and_requires_owned_job_uuid() {
    let state = stored_workspace();
    let server = file_server(state.clone());
    let client = server.client();
    for id in ["workspace", "jobs", "marker"] {
        assert_eq!(client.trash(id).await.unwrap_err(), DriveError::Conflict);
    }
    client
        .create_job_files(&workspace_ids(), &job(), &remote_files())
        .await
        .unwrap();
    state.lock().unwrap().get_mut("folder").unwrap().metadata["appProperties"]["xix_job_id"] =
        json!("not-a-uuid");
    assert_eq!(
        client.trash("folder").await.unwrap_err(),
        DriveError::Conflict
    );
    state.lock().unwrap().get_mut("folder").unwrap().metadata["appProperties"]["xix_job_id"] =
        json!(JOB);
    client.trash("folder").await.unwrap();
    assert_eq!(state.lock().unwrap()["folder"].metadata["trashed"], true);
    // App can restart after trash succeeds but before removing its local record.
    client.trash("folder").await.unwrap();
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "PATCH")
            .count(),
        1
    );
}

#[tokio::test]
async fn workspace_discovery_reuses_saved_layout_without_writes() {
    let state = stored_workspace();
    let server = file_server(state);
    let client = server.client();
    assert_eq!(client.ensure_workspace().await.unwrap(), workspace_ids());
    assert_eq!(client.ensure_workspace().await.unwrap(), workspace_ids());
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
}

#[tokio::test]
async fn empty_workspace_is_created_once_and_marker_identifies_this_desktop() {
    let state = Arc::new(Mutex::new(HashMap::new()));
    let server = file_server(state.clone());
    let client = server.client();
    let ids = client.ensure_workspace().await.unwrap();
    assert_eq!(client.ensure_workspace().await.unwrap(), ids);
    let state = state.lock().unwrap();
    assert_eq!(state.len(), 3);
    assert_eq!(
        serde_json::from_slice::<Value>(&state[&ids.marker_id].bytes).unwrap(),
        json!({"schema_version":1,"desktop_install_id":INSTALL})
    );
}

#[tokio::test]
async fn resumable_upload_saves_session_before_chunks_and_resumes_from_server_offset() {
    let dir = TestDir::new();
    let path = dir.0.join("input.mp4");
    std::fs::write(&path, vec![7; 262147]).unwrap();
    let saved = Arc::new(Mutex::new(Vec::new()));
    let captured = saved.clone();
    let server = Server::handler(move |r| {
        if r.method == "PATCH" {
            return Response::new(200).header(
                "Location",
                &format!(
                    "http://{}/upload/drive/v3/files/input?upload_id=private",
                    r.headers["host"]
                ),
            );
        }
        if r.method == "GET" {
            return Response::new(200).json(json!({"id":"input","size":"262147"}));
        }
        assert!(
            !captured.lock().unwrap().is_empty(),
            "session not durably saved before bytes"
        );
        if r.headers["content-range"] == "bytes */262147" {
            return Response::new(308).header("Range", "bytes=0-262143");
        }
        assert_eq!(r.headers["content-range"], "bytes 262144-262146/262147");
        assert_eq!(r.body, vec![7; 3]);
        Response::new(200).json(json!({"id":"input","size":"262147"}))
    });
    let sink: SessionSink = {
        let saved = saved.clone();
        Arc::new(move |s| {
            saved.lock().unwrap().push(s);
            Ok(())
        })
    };
    let session = format!(
        "{}upload/drive/v3/files/input?upload_id=private",
        server.origin
    );
    saved.lock().unwrap().push(Some(session.clone()));
    let result = server
        .client()
        .upload_resumable("input", &path, Some(&session), sink, progress())
        .await
        .unwrap();
    assert_eq!(result.uploaded_bytes, 262147);
    assert_eq!(saved.lock().unwrap().last(), Some(&None));
    assert!(server
        .requests
        .lock()
        .unwrap()
        .iter()
        .all(|r| r.method != "PATCH"));
}

#[tokio::test]
async fn new_upload_chunks_are_aligned_and_complete_only_after_size_check() {
    let dir = TestDir::new();
    let path = dir.0.join("input.mp4");
    let size = 8 * 1024 * 1024 + 3;
    std::fs::write(&path, vec![6; size]).unwrap();
    let saved = Arc::new(Mutex::new(Vec::new()));
    let captured = saved.clone();
    let server = Server::handler(move |r| {
        if r.method == "PATCH" {
            assert_eq!(r.headers["x-upload-content-length"], "8388611");
            return Response::new(200).header(
                "Location",
                &format!(
                    "http://{}/upload/drive/v3/files/input?upload_id=private",
                    r.headers["host"]
                ),
            );
        }
        if r.method == "GET" {
            return Response::new(200).json(json!({"id":"input","size":"8388611"}));
        }
        assert_eq!(captured.lock().unwrap().len(), 1);
        match r.headers["content-range"].as_str() {
            "bytes 0-8388607/8388611" => {
                assert_eq!(r.body.len(), 8388608);
                Response::new(308).header("Range", "bytes=0-8388607")
            }
            "bytes 8388608-8388610/8388611" => {
                assert_eq!(r.body, vec![6; 3]);
                Response::new(200).json(json!({"id":"input","size":"8388611"}))
            }
            _ => panic!("unexpected upload range"),
        }
    });
    let sink: SessionSink = {
        let saved = saved.clone();
        Arc::new(move |s| {
            saved.lock().unwrap().push(s);
            Ok(())
        })
    };
    let result = server
        .client()
        .upload_resumable("input", &path, None, sink, progress())
        .await
        .unwrap();
    assert_eq!(result.uploaded_bytes, size as u64);
    assert_eq!(saved.lock().unwrap().len(), 2);
    assert!(saved.lock().unwrap()[0].is_some());
    assert_eq!(saved.lock().unwrap()[1], None);
}

#[tokio::test]
async fn upload_rejects_foreign_session_or_failed_persistence_without_sending_video() {
    let dir = TestDir::new();
    let path = dir.0.join("input.mp4");
    std::fs::write(&path, b"abc").unwrap();
    let server = Server::handler(|r| {
        Response::new(200).header(
            "Location",
            &format!(
                "http://{}/upload/drive/v3/files/input?upload_id=x",
                r.headers["host"]
            ),
        )
    });
    let client = server.client();
    assert_eq!(
        client
            .upload_resumable(
                "input",
                &path,
                Some("https://attacker.invalid/session"),
                sessions(),
                progress()
            )
            .await
            .unwrap_err(),
        DriveError::UnsafeUrl
    );
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(
        client
            .upload_resumable(
                "input",
                &path,
                None,
                Arc::new(|_| Err(DriveError::Storage)),
                progress()
            )
            .await
            .unwrap_err(),
        DriveError::Storage
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn expired_session_restarts_but_never_reuses_foreign_redirect() {
    let dir = TestDir::new();
    let path = dir.0.join("input.mp4");
    std::fs::write(&path, b"abc").unwrap();
    let server = Server::new(vec![
        Response::new(404),
        Response::new(200).header("Location", "https://attacker.invalid/session"),
    ]);
    let session = format!("{}upload/drive/v3/files/input?upload_id=old", server.origin);
    assert_eq!(
        server
            .client()
            .upload_resumable("input", &path, Some(&session), sessions(), progress())
            .await
            .unwrap_err(),
        DriveError::UnsafeUrl
    );
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn download_resumes_with_range_and_restarts_when_server_ignores_it() {
    for ignored in [false, true] {
        let dir = TestDir::new();
        let path = dir.0.join(".output.mp4.part");
        std::fs::write(&path, b"abc").unwrap();
        let response = if ignored {
            Response::new(200).bytes(b"abcdef")
        } else {
            Response::new(206)
                .header("Content-Range", "bytes 3-5/6")
                .bytes(b"def")
        };
        let server = Server::new(vec![
            Response::new(200).json(json!({"id":"output","size":"6"})),
            response,
        ]);
        server
            .client()
            .download_resumable("output", &path, progress())
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");
        assert_eq!(
            server.requests.lock().unwrap()[1].headers["range"],
            "bytes=3-"
        );
    }
}

#[tokio::test]
async fn bad_download_range_never_appends_to_partial_file() {
    let dir = TestDir::new();
    let path = dir.0.join(".output.mp4.part");
    std::fs::write(&path, b"abc").unwrap();
    let server = Server::new(vec![
        Response::new(200).json(json!({"id":"output","size":"6"})),
        Response::new(206)
            .header("Content-Range", "bytes 2-4/6")
            .bytes(b"cde"),
    ]);
    assert!(server
        .client()
        .download_resumable("output", &path, progress())
        .await
        .is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"abc");
}

#[tokio::test]
async fn download_rejects_an_unsafe_parent_before_contacting_drive() {
    let dir = TestDir::new();
    let blocked_parent = dir.0.join("not-a-directory");
    std::fs::write(&blocked_parent, b"owner data").unwrap();
    let path = blocked_parent.join(".output.mp4.part");
    let server = Server::new(vec![
        Response::new(200).json(json!({"id":"output","size":"3"})),
        Response::new(200).bytes(b"abc"),
    ]);
    assert_eq!(
        server
            .client()
            .download_resumable("output", &path, progress())
            .await
            .unwrap_err(),
        DriveError::LocalIo
    );
    assert!(server.requests.lock().unwrap().is_empty());
    assert_eq!(std::fs::read(blocked_parent).unwrap(), b"owner data");
}

#[test]
fn upload_contract_accepts_transport_streams_and_rejects_unaccepted_avi() {
    let mut manifest = job().manifest_json;
    manifest["input"]["extension"] = json!("ts");
    manifest["input"]["name"] = json!("input.ts");
    assert!(manifest_input(&manifest).is_ok());
    manifest["input"]["extension"] = json!("avi");
    manifest["input"]["name"] = json!("input.avi");
    assert!(manifest_input(&manifest).is_err());
}

#[tokio::test]
async fn lost_chunk_reply_queries_server_before_sending_remaining_bytes() {
    let dir = TestDir::new();
    let path = dir.0.join("input.mp4");
    std::fs::write(&path, vec![8; 262147]).unwrap();
    let mut lost = false;
    let server = Server::handler(move |r| {
        if r.method == "PATCH" {
            return Response::new(200).header(
                "Location",
                &format!(
                    "http://{}/upload/drive/v3/files/input?upload_id=x",
                    r.headers["host"]
                ),
            );
        }
        if r.method == "GET" {
            return Response::new(200).json(json!({"id":"input","size":"262147"}));
        }
        if !lost {
            lost = true;
            assert_eq!(r.headers["content-range"], "bytes 0-262146/262147");
            let mut reply = Response::new(200);
            reply.disconnect = true;
            return reply;
        }
        if r.headers["content-range"] == "bytes */262147" {
            assert!(r.body.is_empty());
            return Response::new(308).header("Range", "bytes=0-262143");
        }
        assert_eq!(r.headers["content-range"], "bytes 262144-262146/262147");
        assert_eq!(r.body, vec![8; 3]);
        Response::new(200).json(json!({"id":"input","size":"262147"}))
    });
    assert_eq!(
        server
            .client()
            .upload_resumable("input", &path, None, sessions(), progress())
            .await
            .unwrap()
            .uploaded_bytes,
        262147
    );
    assert_eq!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == "PUT")
            .count(),
        3
    );
}

#[tokio::test]
async fn malformed_or_impossible_upload_offsets_are_not_trusted() {
    let dir = TestDir::new();
    let path = dir.0.join("input.mp4");
    std::fs::write(&path, b"abc").unwrap();
    for range in [
        "bytes=1-1",
        "bytes=0-3",
        "bytes=0--1",
        "bytes=0-18446744073709551615",
        "bytes=0-1,3-4",
    ] {
        let server = Server::new(vec![Response::new(308).header("Range", range)]);
        let session = format!("{}upload/drive/v3/files/input?upload_id=x", server.origin);
        assert_eq!(
            server
                .client()
                .upload_resumable("input", &path, Some(&session), sessions(), progress())
                .await
                .unwrap_err(),
            DriveError::InvalidResponse
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn short_download_is_rejected_before_overwriting_a_partial_file() {
    let dir = TestDir::new();
    let path = dir.0.join(".output.mp4.part");
    let first = Server::new(vec![
        Response::new(200).json(json!({"id":"output","size":"6"})),
        Response::new(200).bytes(b"abc"),
    ]);
    // A complete but shorter-than-advertised file is rejected before replacing a partial file.
    assert_eq!(
        first
            .client()
            .download_resumable("output", &path, progress())
            .await
            .unwrap_err(),
        DriveError::SizeMismatch
    );
    assert!(std::fs::read(&path).unwrap().is_empty());
    std::fs::write(&path, b"abc").unwrap();
    let retry = Server::new(vec![
        Response::new(200).json(json!({"id":"output","size":"6"})),
        Response::new(206)
            .header("Content-Range", "bytes 3-5/6")
            .bytes(b"def"),
    ]);
    retry
        .client()
        .download_resumable("output", &path, progress())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
}

#[tokio::test]
async fn disconnected_download_automatically_resumes_at_last_written_byte() {
    let dir = TestDir::new();
    let path = dir.0.join(".output.mp4.part");
    let mut interrupted = Response::new(200).bytes(b"abc");
    interrupted.declared_length = Some(6);
    let server = Server::new(vec![
        Response::new(200).json(json!({"id":"output","size":"6"})),
        interrupted,
        Response::new(206)
            .header("Content-Range", "bytes 3-5/6")
            .bytes(b"def"),
    ]);
    server
        .client()
        .download_resumable("output", &path, progress())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
    assert_eq!(
        server.requests.lock().unwrap()[2].headers["range"],
        "bytes=3-"
    );
}
