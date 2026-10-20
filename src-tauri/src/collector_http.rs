use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub(super) struct Response {
    pub status: u16,
    pub body: Vec<u8>,
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
    let url = tauri::Url::parse(endpoint).map_err(|_| io::Error::other("invalid endpoint"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || token.is_empty()
        || token.bytes().any(|byte| byte.is_ascii_control())
        || !matches!(
            content_type,
            "application/json" | "application/octet-stream"
        )
        || body.first() != Some(&b'{')
    {
        return Err(io::Error::other("invalid request"));
    }
    let root = super::collector_work_root("http")
        .map_err(|_| io::Error::other("request directory unavailable"))?;
    let request = RequestFile(root.join("request.partial"));
    {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&request.0)?;
        output.write_all(body)?;
    }
    let mut body_argument = std::ffi::OsString::from("@");
    body_argument.push(&request.0);
    let mut command = Command::new("curl.exe");
    command
        .args([
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
            "POST",
            "--header",
            &format!("Content-Type: {content_type}"),
            "--header",
            "@-",
            "--url",
            endpoint,
            "--write-out",
            "\n%{http_code}",
        ])
        .arg("--data-binary")
        .arg(body_argument)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    source_windows::updater::hide(&mut command);
    let mut child = command.spawn()?;
    let written = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("request pipe missing"))?;
        writeln!(stdin, "Authorization: Bearer {token}")
    })();
    if let Err(error) = written {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let output = child.wait_with_output()?;
    if !output.status.success() && output.status.code() != Some(22) {
        return Err(io::Error::other("request transport failed"));
    }
    let split = output
        .stdout
        .iter()
        .rposition(|byte| *byte == b'\n')
        .ok_or_else(|| io::Error::other("response status missing"))?;
    let status = std::str::from_utf8(&output.stdout[split + 1..])
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|status| (100..600).contains(status))
        .ok_or_else(|| io::Error::other("invalid response status"))?;
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
