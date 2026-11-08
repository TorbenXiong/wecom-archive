use super::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const TOKEN: &str = "synthetic-confirmation-token";
const COMPLETED_AT: &str = "2026-01-02T03:04:05Z";
const RESULT_PATH: &str = "/synthetic/api/v1/collector-uploads/";

#[derive(Clone)]
struct Request {
    method: String,
    path: String,
    headers: String,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    fn id(&self) -> Uuid {
        Uuid::parse_str(self.path.strip_prefix(RESULT_PATH).unwrap()).unwrap()
    }
}

enum Reply {
    Json(u16, Value),
    Close,
}

struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn new(mut reply: impl FnMut(&Request, usize) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!(
            "http://{}/synthetic/api/v1/imports/enterprise",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(12);
            while !stopping.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "synthetic server exceeded deadline"
                );
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("synthetic accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_request(&mut stream);
                assert_eq!(
                    request.header("authorization"),
                    Some("Bearer synthetic-confirmation-token")
                );
                let index = {
                    let mut requests = captured.lock().unwrap();
                    let index = requests.len();
                    requests.push(request.clone());
                    index
                };
                if let Reply::Json(status, body) = reply(&request, index) {
                    let body = serde_json::to_vec(&body).unwrap();
                    write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    stream.write_all(&body).unwrap();
                }
            }
        });
        Self {
            endpoint,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn count(&self, method: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == method)
            .count()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            assert!(result.is_ok(), "synthetic server failed");
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() < 1024 * 1024);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut first_line = headers.lines().next().unwrap().split_whitespace();
    let method = first_line.next().unwrap().to_owned();
    let path = first_line.next().unwrap().to_owned();
    let mut request = Request {
        method,
        path,
        headers,
        body: Vec::new(),
    };
    let length = request
        .header("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    assert!(length < 1024 * 1024);
    if request
        .header("expect")
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
    {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
    }
    while bytes.len() < header_end + length {
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "request ended before complete body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    request.body = bytes[header_end..header_end + length].to_vec();
    request
}

fn policy() -> Policy {
    Policy {
        receive_timeout: 2,
        status_timeout: 1,
        confirm_budget: Duration::from_secs(2),
        poll_interval: Duration::from_millis(2),
    }
}

fn package() -> (Vec<u8>, PackageIdentity) {
    let identity = PackageIdentity {
        export_id: Uuid::new_v4(),
        content_sha256: "a".repeat(64),
    };
    let body = serde_json::to_vec(&json!({
        "export_id": identity.export_id,
        "plaintext_sha256": identity.content_sha256,
        "synthetic": "fixture"
    }))
    .unwrap();
    (body, identity)
}

fn missing() -> Reply {
    Reply::Json(404, json!({"code": "COLLECTOR_UPLOAD_NOT_FOUND"}))
}

fn accepted(request: &Request) -> Reply {
    Reply::Json(202, json!({"uploadId": request.id()}))
}

fn progress(request: &Request, identity: &PackageIdentity, status: &str) -> Value {
    json!({
        "schema": "collector-upload.v1",
        "uploadId": request.id(),
        "exportId": identity.export_id,
        "contentSha256": identity.content_sha256,
        "status": status,
        "result": (status == "completed").then(|| json!({
            "exportId": identity.export_id,
            "completedAt": COMPLETED_AT,
            "inserted": 3,
            "unchanged": 4,
            "revised": 2,
            "mediaCount": 5
        }))
    })
}

fn assert_unknown<T: std::fmt::Debug>(result: Result<T, CommandError>, state: &AppState) {
    assert_eq!(result.unwrap_err().code, "ENTERPRISE_UPLOAD_RESULT_UNKNOWN");
    assert!(state.pending_upload.lock().unwrap().is_some());
    assert!(state.confirmed_upload.lock().unwrap().is_none());
    let status = state.schedule_status.lock().unwrap();
    assert!(status.last_success_at.is_none());
    assert_eq!(status.uploaded_count, 0);
}

#[test]
fn tracked_upload_confirms_bound_result_after_one_post() {
    let state = AppState::default();
    let (package, identity) = package();
    let expected = identity.clone();
    let server = Server::new(move |request, index| match index {
        0 => {
            assert_eq!(request.method, "GET");
            missing()
        }
        1 => {
            assert_eq!(request.method, "POST");
            accepted(request)
        }
        2 => Reply::Json(200, progress(request, &expected, "completed")),
        _ => panic!("unexpected request"),
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert!(state.pending_upload.lock().unwrap().is_none());
    let status = state.schedule_status.lock().unwrap().clone();
    assert_eq!(status.last_success_at.as_deref(), Some(COMPLETED_AT));
    assert_eq!(status.last_upload_message_count, Some(9));
    assert_eq!(status.last_media_count, Some(5));
    let receipt = state.confirmed_upload.lock().unwrap().clone().unwrap();
    assert!(receipt.identity == identity);
    assert_eq!(receipt.scope, client.scope);
    let requests = server.requests();
    assert_eq!(server.count("POST"), 1);
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.id() == requests[0].id())
    );
    assert_eq!(requests[1].body, package);
    assert_eq!(
        requests[1].header("X-Collector-Export-Id"),
        Some(identity.export_id.to_string().as_str())
    );
    assert_eq!(
        requests[1].header("X-Collector-Content-Sha256"),
        Some(identity.content_sha256.as_str())
    );
}

#[test]
fn lost_post_response_only_queries_original_upload_to_completion() {
    let state = AppState::default();
    let (package, identity) = package();
    let expected_body = package.clone();
    let server = Server::new(move |request, index| match index {
        0 => missing(),
        1 => {
            assert_eq!(request.method, "POST");
            assert_eq!(request.body, expected_body);
            Reply::Close
        }
        2 => Reply::Json(200, progress(request, &identity, "completed")),
        _ => panic!("unexpected request"),
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert_eq!(server.count("POST"), 1);
    let requests = server.requests();
    assert_eq!(requests[2].method, "GET");
    assert_eq!(requests[1].id(), requests[2].id());
    assert!(state.pending_upload.lock().unwrap().is_none());
}

#[test]
fn mismatched_refusal_responses_query_the_original_upload_to_completion() {
    for code in [
        "INVALID_QUERY",
        "COLLECTOR_UPLOAD_BUSY",
        "UNAUTHORIZED",
        "COLLECTOR_IDENTITY_REQUIRED",
    ] {
        for status in [408, 409, 429, 500, 502, 503, 504] {
            if status == 503 && code == "COLLECTOR_UPLOAD_BUSY" {
                continue;
            }
            let state = AppState::default();
            let (package, identity) = package();
            let expected_body = package.clone();
            let server = Server::new(move |request, index| match index {
                0 => missing(),
                1 => {
                    assert_eq!(request.method, "POST");
                    assert_eq!(request.body, expected_body);
                    Reply::Json(status, json!({"code": code}))
                }
                2 => Reply::Json(200, progress(request, &identity, "completed")),
                _ => panic!("unexpected request"),
            });
            let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
            assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
            let requests = server.requests();
            assert_eq!(requests.len(), 3);
            assert_eq!(server.count("POST"), 1);
            assert_eq!(requests[2].method, "GET");
            assert!(
                requests
                    .iter()
                    .all(|request| request.id() == requests[0].id())
            );
            assert!(state.pending_upload.lock().unwrap().is_none());
            assert!(state.confirmed_upload.lock().unwrap().is_some());
        }
    }
}

#[test]
fn mismatched_refusal_retains_pending_and_recovers_without_reposting() {
    let state = AppState::default();
    let (package, identity) = package();
    let complete = Arc::new(AtomicBool::new(false));
    let completed = complete.clone();
    let server = Server::new(move |request, index| match index {
        0 => missing(),
        1 => {
            assert_eq!(request.method, "POST");
            Reply::Json(503, json!({"code": "UNAUTHORIZED"}))
        }
        _ => {
            assert_eq!(request.method, "GET");
            if completed.load(Ordering::Acquire) {
                Reply::Json(200, progress(request, &identity, "completed"))
            } else {
                Reply::Json(503, json!({"code": "UPSTREAM_TIMEOUT"}))
            }
        }
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_unknown(client.upload(&package), &state);
    let pending_id = state
        .pending_upload
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .upload_id;
    assert_unknown(client.upload(&package), &state);
    assert_eq!(server.count("POST"), 1);
    assert_eq!(
        state
            .pending_upload
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upload_id,
        pending_id
    );
    complete.store(true, Ordering::Release);
    let confirmed = client.confirm_pending().unwrap().unwrap();
    assert_eq!(confirmed.completed_at, COMPLETED_AT);
    assert_eq!(confirmed.message_count, 9);
    let request_count = server.requests().len();
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert_eq!(server.requests().len(), request_count);
    assert_eq!(server.count("POST"), 1);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.id() == pending_id)
    );
    assert!(state.pending_upload.lock().unwrap().is_none());
}

#[test]
fn exact_registration_refusals_clear_pending_and_allow_a_new_attempt() {
    for (status, code) in [
        (400, "INVALID_QUERY"),
        (401, "UNAUTHORIZED"),
        (403, "COLLECTOR_IDENTITY_REQUIRED"),
        (503, "COLLECTOR_UPLOAD_BUSY"),
    ] {
        let state = AppState::default();
        let (package, identity) = package();
        let server = Server::new(move |request, index| match index {
            0 | 2 => missing(),
            1 => {
                assert_eq!(request.method, "POST");
                Reply::Json(status, json!({"code": code}))
            }
            3 => accepted(request),
            4 => Reply::Json(200, progress(request, &identity, "completed")),
            _ => panic!("unexpected request"),
        });
        let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
        assert_eq!(
            client.upload(&package).unwrap_err().code,
            "ENTERPRISE_UPLOAD_FAILED"
        );
        assert!(state.pending_upload.lock().unwrap().is_none());
        assert!(state.confirmed_upload.lock().unwrap().is_none());
        assert_eq!(server.requests().len(), 2);
        assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
        let requests = server.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(server.count("POST"), 2);
        assert_ne!(requests[1].id(), requests[3].id());
        assert_eq!(requests[3].id(), requests[4].id());
    }
}

#[test]
fn running_budget_keeps_pending_then_confirmation_and_receipt_reuse_do_not_post() {
    let state = AppState::default();
    let (package, identity) = package();
    let complete = Arc::new(AtomicBool::new(false));
    let complete_reply = complete.clone();
    let server = Server::new(move |request, index| {
        if index == 0 {
            return missing();
        }
        if request.method == "POST" {
            return accepted(request);
        }
        Reply::Json(
            200,
            progress(
                request,
                &identity,
                if complete_reply.load(Ordering::Acquire) {
                    "completed"
                } else {
                    "running"
                },
            ),
        )
    });
    let short = Policy {
        confirm_budget: Duration::from_millis(30),
        ..policy()
    };
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, short);
    assert_unknown(client.upload(&package), &state);
    let pending_id = state
        .pending_upload
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .upload_id;
    complete.store(true, Ordering::Release);
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    let confirmed = client.confirm_pending().unwrap().unwrap();
    assert_eq!(confirmed.completed_at, COMPLETED_AT);
    assert_eq!(confirmed.message_count, 9);
    assert_eq!(confirmed.media_count, 5);
    assert!(state.pending_upload.lock().unwrap().is_none());
    let before = server.requests().len();
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert_eq!(server.requests().len(), before);
    assert_eq!(server.count("POST"), 1);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.id() == pending_id)
    );
}

#[test]
fn completion_identity_digest_and_timestamp_mismatches_never_confirm_success() {
    for field in [
        "schema",
        "uploadId",
        "exportId",
        "contentSha256",
        "resultExport",
        "completedAt",
        "countOverflow",
        "missingCount",
    ] {
        let state = AppState::default();
        let (package, identity) = package();
        let server = Server::new(move |request, index| match index {
            0 => missing(),
            1 => accepted(request),
            2 => {
                let mut result = progress(request, &identity, "completed");
                match field {
                    "schema" => result["schema"] = json!("collector-upload.v2"),
                    "uploadId" => result["uploadId"] = json!(Uuid::new_v4()),
                    "exportId" => result["exportId"] = json!(Uuid::new_v4()),
                    "contentSha256" => result["contentSha256"] = json!("b".repeat(64)),
                    "resultExport" => result["result"]["exportId"] = json!(Uuid::new_v4()),
                    "completedAt" => result["result"]["completedAt"] = json!("invalid-time"),
                    "countOverflow" => result["result"]["inserted"] = json!(u64::MAX),
                    "missingCount" => {
                        result["result"]
                            .as_object_mut()
                            .unwrap()
                            .remove("mediaCount");
                    }
                    _ => unreachable!(),
                }
                Reply::Json(200, result)
            }
            _ => panic!("unexpected request"),
        });
        let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
        assert_unknown(client.upload(&package), &state);
        assert_eq!(server.count("POST"), 1);
    }
}

#[test]
fn confirmed_failure_clears_pending_and_allows_a_new_upload_attempt() {
    let state = AppState::default();
    let (package, identity) = package();
    let server = Server::new(move |request, index| match index {
        0 | 3 => missing(),
        1 | 4 => accepted(request),
        2 => Reply::Json(200, progress(request, &identity, "failed")),
        5 => Reply::Json(200, progress(request, &identity, "completed")),
        _ => panic!("unexpected request"),
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_eq!(
        client.upload(&package).unwrap_err().code,
        "ENTERPRISE_UPLOAD_FAILED"
    );
    assert!(state.pending_upload.lock().unwrap().is_none());
    assert!(state.confirmed_upload.lock().unwrap().is_none());
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert_eq!(server.count("POST"), 2);
    let requests = server.requests();
    assert_ne!(requests[1].id(), requests[4].id());
}

#[test]
fn unsupported_status_api_preserves_the_legacy_post_and_confirmed_receipt() {
    let state = AppState::default();
    let (package, identity) = package();
    let expected_body = package.clone();
    let server = Server::new(move |request, index| match index {
        0 => Reply::Json(404, json!({"code": "NOT_FOUND"})),
        1 => {
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/synthetic/api/v1/imports/enterprise");
            assert_eq!(request.body, expected_body);
            assert!(request.header("X-Collector-Export-Id").is_none());
            assert!(request.header("X-Collector-Content-Sha256").is_none());
            Reply::Json(
                200,
                json!({"exportId": identity.export_id, "completedAt": COMPLETED_AT,
                    "inserted": 3, "unchanged": 4, "revised": 2, "mediaCount": 5}),
            )
        }
        _ => panic!("unexpected request"),
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert_eq!(client.upload(&package).unwrap(), COMPLETED_AT);
    assert_eq!(server.requests().len(), 2);
    assert_eq!(server.count("POST"), 1);
    assert!(state.pending_upload.lock().unwrap().is_none());
    assert!(state.confirmed_upload.lock().unwrap().is_some());
}

#[test]
fn lost_legacy_response_retains_pending_and_does_not_repeat_an_unqueryable_upload() {
    let state = AppState::default();
    let (package, _) = package();
    let server = Server::new(move |request, index| match index {
        0 => Reply::Json(404, json!({"code": "NOT_FOUND"})),
        1 => {
            assert_eq!(request.method, "POST");
            Reply::Close
        }
        _ => panic!("unexpected request"),
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_unknown(client.upload(&package), &state);
    assert!(
        !state
            .pending_upload
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .tracked
    );
    assert_unknown(client.confirm_pending(), &state);
    assert_unknown(client.upload(&package), &state);
    assert_eq!(server.requests().len(), 2);
    assert_eq!(server.count("POST"), 1);
}

#[test]
fn changed_scope_makes_no_request_and_query_auth_failure_does_not_revoke_identity() {
    let state = AppState::default();
    let (_, identity) = package();
    let old = UploadClient::new(
        &state,
        "http://127.0.0.1:1/api/v1/imports/enterprise",
        TOKEN,
        policy(),
    );
    *state.pending_upload.lock().unwrap() = Some(PendingUpload {
        upload_id: Uuid::new_v4(),
        identity,
        scope: old.scope.clone(),
        tracked: true,
    });
    let server = Server::new(|_, _| panic!("changed endpoint must not be queried"));
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_unknown(client.confirm_pending(), &state);
    assert!(server.requests().is_empty());

    let state = AppState::default();
    let (package, _) = package();
    let server = Server::new(|request, index| match index {
        0 => missing(),
        1 => accepted(request),
        2 => Reply::Json(401, json!({"code": "UNAUTHORIZED"})),
        _ => panic!("unexpected request"),
    });
    let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
    assert_unknown(client.upload(&package), &state);
    assert!(
        !state
            .schedule_status
            .lock()
            .unwrap()
            .server_authorization_denied
    );
    assert_eq!(server.count("POST"), 1);
}

#[test]
fn known_before_send_failure_clears_pending_without_confirming_or_posting() {
    for tracked in [false, true] {
        let state = AppState::default();
        let (package, _) = package();
        // Valid JSON identity, rejected by the transport before it starts curl.
        let package = [b" ".as_slice(), package.as_slice()].concat();
        let server = Server::new(move |request, index| {
            assert_eq!(index, 0);
            assert_eq!(request.method, "GET");
            if tracked {
                missing()
            } else {
                Reply::Json(404, json!({"code": "NOT_FOUND"}))
            }
        });
        let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
        assert_eq!(
            client.upload(&package).unwrap_err().code,
            "ENTERPRISE_UPLOAD_FAILED"
        );
        assert!(state.pending_upload.lock().unwrap().is_none());
        assert!(state.confirmed_upload.lock().unwrap().is_none());
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.count("POST"), 0);
    }
}

#[test]
fn local_collection_or_config_failure_cannot_hide_a_pending_result() {
    let state = AppState::default();
    let (_, identity) = package();
    let client = UploadClient::new(&state, "http://127.0.0.1:1/", TOKEN, policy());
    client
        .save(PendingUpload {
            upload_id: Uuid::new_v4(),
            identity,
            scope: client.scope.clone(),
            tracked: true,
        })
        .unwrap();
    crate::state::begin_collector_attempt(&state);
    crate::state::finish_collector_attempt(&state, &Ok::<_, CommandError>(()));
    assert!(state.schedule_status.lock().unwrap().upload_result_unknown);
    crate::state::begin_collector_attempt(&state);
    crate::state::finish_collector_attempt(
        &state,
        &Err::<(), _>(CommandError {
            code: "ENTERPRISE_CONFIG_EXPIRED",
            message: "synthetic expired config".into(),
            recoverable: true,
        }),
    );
    assert!(state.schedule_status.lock().unwrap().upload_result_unknown);
    assert!(state.pending_upload.lock().unwrap().is_some());
}

#[test]
fn manual_confirmation_returns_original_result_without_reading_or_uploading_latest_export() {
    for old_snapshot in [false, true] {
        let state = AppState::default();
        let (_, identity) = package();
        let expected = identity.clone();
        let server = Server::new(move |request, index| {
            assert_eq!(index, 0);
            assert_eq!(request.method, "GET");
            Reply::Json(200, progress(request, &expected, "completed"))
        });
        let client = UploadClient::new(&state, &server.endpoint, TOKEN, policy());
        let pending_id = Uuid::new_v4();
        client
            .save(PendingUpload {
                upload_id: pending_id,
                identity,
                scope: client.scope.clone(),
                tracked: true,
            })
            .unwrap();
        let old_export_id = if old_snapshot {
            let export =
                archive_transfer::create_export("synthetic", vec![], vec![], vec![]).unwrap();
            let id = export.export_id;
            *state.latest_export.lock().unwrap() = Some(Arc::new(export));
            Some(id)
        } else {
            None
        };
        let config = serde_json::from_value(json!({
            "schemaVersion": "enterprise-collector.v1",
            "organizationId": "synthetic", "organizationName": "synthetic",
            "collectionNotice": "synthetic", "keyId": "synthetic",
            "publicKeyHex": "", "signingPublicKeyHex": "", "signatureHex": "",
            "uploadUrl": server.endpoint, "uploadToken": TOKEN,
            "collectorSchedule": {"mode": "interval", "intervalMinutes": 10}
        }))
        .unwrap();
        crate::state::begin_collector_attempt(&state);
        let result = crate::commands::upload_latest_for_config(&state, &config);
        crate::state::finish_collector_attempt(&state, &result);
        assert_eq!(result.unwrap().message_count, 9);
        let status = state.schedule_status.lock().unwrap();
        assert_eq!(status.last_success_at.as_deref(), Some(COMPLETED_AT));
        assert_eq!(status.last_message_count, Some(9));
        assert_eq!(status.last_media_count, Some(5));
        assert!(status.next_run_at.is_some());
        assert!(!status.upload_result_unknown);
        assert!(state.pending_upload.lock().unwrap().is_none());
        assert!(state.uploaded_messages.lock().unwrap().is_empty());
        assert_eq!(
            state
                .latest_export
                .lock()
                .unwrap()
                .as_ref()
                .map(|e| e.export_id),
            old_export_id
        );
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0].id(), pending_id);
        assert_eq!(server.count("POST"), 0);
    }
}

#[test]
fn worker_panic_preserves_pending_and_returns_unknown_to_the_command() {
    let state = AppState::default();
    let (_, identity) = package();
    let client = UploadClient::new(&state, "http://127.0.0.1:1/", TOKEN, policy());
    client
        .save(PendingUpload {
            upload_id: Uuid::new_v4(),
            identity,
            scope: client.scope.clone(),
            tracked: true,
        })
        .unwrap();
    state.collection_running.store(true, Ordering::Release);
    crate::state::begin_collector_attempt(&state);
    let result = tauri::async_runtime::block_on(crate::state::run_owned_collector_attempt(
        state.clone(),
        crate::state::CollectionRunGuard(state.collection_running.clone()),
        |_| -> Result<(), CommandError> { panic!("synthetic worker failure") },
    ));
    assert_unknown(result, &state);
    assert!(!state.collection_running.load(Ordering::Acquire));
    let status = state.schedule_status.lock().unwrap();
    assert!(!status.running);
    assert!(status.upload_result_unknown);
    assert!(status.last_error.is_some());
}
