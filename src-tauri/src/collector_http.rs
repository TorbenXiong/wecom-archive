use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub(super) struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(super) struct UploadIdentity<'a> {
    pub export_id: &'a str,
    pub content_sha256: &'a str,
}

#[derive(Debug)]
pub(super) struct UploadError {
    pub outcome_unknown: bool,
    pub error: io::Error,
}

impl UploadError {
    fn before_send(error: io::Error) -> Self {
        Self {
            outcome_unknown: false,
            error,
        }
    }

    fn after_spawn(error: io::Error) -> Self {
        // Once curl is running, it may have delivered the request even if its
        // pipe, process or response later fails. Keep the original upload ID.
        Self {
            outcome_unknown: true,
            error,
        }
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for UploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl Response {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

struct RequestFile(PathBuf);

impl Drop for RequestFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(root) = self.0.parent() {
            let _ = std::fs::remove_dir(root);
        }
    }
}

/// Credentials only travel through an anonymous pipe. The request body is staged
/// under the collector work directory so large encrypted packages do not depend
/// on curl's config-line limits; the temporary file is removed on every exit.
pub(super) fn post_json(
    endpoint: &str,
    token: &str,
    body: &[u8],
    content_type: &str,
    connect_timeout: u32,
    timeout: u32,
) -> io::Result<Response> {
    post_json_upload(
        endpoint,
        token,
        body,
        content_type,
        connect_timeout,
        timeout,
    )
    .map_err(|error| error.error)
}

/// Legacy uploads need the same send-boundary information without new headers.
pub(super) fn post_json_upload(
    endpoint: &str,
    token: &str,
    body: &[u8],
    content_type: &str,
    connect_timeout: u32,
    timeout: u32,
) -> Result<Response, UploadError> {
    request(
        endpoint,
        token,
        Some((body, content_type)),
        None,
        connect_timeout,
        timeout,
    )
}

pub(super) fn post_upload(
    endpoint: &str,
    token: &str,
    body: &[u8],
    identity: UploadIdentity<'_>,
    connect_timeout: u32,
    timeout: u32,
) -> Result<Response, UploadError> {
    request(
        endpoint,
        token,
        Some((body, "application/octet-stream")),
        Some(identity),
        connect_timeout,
        timeout,
    )
}

/// Status queries carry no body and create no collector work files.
pub(super) fn get_json(
    endpoint: &str,
    token: &str,
    connect_timeout: u32,
    timeout: u32,
) -> io::Result<Response> {
    request(endpoint, token, None, None, connect_timeout, timeout).map_err(|error| error.error)
}

fn request(
    endpoint: &str,
    token: &str,
    body: Option<(&[u8], &str)>,
    identity: Option<UploadIdentity<'_>>,
    connect_timeout: u32,
    timeout: u32,
) -> Result<Response, UploadError> {
    let url = tauri::Url::parse(endpoint)
        .map_err(|_| UploadError::before_send(io::Error::other("invalid endpoint")))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || token.is_empty()
        || token.bytes().any(|byte| byte.is_ascii_control())
        || body.is_some_and(|(bytes, content_type)| {
            !matches!(
                content_type,
                "application/json" | "application/octet-stream"
            ) || bytes.first() != Some(&b'{')
        })
        || identity.as_ref().is_some_and(|identity| {
            uuid::Uuid::parse_str(identity.export_id).map_or(true, |id| id.is_nil())
                || identity.content_sha256.len() != 64
                || !identity
                    .content_sha256
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
    {
        return Err(UploadError::before_send(io::Error::other(
            "invalid request",
        )));
    }
    let request = if let Some((bytes, _)) = body {
        let root = crate::paths::collector_work_root("http").map_err(|_| {
            UploadError::before_send(io::Error::other("request directory unavailable"))
        })?;
        let path = root.join("request.partial");
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(UploadError::before_send)?;
        // Register cleanup only once this request owns the newly created file.
        let request = RequestFile(path);
        output.write_all(bytes).map_err(UploadError::before_send)?;
        drop(output);
        Some(request)
    } else {
        None
    };
    let mut command = Command::new("curl.exe");
    command.args([
        "--disable",
        "--fail-with-body",
        "--silent",
        "--show-error",
        "--connect-timeout",
        &connect_timeout.to_string(),
        "--max-time",
        &timeout.to_string(),
        "--proto",
        "=http,https",
        "--request",
        if body.is_some() { "POST" } else { "GET" },
        "--header",
        "@-",
        "--url",
        endpoint,
        "--write-out",
        "\n%{http_code}",
    ]);
    if let Some((_, content_type)) = body {
        command.args(["--header", &format!("Content-Type: {content_type}")]);
    } else {
        command.args(["--header", "Accept: application/json"]);
    }
    if let Some(request) = &request {
        let mut body_argument = std::ffi::OsString::from("@");
        body_argument.push(&request.0);
        command.arg("--data-binary").arg(body_argument);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    source_windows::updater::hide(&mut command);
    let mut child = command.spawn().map_err(UploadError::before_send)?;
    let written = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("request pipe missing"))?;
        writeln!(stdin, "Authorization: Bearer {token}")?;
        if let Some(identity) = identity {
            writeln!(stdin, "X-Collector-Export-Id: {}", identity.export_id)?;
            writeln!(
                stdin,
                "X-Collector-Content-Sha256: {}",
                identity.content_sha256
            )?;
        }
        Ok::<_, io::Error>(())
    })();
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(UploadError::after_spawn(error));
    }
    let output = child.wait_with_output().map_err(UploadError::after_spawn)?;
    if !output.status.success() && output.status.code() != Some(22) {
        return Err(UploadError::after_spawn(io::Error::other(
            "request transport failed",
        )));
    }
    let split = output
        .stdout
        .iter()
        .rposition(|byte| *byte == b'\n')
        .ok_or_else(|| UploadError::after_spawn(io::Error::other("response status missing")))?;
    let status = std::str::from_utf8(&output.stdout[split + 1..])
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|status| (100..600).contains(status))
        .ok_or_else(|| UploadError::after_spawn(io::Error::other("invalid response status")))?;
    Ok(Response {
        status,
        body: output.stdout[..split].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    fn serve_request(
        status: u16,
        method: &'static str,
        body: Vec<u8>,
        identity: Option<(String, String)>,
        location: Option<String>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/synthetic", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic request not received");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => panic!("synthetic accept failed"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let (header_end, length) = loop {
                let mut buffer = [0; 8192];
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    if headers
                        .to_ascii_lowercase()
                        .contains("expect: 100-continue")
                    {
                        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
                    }
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while request.len() < header_end + length {
                let mut buffer = [0; 8192];
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            let headers = String::from_utf8_lossy(&request[..header_end]);
            assert!(headers.starts_with(&format!("{method} /synthetic HTTP/1.1\r\n")));
            assert!(headers.contains("Authorization: Bearer synthetic-status-token\r\n"));
            assert_eq!(&request[header_end..header_end + length], body.as_slice());
            if method == "GET" {
                assert_eq!(length, 0);
                assert!(!headers.to_ascii_lowercase().contains("transfer-encoding:"));
                assert!(!headers.contains("Content-Type:"));
            }
            if let Some((id, hash)) = identity {
                assert!(headers.contains(&format!("X-Collector-Export-Id: {id}\r\n")));
                assert!(headers.contains(&format!("X-Collector-Content-Sha256: {hash}\r\n")));
                assert!(headers.contains("Content-Type: application/octet-stream\r\n"));
            } else {
                assert!(!headers.contains("X-Collector-Export-Id:"));
                assert!(!headers.contains("X-Collector-Content-Sha256:"));
            }
            let response = b"{\"status\":\"synthetic\"}\n";
            write!(
                stream,
                "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n",
                response.len()
            )
            .unwrap();
            if let Some(location) = location {
                write!(stream, "Location: {location}\r\n").unwrap();
            }
            stream.write_all(b"\r\n").unwrap();
            stream.write_all(response).unwrap();
        });
        (endpoint, server)
    }

    #[test]
    fn authenticated_status_queries_preserve_get_and_error_responses_without_a_body() {
        for status in [200, 400, 503] {
            let (endpoint, server) = serve_request(status, "GET", Vec::new(), None, None);
            let response = get_json(&endpoint, "synthetic-status-token", 2, 5).unwrap();
            server.join().unwrap();
            assert_eq!(response.status, status);
            assert_eq!(response.body, b"{\"status\":\"synthetic\"}\n");
        }
    }

    #[test]
    fn uploads_preserve_the_bound_identity_body_and_http_error_status() {
        let id = "00000000-0000-4000-8000-000000000001";
        let hash = "a".repeat(64);
        for status in [202, 400, 503] {
            let body =
                serde_json::to_vec(&serde_json::json!({"synthetic": "x".repeat(128 * 1024)}))
                    .unwrap();
            let (endpoint, server) = serve_request(
                status,
                "POST",
                body.clone(),
                Some((id.into(), hash.clone())),
                None,
            );
            let response = post_upload(
                &endpoint,
                "synthetic-status-token",
                &body,
                UploadIdentity {
                    export_id: id,
                    content_sha256: &hash,
                },
                2,
                5,
            )
            .unwrap();
            server.join().unwrap();
            assert_eq!(response.status, status);
            assert_eq!(response.body, b"{\"status\":\"synthetic\"}\n");
        }
    }

    #[test]
    fn status_queries_do_not_follow_redirects() {
        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        redirect.set_nonblocking(true).unwrap();
        let location = format!(
            "http://{}/must-not-receive-credentials",
            redirect.local_addr().unwrap()
        );
        let (endpoint, server) = serve_request(302, "GET", Vec::new(), None, Some(location));
        let response = get_json(&endpoint, "synthetic-status-token", 2, 5).unwrap();
        server.join().unwrap();
        assert_eq!(response.status, 302);
        assert!(
            matches!(redirect.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn rejects_upload_metadata_and_invalid_status_credentials_before_spawning() {
        let valid_id = "00000000-0000-4000-8000-000000000001";
        let valid_hash = "a".repeat(64);
        for id in [
            "invalid",
            "00000000-0000-0000-0000-000000000000",
            "00000000-0000-4000-8000-000000000001\r\nX-Injected: true",
        ] {
            let error = post_upload(
                "http://127.0.0.1:1/",
                "synthetic-status-token",
                b"{}",
                UploadIdentity {
                    export_id: id,
                    content_sha256: &valid_hash,
                },
                1,
                1,
            )
            .err()
            .unwrap();
            assert_eq!(error.to_string(), "invalid request");
            assert!(!error.outcome_unknown);
        }
        for hash in [
            "a".repeat(63),
            "A".repeat(64),
            "z".repeat(64),
            format!("{}\r\n", "a".repeat(62)),
        ] {
            let error = post_upload(
                "http://127.0.0.1:1/",
                "synthetic-status-token",
                b"{}",
                UploadIdentity {
                    export_id: valid_id,
                    content_sha256: &hash,
                },
                1,
                1,
            )
            .err()
            .unwrap();
            assert_eq!(error.to_string(), "invalid request");
            assert!(!error.outcome_unknown);
        }
        for token in ["", "synthetic\r\nX-Injected: true", "synthetic\0token"] {
            assert_eq!(
                get_json("http://127.0.0.1:1/", token, 1, 1)
                    .err()
                    .unwrap()
                    .to_string(),
                "invalid request"
            );
        }
        for endpoint in ["file:///synthetic", "http://user:synthetic@127.0.0.1:1/"] {
            assert!(get_json(endpoint, "synthetic-status-token", 1, 1).is_err());
        }
    }

    #[test]
    fn upload_validation_failures_are_definite_and_do_not_send_a_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/synthetic", listener.local_addr().unwrap());
        let id = "00000000-0000-4000-8000-000000000001";
        let hash = "a".repeat(64);
        let error = post_upload(
            &endpoint,
            "synthetic\r\ninvalid-token",
            b"{}",
            UploadIdentity {
                export_id: id,
                content_sha256: &hash,
            },
            1,
            1,
        )
        .err()
        .unwrap();
        assert!(!error.outcome_unknown);
        assert_eq!(error.error.to_string(), "invalid request");
        for (body, content_type) in [
            (b"invalid".as_slice(), "application/octet-stream"),
            (b"{}".as_slice(), "text/plain"),
        ] {
            let error = post_json_upload(
                &endpoint,
                "synthetic-status-token",
                body,
                content_type,
                1,
                1,
            )
            .err()
            .unwrap();
            assert!(!error.outcome_unknown);
            assert_eq!(error.error.to_string(), "invalid request");
        }
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
    }

    #[test]
    fn upload_response_failures_after_curl_starts_keep_the_outcome_unknown() {
        let id = "00000000-0000-4000-8000-000000000001";
        let hash = "a".repeat(64);
        let (endpoint, server) = serve_request(
            999,
            "POST",
            b"{}".to_vec(),
            Some((id.into(), hash.clone())),
            None,
        );
        let error = post_upload(
            &endpoint,
            "synthetic-status-token",
            b"{}",
            UploadIdentity {
                export_id: id,
                content_sha256: &hash,
            },
            1,
            2,
        )
        .err()
        .unwrap();
        server.join().unwrap();
        assert!(error.outcome_unknown);
        let (endpoint, server) = serve_request(999, "POST", b"{}".to_vec(), None, None);
        let error = post_json_upload(
            &endpoint,
            "synthetic-status-token",
            b"{}",
            "application/octet-stream",
            1,
            2,
        )
        .err()
        .unwrap();
        server.join().unwrap();
        assert!(error.outcome_unknown);
    }

    #[test]
    fn legacy_structured_uploads_preserve_http_errors_without_identity_headers() {
        let (endpoint, server) = serve_request(400, "POST", b"{}".to_vec(), None, None);
        let response = post_json_upload(
            &endpoint,
            "synthetic-status-token",
            b"{}",
            "application/octet-stream",
            1,
            2,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(response.status, 400);
        assert_eq!(response.body, b"{\"status\":\"synthetic\"}\n");
    }

    #[test]
    fn authenticated_posts_preserve_headers_json_bytes_and_http_errors() {
        for (status, content_type) in [(200, "application/json"), (503, "application/octet-stream")]
        {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let endpoint = format!("http://{}/synthetic", listener.local_addr().unwrap());
            let padding = if status == 200 {
                128 * 1024
            } else {
                12 * 1024 * 1024
            };
            let body = serde_json::to_vec(
                &serde_json::json!({"text": "合成\n\"\\\t@-", "padding": "x".repeat(padding)}),
            )
            .unwrap();
            let expected_body = body.clone();
            let server = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "synthetic request not received");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("synthetic accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut request = Vec::new();
                let (header_end, length) = loop {
                    let mut buffer = [0; 8192];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        if headers
                            .to_ascii_lowercase()
                            .contains("expect: 100-continue")
                        {
                            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").unwrap();
                        }
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while request.len() < header_end + length {
                    let mut buffer = [0; 8192];
                    let read = stream.read(&mut buffer).unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                let headers = String::from_utf8_lossy(&request[..header_end]);
                assert!(headers.contains("Authorization: Bearer synthetic-\\-\"-token\r\n"));
                assert!(headers.contains(&format!("Content-Type: {content_type}\r\n")));
                assert_eq!(
                    &request[header_end..header_end + length],
                    expected_body.as_slice()
                );
                let response = b"{\"status\":\"synthetic\"}\n";
                write!(stream, "HTTP/1.1 {status} Synthetic\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).unwrap();
                stream.write_all(response).unwrap();
            });
            let response = post_json(
                &endpoint,
                "synthetic-\\-\"-token",
                &body,
                content_type,
                2,
                10,
            )
            .unwrap();
            server.join().unwrap();
            assert_eq!(response.status, status);
            assert_eq!(response.is_success(), status == 200);
            assert_eq!(response.body, b"{\"status\":\"synthetic\"}\n");
        }
    }

    #[test]
    fn rejects_header_injection_and_non_http_urls_before_spawning() {
        for token in ["", "synthetic\r\nX-Injected: true", "synthetic\0token"] {
            assert!(
                post_json(
                    "http://127.0.0.1:1/",
                    token,
                    b"{}",
                    "application/json",
                    1,
                    1
                )
                .is_err()
            );
        }
        for endpoint in ["file:///synthetic", "http://user:synthetic@127.0.0.1:1/"] {
            assert!(
                post_json(endpoint, "synthetic-token", b"{}", "application/json", 1, 1).is_err()
            );
        }
    }
}
