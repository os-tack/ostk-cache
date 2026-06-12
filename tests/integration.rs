use serde_json::json;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn proxy_firmware_byte_stability_across_user_message_length() {
    // Spin up a mock Anthropic API upstream
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    // Run the proxy in the background
    let proxy_port = 8089;
    let mut proxy_process = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("ostk-cache")
        .env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url)
        .spawn()
        .expect("Failed to start proxy");

    // Give proxy a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    // Simulate 3 requests with different user message lengths
    let system_prompt = "A".repeat(4096);
    let lengths = vec![100, 500, 2000];
    let mut captured_firmwares = Vec::new();

    for len in lengths {
        let user_msg = "B".repeat(len);
        let req_body = json!({
            "system": system_prompt,
            "messages": [
                {
                    "role": "user",
                    "content": user_msg
                }
            ]
        });

        let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);

        let client = reqwest::Client::new();
        let request_task = tokio::spawn(async move {
            let _ = client
                .post(&proxy_url)
                .header("anthropic-session-id", "test-session")
                .json(&req_body)
                .send()
                .await;
        });

        // Wait for the proxy to forward the request to our mock upstream
        let (mut stream, _) = mock_upstream.accept().await.unwrap();
        let mut buf = vec![0u8; 65536];
        let n = stream.read(&mut buf).await.unwrap();
        let received_str = String::from_utf8_lossy(&buf[..n]);

        // Find the body after the double newline
        if let Some(body_start) = received_str.find("\r\n\r\n") {
            let body_json = &received_str[body_start + 4..];
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body_json) {
                // Extract the firmware string from req.system
                let firmware = parsed["system"][0]["text"].as_str().unwrap().to_string();
                captured_firmwares.push(firmware);
            } else {
                panic!("Failed to parse upstream body: {}", body_json);
            }
        } else {
            panic!("Malformed HTTP request received at upstream");
        }

        // Close connection
        drop(stream);
        let _ = request_task.await;
    }

    // Kill proxy
    let _ = proxy_process.kill();
    let _ = proxy_process.wait();

    // Assert stability
    assert_eq!(captured_firmwares.len(), 3);
    assert_eq!(
        captured_firmwares[0], captured_firmwares[1],
        "Firmware changed between T1 and T2"
    );
    assert_eq!(
        captured_firmwares[1], captured_firmwares[2],
        "Firmware changed between T2 and T3"
    );
}

#[tokio::test]
async fn proxy_forwards_authorization_and_api_key_headers() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8090;
    let mut proxy_process = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("ostk-cache")
        .env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url)
        .spawn()
        .expect("Failed to start proxy");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let req_body = json!({
        "system": "Sys",
        "messages": [
            {
                "role": "user",
                "content": "Hello"
            }
        ]
    });

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);

    let client = reqwest::Client::new();
    let _request_task = tokio::spawn(async move {
        let _ = client
            .post(&proxy_url)
            .header("x-api-key", "my-api-key")
            .header("authorization", "Bearer test-token")
            .header("anthropic-session-id", "test-session")
            .json(&req_body)
            .send()
            .await;
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).await.unwrap();
    let received_str = String::from_utf8_lossy(&buf[..n]);

    let _ = proxy_process.kill();
    let _ = proxy_process.wait();

    let lower = received_str.to_lowercase();
    let api_key_count = lower.matches("x-api-key: my-api-key").count();
    let auth_count = lower.matches("authorization: bearer test-token").count();
    assert_eq!(
        api_key_count, 1,
        "x-api-key header should appear exactly once, got {}",
        api_key_count
    );
    assert_eq!(
        auth_count, 1,
        "authorization header should appear exactly once, got {}",
        auth_count
    );
}

#[tokio::test]
async fn proxy_does_not_prepend_hud_when_user_message_has_tool_results() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8091;
    let mut proxy_process = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("ostk-cache")
        .env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url)
        .spawn()
        .expect("Failed to start proxy");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let req_body = json!({
        "system": "System prompt",
        "messages": [
            {"role": "user", "content": "Run the tool"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_01", "name": "echo", "input": {"x": 1}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_01", "content": "ok"}
            ]}
        ]
    });

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);

    let client = reqwest::Client::new();
    let _request_task = tokio::spawn(async move {
        let _ = client
            .post(&proxy_url)
            .header("x-api-key", "my-api-key")
            .header("anthropic-session-id", "tool-test-session")
            .json(&req_body)
            .send()
            .await;
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).await.unwrap();
    let received_str = String::from_utf8_lossy(&buf[..n]);

    let _ = proxy_process.kill();
    let _ = proxy_process.wait();

    let body_start = received_str.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let body = &received_str[body_start..];

    let parsed: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|_| panic!("body should be valid JSON: {}", body));
    let last_msg = parsed["messages"]
        .as_array()
        .and_then(|a| a.last())
        .expect("messages array should have last");
    let content = last_msg["content"]
        .as_array()
        .expect("content should be array");

    assert_eq!(
        content[0]["type"], "tool_result",
        "first block of tool_result-bearing user message must be tool_result, got {:?}",
        content[0]
    );

    let has_text_with_hud = content.iter().any(|b| {
        b.get("type").and_then(|t| t.as_str()) == Some("text")
            && b.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.contains("cache:"))
                .unwrap_or(false)
    });
    assert!(
        !has_text_with_hud,
        "no HUD text block should be inserted into a tool_result-bearing user message"
    );
}

#[tokio::test]
async fn proxy_sends_identity_accept_encoding_upstream() {
    // Anthropic gzips SSE if the request has `Accept-Encoding: gzip`.
    // Reqwest in this build has no gzip feature, so the accumulated buffer
    // would hold compressed bytes the parser can't read. Proxy must strip
    // the caller's accept-encoding and force `identity` upstream so the
    // accumulated SSE is plain UTF-8.
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8092;
    let mut proxy_process = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("ostk-cache")
        .env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url)
        .spawn()
        .expect("Failed to start proxy");

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let req_body = json!({
        "system": "Sys",
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);

    let client = reqwest::Client::new();
    let _request_task = tokio::spawn(async move {
        let _ = client
            .post(&proxy_url)
            .header("x-api-key", "k")
            .header("accept-encoding", "gzip, br, zstd")
            .json(&req_body)
            .send()
            .await;
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf).await.unwrap();
    let received_str = String::from_utf8_lossy(&buf[..n]);

    let _ = proxy_process.kill();
    let _ = proxy_process.wait();

    let lower = received_str.to_lowercase();
    let identity_count = lower.matches("accept-encoding: identity").count();
    let gzip_count = lower.matches("accept-encoding: gzip").count();

    assert_eq!(
        identity_count, 1,
        "expected accept-encoding: identity exactly once upstream, got {}",
        identity_count
    );
    assert_eq!(
        gzip_count, 0,
        "expected no accept-encoding: gzip upstream (caller's gzip preference must be stripped), got {}",
        gzip_count
    );
}

// ---------------------------------------------------------------------------
// Helpers used by the three follow-up tests below
// ---------------------------------------------------------------------------

/// Read one HTTP/1.1 request from `stream` (headers + Content-Length body) and
/// return the full bytes received. Returns the raw bytes so callers can split
/// header / body themselves.
async fn read_full_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = [0u8; 8192];

    // Read until end of headers
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await.expect("read upstream req");
        if n == 0 {
            return buf;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(idx) = find_subseq(&buf, b"\r\n\r\n") {
            header_end = idx + 4;
            break;
        }
    }

    // Parse Content-Length
    let header_str = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let cl: usize = header_str
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    // Read the rest of the body
    while buf.len() < header_end + cl {
        let n = stream.read(&mut tmp).await.expect("read upstream body");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    buf
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Write a complete HTTP/1.1 200 response with a JSON body containing a `usage`
/// block so the proxy persists a ledger row.
async fn write_usage_response(stream: &mut tokio::net::TcpStream, input_tokens: u64) {
    let body = json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "ok"}],
        "model": "claude-test",
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": 1,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0
        }
    })
    .to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn write_openai_usage_response(
    stream: &mut tokio::net::TcpStream,
    input_tokens: u64,
    cached_tokens: u64,
) {
    let body = json!({
        "id": "resp_test",
        "object": "response",
        "status": "completed",
        "model": "gpt-5.5",
        "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}],
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": 1,
            "total_tokens": input_tokens + 1,
            "input_tokens_details": {"cached_tokens": cached_tokens}
        }
    })
    .to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn write_codex_rollout(
    sessions_dir: &std::path::Path,
    workspace_cwd: &std::path::Path,
    message: &str,
) {
    let uuid = "019ebd4a-f74c-7223-b855-7d20e434d240";
    let day = sessions_dir.join("2026/06/12");
    std::fs::create_dir_all(&day).unwrap();
    let path = day.join(format!("rollout-2026-06-12T19-00-00-{uuid}.jsonl"));
    let meta = json!({
        "timestamp": "2026-06-12T19:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": uuid,
            "cwd": workspace_cwd.to_string_lossy().to_string(),
            "originator": "codex-tui",
            "model_provider": "openai"
        }
    })
    .to_string();
    let user = json!({
        "timestamp": "2026-06-12T19:00:01Z",
        "type": "event_msg",
        "payload": {
            "type": "user_message",
            "message": message
        }
    })
    .to_string();
    std::fs::write(path, format!("{meta}\n{user}\n")).unwrap();
}

/// Spawn the proxy binary with the given env, optional cwd, and wait briefly
/// for it to be listening. Caller must kill/wait on the returned Child.
///
/// Uses the pre-built binary (`CARGO_BIN_EXE_ostk-cache`) so we can set an
/// arbitrary cwd without `cargo run` failing to locate the workspace Cargo.toml.
fn spawn_proxy(
    proxy_port: u16,
    upstream_url: &str,
    cwd: Option<&std::path::Path>,
) -> std::process::Child {
    spawn_proxy_with_env(proxy_port, upstream_url, cwd, &[])
}

fn spawn_proxy_with_env(
    proxy_port: u16,
    upstream_url: &str,
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> std::process::Child {
    let bin = env!("CARGO_BIN_EXE_ostk-cache");
    let mut cmd = Command::new(bin);
    cmd.env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(p) = cwd {
        cmd.current_dir(p);
    }
    cmd.spawn().expect("Failed to start proxy")
}

async fn wait_for_proxy(port: u16, timeout_ms: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "proxy did not come up on port {} within {}ms",
                port, timeout_ms
            );
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn gpt_adapter_adds_cache_knobs_and_records_cached_usage() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let tmp = tempfile::tempdir().expect("tempdir");
    let proxy_port = 8108;
    let mut proxy = spawn_proxy_with_env(
        proxy_port,
        &upstream_url,
        Some(tmp.path()),
        &[("OSTK_PROVIDER", "gpt"), ("OPENAI_BASE_URL", &upstream_url)],
    );
    wait_for_proxy(proxy_port, 5000).await;

    let proxy_url = format!("http://127.0.0.1:{}/v1/responses", proxy_port);
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&proxy_url)
            .header("authorization", "Bearer secret")
            .header("x-client-request-id", "gpt-session")
            .json(&json!({
                "model": "gpt-5.5",
                "input": [{"role": "user", "content": "hello"}]
            }))
            .send()
            .await
            .expect("send gpt request");
        let status = resp.status();
        let _ = resp.bytes().await;
        status
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let upstream_req = read_full_http_request(&mut up).await;
    write_openai_usage_response(&mut up, 2006, 1920).await;
    let status = req_task.await.unwrap();
    assert_eq!(status.as_u16(), 200);

    let _ = proxy.kill();
    let _ = proxy.wait();

    let body_idx = find_subseq(&upstream_req, b"\r\n\r\n").expect("upstream req has body") + 4;
    let body: serde_json::Value = serde_json::from_slice(&upstream_req[body_idx..]).unwrap();
    assert_eq!(body["model"], json!("gpt-5.5"));
    assert_eq!(body["prompt_cache_retention"], json!("24h"));
    assert_eq!(
        body["prompt_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("ostk:gpt:"),
        true,
        "prompt_cache_key should be generated from workspace identity: {}",
        body
    );

    let ledger_path = tmp.path().join(".ostk/memory/ledger.jsonl");
    let content = std::fs::read_to_string(&ledger_path)
        .expect("ledger.jsonl should exist after completed gpt request");
    let row: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(row["session"], json!("gpt-session"));
    assert_eq!(row["mode"], json!("gpt"));
    assert_eq!(row["input_tokens_total"], json!(86));
    assert_eq!(row["cache_read_tokens"], json!(1920));
}

#[tokio::test]
async fn gpt_codex_tail_flag_off_forwards_body_byte_identical() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let cwd = tempfile::tempdir().expect("cwd tempdir");
    let sessions = tempfile::tempdir().expect("sessions tempdir");
    let cwd_canonical = std::fs::canonicalize(cwd.path()).unwrap();
    write_codex_rollout(sessions.path(), &cwd_canonical, "hidden tail message");
    let sessions_str = sessions.path().to_string_lossy().to_string();
    let proxy_port = 8115;
    let mut proxy = spawn_proxy_with_env(
        proxy_port,
        &upstream_url,
        Some(cwd.path()),
        &[
            ("OSTK_PROVIDER", "gpt"),
            ("OPENAI_BASE_URL", &upstream_url),
            ("OSTK_CACHE_CODEX_SESSIONS_DIR", &sessions_str),
        ],
    );
    wait_for_proxy(proxy_port, 5000).await;

    let original_body = serde_json::to_string(&json!({
        "model": "gpt-5",
        "instructions": "base instructions",
        "prompt_cache_key": "existing-key",
        "input": [{"type": "message", "role": "user", "content": "hello"}]
    }))
    .unwrap();
    let proxy_url = format!("http://127.0.0.1:{}/v1/responses", proxy_port);
    let original_for_send = original_body.clone();
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&proxy_url)
            .header("authorization", "Bearer secret")
            .header("x-client-request-id", "gpt-tail-off")
            .header("content-type", "application/json")
            .body(original_for_send)
            .send()
            .await
            .expect("send gpt request");
        let _ = resp.bytes().await;
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let upstream_req = read_full_http_request(&mut up).await;
    write_openai_usage_response(&mut up, 120, 0).await;
    let _ = req_task.await;
    let _ = proxy.kill();
    let _ = proxy.wait();

    let body_idx = find_subseq(&upstream_req, b"\r\n\r\n").expect("upstream req has body") + 4;
    assert_eq!(&upstream_req[body_idx..], original_body.as_bytes());
}

#[tokio::test]
async fn gpt_codex_tail_flag_on_appends_instructions_receipt() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let cwd = tempfile::tempdir().expect("cwd tempdir");
    let sessions = tempfile::tempdir().expect("sessions tempdir");
    let cwd_canonical = std::fs::canonicalize(cwd.path()).unwrap();
    write_codex_rollout(sessions.path(), &cwd_canonical, "visible tail message");
    let sessions_str = sessions.path().to_string_lossy().to_string();
    let proxy_port = 8116;
    let mut proxy = spawn_proxy_with_env(
        proxy_port,
        &upstream_url,
        Some(cwd.path()),
        &[
            ("OSTK_PROVIDER", "gpt"),
            ("OPENAI_BASE_URL", &upstream_url),
            ("OSTK_CACHE_CODEX_TAIL_TRANSCRIPT", "1"),
            ("OSTK_CACHE_CODEX_SESSIONS_DIR", &sessions_str),
        ],
    );
    wait_for_proxy(proxy_port, 5000).await;

    let original_body = serde_json::to_string(&json!({
        "model": "gpt-5",
        "instructions": "base instructions",
        "prompt_cache_key": "existing-key",
        "input": [{"type": "message", "role": "user", "content": "hello"}]
    }))
    .unwrap();
    let proxy_url = format!("http://127.0.0.1:{}/v1/responses", proxy_port);
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&proxy_url)
            .header("authorization", "Bearer secret")
            .header("x-client-request-id", "gpt-tail-on")
            .header("content-type", "application/json")
            .body(original_body)
            .send()
            .await
            .expect("send gpt request");
        let _ = resp.bytes().await;
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let upstream_req = read_full_http_request(&mut up).await;
    write_openai_usage_response(&mut up, 120, 0).await;
    let _ = req_task.await;
    let _ = proxy.kill();
    let _ = proxy.wait();

    let body_idx = find_subseq(&upstream_req, b"\r\n\r\n").expect("upstream req has body") + 4;
    let body: serde_json::Value = serde_json::from_slice(&upstream_req[body_idx..]).unwrap();
    let instructions = body["instructions"].as_str().unwrap();
    assert!(instructions.starts_with("base instructions"));
    assert!(instructions.contains("## ostk-cache Codex transcript tail"));
    assert!(instructions.contains("visible tail message"));
}

#[tokio::test]
async fn capture_http_writes_request_response_bodies_and_redacted_metadata() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let tmp = tempfile::tempdir().expect("tempdir");
    let capture_dir = tmp.path().join("captures");
    let capture_dir_str = capture_dir.to_string_lossy().to_string();
    let proxy_port = 8107;
    let mut proxy = spawn_proxy_with_env(
        proxy_port,
        &upstream_url,
        Some(tmp.path()),
        &[
            ("OSTK_CAPTURE_HTTP", "1"),
            ("OSTK_CAPTURE_HTTP_DIR", &capture_dir_str),
            ("OSTK_CACHE_PASSTHROUGH", "1"),
        ],
    );
    wait_for_proxy(proxy_port, 5000).await;

    let req_body = serde_json::to_vec(&json!({
        "system": "capture system",
        "messages": [{"role": "user", "content": "capture me"}]
    }))
    .unwrap();
    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);
    let req_body_for_client = req_body.clone();
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&proxy_url)
            .header("x-api-key", "secret-key")
            .header("anthropic-session-id", "capture-session")
            .header("content-type", "application/json")
            .body(req_body_for_client)
            .send()
            .await
            .expect("send capture request");
        let status = resp.status();
        let _ = resp.bytes().await;
        status
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let upstream_req = read_full_http_request(&mut up).await;
    write_usage_response(&mut up, 12).await;
    let status = req_task.await.unwrap();
    assert_eq!(status.as_u16(), 200);

    let mut metadata_path = None;
    for _ in 0..100 {
        // →2035: captures are now sharded into subdirs. Walk two levels:
        // capture_dir/<shard>/<entry>/metadata.json OR
        // capture_dir/<entry>/metadata.json (legacy flat, kept for compat).
        'outer: {
            if let Ok(top_entries) = std::fs::read_dir(&capture_dir) {
                for top_entry in top_entries.flatten() {
                    let top_path = top_entry.path();
                    if !top_path.is_dir() {
                        continue;
                    }
                    // Check flat (legacy): top_path is the entry dir.
                    let flat_candidate = top_path.join("metadata.json");
                    if flat_candidate.exists() {
                        metadata_path = Some(flat_candidate);
                        break 'outer;
                    }
                    // Check sharded: top_path is a shard dir.
                    if let Ok(shard_entries) = std::fs::read_dir(&top_path) {
                        for shard_entry in shard_entries.flatten() {
                            let entry_path = shard_entry.path();
                            let candidate = entry_path.join("metadata.json");
                            if candidate.exists() {
                                metadata_path = Some(candidate);
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        if metadata_path.is_some() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    let metadata_path = metadata_path.expect("capture metadata should be written");
    let capture_request_dir = metadata_path.parent().unwrap().to_path_buf();

    // →2035: capture writes are async; wait for the finish files to land
    // before killing the proxy (otherwise the writer task may be mid-write).
    let response_body_path = capture_request_dir.join("response.body");
    for _ in 0..100 {
        if response_body_path.exists() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    // Wait for the final metadata (status=200) BEFORE killing the proxy —
    // killing mid-rewrite was a torn-write race until write_metadata went
    // atomic; waiting here keeps the test deterministic either way.
    for _ in 0..100 {
        if let Ok(raw) = std::fs::read(&metadata_path) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) {
                if v["status"].as_u64().is_some() {
                    break;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    let _ = proxy.kill();
    let _ = proxy.wait();

    assert_eq!(
        std::fs::read(capture_request_dir.join("request-in.body")).unwrap(),
        req_body
    );
    assert_eq!(
        std::fs::read(capture_request_dir.join("request-out.body")).unwrap(),
        req_body,
        "passthrough capture should preserve the exact outbound body"
    );
    let response_body = std::fs::read_to_string(&response_body_path).unwrap();
    assert!(
        response_body.contains("\"usage\""),
        "response body should be captured: {}",
        response_body
    );

    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(metadata["session"], json!("capture-session"));
    assert_eq!(metadata["status"], json!(200));
    assert_eq!(
        metadata["request_in"]["bytes"],
        json!(req_body.len() as u64)
    );
    assert!(
        metadata["request_headers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| { h["name"] == json!("x-api-key") && h["value"] == json!("[redacted]") }),
        "x-api-key must be redacted in metadata: {}",
        metadata
    );

    let body_idx = find_subseq(&upstream_req, b"\r\n\r\n").expect("upstream req has body") + 4;
    assert_eq!(&upstream_req[body_idx..], req_body.as_slice());
}

// ---------------------------------------------------------------------------
// T6 follow-up: session isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_isolates_per_session_amp_accumulators_and_ledger_rows() {
    // Two requests with distinct anthropic-session-id headers should produce
    // two ledger rows whose `session` fields differ. The per-session
    // AmpAccumulator must not leak state from one session into the other.
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let tmp = tempfile::tempdir().expect("tempdir");
    let proxy_port = 8093;
    let mut proxy = spawn_proxy(proxy_port, &upstream_url, Some(tmp.path()));
    wait_for_proxy(proxy_port, 5000).await;

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);

    // Fire request #1 (session A) and serve a response so the ledger persists.
    let url1 = proxy_url.clone();
    let req_task_a = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let _ = client
            .post(&url1)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "session-a")
            .json(&json!({"system": "S", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .map(|r| r.bytes());
    });

    let (mut up_a, _) = mock_upstream.accept().await.unwrap();
    let _ = read_full_http_request(&mut up_a).await;
    write_usage_response(&mut up_a, 100).await;
    let _ = req_task_a.await;

    // Fire request #2 (session B).
    let url2 = proxy_url.clone();
    let req_task_b = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let _ = client
            .post(&url2)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "session-b")
            .json(&json!({"system": "S", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .map(|r| r.bytes());
    });

    let (mut up_b, _) = mock_upstream.accept().await.unwrap();
    let _ = read_full_http_request(&mut up_b).await;
    write_usage_response(&mut up_b, 200).await;
    let _ = req_task_b.await;

    // Wait briefly for the streaming task in the proxy to flush ledger rows.
    let ledger_path = tmp.path().join(".ostk/memory/ledger.jsonl");
    let mut tries = 0;
    while tries < 50 {
        if let Ok(s) = std::fs::read_to_string(&ledger_path)
            && s.lines().count() >= 2
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        tries += 1;
    }

    let _ = proxy.kill();
    let _ = proxy.wait();

    let content = std::fs::read_to_string(&ledger_path)
        .expect("ledger.jsonl should exist after two completed requests");
    let rows: Vec<serde_json::Value> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each ledger line is valid JSON"))
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "expected exactly two ledger rows, got {}: {}",
        rows.len(),
        content
    );

    let sessions: Vec<&str> = rows
        .iter()
        .map(|r| r["session"].as_str().expect("session field is a string"))
        .collect();
    assert!(
        sessions.contains(&"session-a"),
        "ledger missing session-a: {:?}",
        sessions
    );
    assert!(
        sessions.contains(&"session-b"),
        "ledger missing session-b: {:?}",
        sessions
    );
    assert_ne!(
        sessions[0], sessions[1],
        "two rows must carry different session ids"
    );
}

// ---------------------------------------------------------------------------
// T6 follow-up: content-type variants
// ---------------------------------------------------------------------------

#[tokio::test]
async fn proxy_accepts_charset_qualified_content_type_and_normalizes_upstream() {
    // Caller sends `application/json; charset=utf-8`. Proxy must:
    //   (1) parse the body successfully (no 400),
    //   (2) forward `content-type: application/json` upstream exactly once,
    //   (3) NOT pass the `charset=utf-8` suffix upstream (proxy controls
    //       canonicalization; upstream should see the canonical form).
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8094;
    let mut proxy = spawn_proxy(proxy_port, &upstream_url, None);
    wait_for_proxy(proxy_port, 5000).await;

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);
    let req_body = serde_json::to_vec(&json!({
        "system": "Sys",
        "messages": [{"role": "user", "content": "Hello"}]
    }))
    .unwrap();

    let client = reqwest::Client::new();
    let req_task = tokio::spawn(async move {
        let resp = client
            .post(&proxy_url)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "ct-test")
            .header("content-type", "application/json; charset=utf-8")
            .body(req_body)
            .send()
            .await;
        // Drain to completion so the proxy finishes its work cleanly.
        if let Ok(r) = resp {
            let status = r.status();
            let _ = r.bytes().await;
            status
        } else {
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        }
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let req_bytes = read_full_http_request(&mut stream).await;
    write_usage_response(&mut stream, 1).await;

    let status = req_task.await.unwrap();
    let _ = proxy.kill();
    let _ = proxy.wait();

    assert_eq!(
        status.as_u16(),
        200,
        "proxy must accept charset-qualified content-type and return 200"
    );

    let received = String::from_utf8_lossy(&req_bytes);
    let lower = received.to_lowercase();

    let canonical = lower.matches("content-type: application/json\r\n").count();
    let with_charset = lower
        .matches("content-type: application/json; charset=utf-8")
        .count();
    assert_eq!(
        canonical, 1,
        "upstream must see canonical `content-type: application/json` exactly once, got {} (raw: {:?})",
        canonical, received
    );
    assert_eq!(
        with_charset, 0,
        "proxy must not propagate the caller's `charset=utf-8` suffix upstream, got {}",
        with_charset
    );

    // Body before the headers/body split must be parseable JSON — confirms the
    // proxy did not mangle the payload while normalizing the content-type.
    let body_idx =
        find_subseq(&req_bytes, b"\r\n\r\n").expect("upstream req has body separator") + 4;
    let body = &req_bytes[body_idx..];
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "upstream body must be valid JSON, got error {}: {:?}",
            e,
            String::from_utf8_lossy(body)
        )
    });
    assert_eq!(parsed["messages"][0]["role"], "user");
}

// ---------------------------------------------------------------------------
// T6 follow-up: ledger crash-survival under SIGKILL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ledger_jsonl_survives_proxy_sigkill_and_remains_appendable() {
    // Sequence:
    //   1. Spawn proxy in a tempdir cwd, fire one full request, wait for the
    //      ledger row to land on disk.
    //   2. SIGKILL the proxy (simulates abrupt crash, no graceful shutdown).
    //   3. Re-spawn proxy in the same cwd, fire another full request.
    //   4. Verify ledger.jsonl now has 2 well-formed JSON rows (the first row
    //      survived the kill, the second appended cleanly).
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let tmp = tempfile::tempdir().expect("tempdir");
    let ledger_path = tmp.path().join(".ostk/memory/ledger.jsonl");

    // ---- Phase 1: first proxy, one completed request, wait for row ----
    let proxy_port_a = 8095;
    let mut proxy_a = spawn_proxy(proxy_port_a, &upstream_url, Some(tmp.path()));
    wait_for_proxy(proxy_port_a, 5000).await;

    let url_a = format!("http://127.0.0.1:{}/v1/messages", proxy_port_a);
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&url_a)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "crash-sess-1")
            .json(&json!({"system": "S", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await;
        if let Ok(r) = resp {
            let _ = r.bytes().await;
        }
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let _ = read_full_http_request(&mut up).await;
    write_usage_response(&mut up, 50).await;
    let _ = req_task.await;

    // Wait for the ledger row to actually land before we kill -9.
    let mut tries = 0;
    while tries < 100 {
        if let Ok(s) = std::fs::read_to_string(&ledger_path)
            && s.lines().filter(|l| !l.trim().is_empty()).count() >= 1
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        tries += 1;
    }
    let pre_crash =
        std::fs::read_to_string(&ledger_path).expect("ledger.jsonl must exist before crash");
    assert!(
        pre_crash.lines().filter(|l| !l.trim().is_empty()).count() >= 1,
        "expected at least one ledger row before SIGKILL, got: {:?}",
        pre_crash
    );

    // ---- Phase 2: SIGKILL (force kill — no chance for graceful shutdown) ----
    // The cargo wrapper PID is not the binary PID; killing it leaves the
    // child running. Send SIGKILL to the actual ostk-cache binary using pkill
    // scoped to our PROXY_PORT env via the cmdline arg PROXY_PORT=8095.
    // std::process::Child::kill on macOS sends SIGKILL, which is propagated to
    // the cargo subprocess. To be sure the proxy binary itself dies, we also
    // pkill any matching binary with our specific port env in its environment.
    let _ = proxy_a.kill(); // SIGKILL on Unix
    let _ = proxy_a.wait();
    // Best-effort: kill any lingering child that didn't go down with cargo.
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("-f")
        .arg(format!("PROXY_PORT={}", proxy_port_a))
        .output();
    // Wait for the port to actually free.
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", proxy_port_a))
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // The pre-crash ledger contents must still be on disk and parseable.
    let after_crash =
        std::fs::read_to_string(&ledger_path).expect("ledger.jsonl must still exist after SIGKILL");
    let rows_after_crash: Vec<serde_json::Value> = after_crash
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!("ledger row corrupted after crash: {} (line: {:?})", e, l)
            })
        })
        .collect();
    assert!(
        !rows_after_crash.is_empty(),
        "ledger rows written before crash must survive SIGKILL"
    );
    let pre_count = rows_after_crash.len();

    // ---- Phase 3: restart proxy in the same cwd; fire another request ----
    let proxy_port_b = 8096;
    let mut proxy_b = spawn_proxy(proxy_port_b, &upstream_url, Some(tmp.path()));
    wait_for_proxy(proxy_port_b, 10_000).await;

    let url_b = format!("http://127.0.0.1:{}/v1/messages", proxy_port_b);
    let req_task2 = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&url_b)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "crash-sess-2")
            .json(&json!({"system": "S", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await;
        if let Ok(r) = resp {
            let _ = r.bytes().await;
        }
    });

    let (mut up2, _) = mock_upstream.accept().await.unwrap();
    let _ = read_full_http_request(&mut up2).await;
    write_usage_response(&mut up2, 75).await;
    let _ = req_task2.await;

    // Wait for the new row to land.
    let mut tries = 0;
    while tries < 100 {
        if let Ok(s) = std::fs::read_to_string(&ledger_path)
            && s.lines().filter(|l| !l.trim().is_empty()).count() > pre_count
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        tries += 1;
    }

    let _ = proxy_b.kill();
    let _ = proxy_b.wait();

    let final_content = std::fs::read_to_string(&ledger_path)
        .expect("ledger.jsonl must exist after restart and second request");
    let final_rows: Vec<serde_json::Value> = final_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!(
                    "ledger row not valid JSON after restart: {} (line: {:?})",
                    e, l
                )
            })
        })
        .collect();
    assert!(
        final_rows.len() > pre_count,
        "expected new ledger row appended after restart (had {}, now {}): {}",
        pre_count,
        final_rows.len(),
        final_content
    );
    // Confirm pre-crash rows are still byte-identical at the front of the file
    // — no truncation, no rewrite during restart.
    assert!(
        final_content.starts_with(&after_crash),
        "post-restart ledger must start with pre-crash bytes (append-only invariant)"
    );

    // Confirm the new row carries the second session id.
    let last = final_rows.last().expect("at least one row");
    assert_eq!(
        last["session"].as_str(),
        Some("crash-sess-2"),
        "post-restart row should carry the second session id"
    );
}

// ===========================================================================
// /hook/event endpoint tests (Task #4)
// ===========================================================================

/// Helper for /hook/event tests: spawn proxy in tempdir, return (child, port,
/// tempdir handle). Caller drives requests against `http://127.0.0.1:<port>/hook/event`.
async fn spawn_hook_proxy(port: u16) -> (std::process::Child, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // No upstream is needed for /hook/event — pass a bogus URL just so the env
    // var is set (the proxy never contacts upstream for hook events).
    let mut child = spawn_proxy(port, "http://127.0.0.1:1", Some(tmp.path()));
    // Wait for the port to come up; if the binary fails to start, surface that.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(5000);
    loop {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("hook proxy did not come up on port {} within 5000ms", port);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    (child, tmp)
}

fn read_hooks_jsonl(dir: &std::path::Path) -> Vec<serde_json::Value> {
    let path = dir.join(".l1.5/hooks.jsonl");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("hooks.jsonl missing at {:?}: {}", path, e));
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("hooks.jsonl row not valid JSON: {} (line: {:?})", e, l))
        })
        .collect()
}

#[tokio::test]
async fn hook_endpoint_accepts_all_five_event_types_and_writes_correct_rows() {
    // Each of the 5 valid Claude Code event types should:
    //   * return 200 on POST /hook/event
    //   * append exactly one row to .l1.5/hooks.jsonl carrying the matching
    //     event_type, the workspace_id and session_id we sent, and our payload.
    let (mut proxy, tmp) = spawn_hook_proxy(8097).await;
    let url = "http://127.0.0.1:8097/hook/event";
    let client = reqwest::Client::new();

    // Order matters only for the row sequence we assert at the end.
    let events = [
        ("SessionStart", json!({"reason": "boot"})),
        ("UserPromptSubmit", json!({"prompt": "hi"})),
        ("PreToolUse", json!({"tool": "echo"})),
        ("PostToolUse", json!({"tool": "echo", "ok": true})),
        ("Stop", json!({"reason": "user"})),
    ];

    for (idx, (kind, payload)) in events.iter().enumerate() {
        let body = json!({
            "type": kind,
            "workspace_id": "ws-hook-test",
            "session_id": format!("sess-hook-{}", idx),
            "payload": payload,
        });
        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("POST /hook/event failed for {}: {}", kind, e));
        assert_eq!(
            resp.status().as_u16(),
            200,
            "expected 200 for valid event {}, got {}",
            kind,
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.expect("response is JSON");
        assert_eq!(
            body["ok"],
            json!(true),
            "valid event response should include {{\"ok\":true}}"
        );
    }

    // Allow filesystem flush before reading.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let _ = proxy.kill();
    let _ = proxy.wait();

    let rows = read_hooks_jsonl(tmp.path());
    assert_eq!(
        rows.len(),
        events.len(),
        "expected one hooks.jsonl row per event, got {} rows for {} events",
        rows.len(),
        events.len()
    );

    for (idx, (kind, payload)) in events.iter().enumerate() {
        let row = &rows[idx];
        assert_eq!(
            row["event_type"].as_str(),
            Some(*kind),
            "row {} has wrong event_type: {:?}",
            idx,
            row
        );
        assert_eq!(
            row["workspace_id"].as_str(),
            Some("ws-hook-test"),
            "row {} has wrong workspace_id: {:?}",
            idx,
            row
        );
        assert_eq!(
            row["session_id"].as_str(),
            Some(format!("sess-hook-{}", idx).as_str()),
            "row {} has wrong session_id: {:?}",
            idx,
            row
        );
        assert_eq!(
            &row["payload"], payload,
            "row {} payload mismatch: {:?} vs {:?}",
            idx, row["payload"], payload
        );
        assert!(
            row["timestamp"].is_string() || row["timestamp"].is_number(),
            "row {} timestamp should be present: {:?}",
            idx,
            row
        );
    }
}

#[tokio::test]
async fn hook_endpoint_accepts_snake_case_event_aliases() {
    // The proxy advertises snake_case as a shell-hook ergonomic alias; verify
    // each one round-trips into the canonical PascalCase event_type field.
    let (mut proxy, tmp) = spawn_hook_proxy(8098).await;
    let url = "http://127.0.0.1:8098/hook/event";
    let client = reqwest::Client::new();

    let aliases = [
        ("session_start", "SessionStart"),
        ("user_prompt_submit", "UserPromptSubmit"),
        ("pre_tool_use", "PreToolUse"),
        ("post_tool_use", "PostToolUse"),
        ("stop", "Stop"),
    ];

    for (snake, _expected) in aliases.iter() {
        let resp = client
            .post(url)
            .json(&json!({
                "type": snake,
                "workspace_id": "w",
                "session_id": "s",
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status().as_u16(),
            200,
            "snake_case alias {} should be accepted, got {}",
            snake,
            resp.status()
        );
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let _ = proxy.kill();
    let _ = proxy.wait();

    let rows = read_hooks_jsonl(tmp.path());
    assert_eq!(
        rows.len(),
        aliases.len(),
        "expected one row per alias, got {}: {:?}",
        rows.len(),
        rows
    );
    for (idx, (_, expected)) in aliases.iter().enumerate() {
        assert_eq!(
            rows[idx]["event_type"].as_str(),
            Some(*expected),
            "row {} should normalize snake_case alias to {}: {:?}",
            idx,
            expected,
            rows[idx]
        );
    }
}

#[tokio::test]
async fn hook_endpoint_rejects_unknown_event_type_with_400() {
    // Unknown `type` strings (not in the 5 PascalCase or 5 snake_case names)
    // must be rejected with 400 and produce no hooks.jsonl row.
    let (mut proxy, tmp) = spawn_hook_proxy(8099).await;
    let url = "http://127.0.0.1:8099/hook/event";
    let client = reqwest::Client::new();

    let resp = client
        .post(url)
        .json(&json!({
            "type": "NotARealHookKind",
            "workspace_id": "w",
            "session_id": "s",
        }))
        .send()
        .await
        .expect("send");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.expect("response is JSON");

    let _ = proxy.kill();
    let _ = proxy.wait();

    assert_eq!(
        status, 400,
        "unknown event type must return 400, got {} (body: {})",
        status, body
    );
    assert_eq!(
        body["type"],
        json!("error"),
        "400 body should be the proxy's error envelope: {}",
        body
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .map(|m| m.contains("NotARealHookKind"))
            .unwrap_or(false),
        "error message should name the offending type: {}",
        body
    );
    // No row should have been written.
    let path = tmp.path().join(".l1.5/hooks.jsonl");
    assert!(
        !path.exists() || std::fs::read_to_string(&path).unwrap().trim().is_empty(),
        "hooks.jsonl must not record an invalid event"
    );
}

#[tokio::test]
async fn hook_endpoint_rejects_malformed_json_with_400() {
    // Bytes that aren't valid JSON must be rejected by axum's Json extractor
    // with 400 (not 200) and must not produce a hooks.jsonl row.
    let (mut proxy, tmp) = spawn_hook_proxy(8100).await;
    let url = "http://127.0.0.1:8100/hook/event";
    let client = reqwest::Client::new();

    let cases = [
        ("not json at all", "this is not JSON {{{"),
        ("truncated object", "{\"type\":\"SessionStart\""),
        ("empty body", ""),
    ];

    for (label, body_str) in cases.iter() {
        let resp = client
            .post(url)
            .header("content-type", "application/json")
            .body(body_str.to_string())
            .send()
            .await
            .expect("send");
        assert_eq!(
            resp.status().as_u16(),
            400,
            "case {:?}: malformed JSON must return 400, got {}",
            label,
            resp.status()
        );
    }

    let _ = proxy.kill();
    let _ = proxy.wait();

    let path = tmp.path().join(".l1.5/hooks.jsonl");
    assert!(
        !path.exists() || std::fs::read_to_string(&path).unwrap().trim().is_empty(),
        "hooks.jsonl must not record any rows for malformed JSON requests"
    );
}

#[tokio::test]
async fn hook_endpoint_rejects_missing_required_fields_with_4xx() {
    // The HookEventRequest schema requires `type`, `workspace_id`, and
    // `session_id`. Omitting any of them must produce a 4xx response and
    // must not write a hooks.jsonl row. (axum's Json extractor returns 422
    // for schema violations by default; the contract here is "the proxy
    // refuses to write a partial event," which 422 satisfies.)
    let (mut proxy, tmp) = spawn_hook_proxy(8101).await;
    let url = "http://127.0.0.1:8101/hook/event";
    let client = reqwest::Client::new();

    let cases = [
        (
            "missing workspace_id",
            json!({"type": "SessionStart", "session_id": "s"}),
        ),
        (
            "missing session_id",
            json!({"type": "SessionStart", "workspace_id": "w"}),
        ),
        (
            "missing type",
            json!({"workspace_id": "w", "session_id": "s"}),
        ),
        ("empty object", json!({})),
    ];

    for (label, body) in cases.iter() {
        let resp = client.post(url).json(body).send().await.expect("send");
        let status = resp.status().as_u16();
        assert!(
            (400..500).contains(&status),
            "case {:?}: missing required field must return a 4xx, got {} (body sent: {})",
            label,
            status,
            body
        );
        // Specifically, the proxy must not return 200 for an incomplete event.
        assert_ne!(
            status, 200,
            "case {:?}: must NOT return 200 for body missing required fields ({})",
            label, body
        );
    }

    let _ = proxy.kill();
    let _ = proxy.wait();

    let path = tmp.path().join(".l1.5/hooks.jsonl");
    assert!(
        !path.exists() || std::fs::read_to_string(&path).unwrap().trim().is_empty(),
        "hooks.jsonl must not record rows for any incomplete-body request"
    );
}

#[tokio::test]
async fn hook_endpoint_writes_null_payload_when_omitted() {
    // The `payload` field is optional. When the client omits it, the
    // hooks.jsonl row should still be written with payload: null (rather
    // than rejecting the request or writing a row missing the key).
    let (mut proxy, tmp) = spawn_hook_proxy(8102).await;
    let url = "http://127.0.0.1:8102/hook/event";
    let client = reqwest::Client::new();

    let resp = client
        .post(url)
        .json(&json!({
            "type": "SessionStart",
            "workspace_id": "w",
            "session_id": "s",
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "valid event with no payload must return 200, got {}",
        resp.status()
    );

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let _ = proxy.kill();
    let _ = proxy.wait();

    let rows = read_hooks_jsonl(tmp.path());
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one row, got {}",
        rows.len()
    );
    assert_eq!(
        rows[0]["payload"],
        serde_json::Value::Null,
        "omitted payload must serialize as JSON null, got {:?}",
        rows[0]["payload"]
    );
    assert_eq!(rows[0]["event_type"].as_str(), Some("SessionStart"));
}

// ===========================================================================
// Passthrough mode tests (Task #4)
// ===========================================================================
//
// These tests prove that OSTK_CACHE_PASSTHROUGH=1 makes the proxy forward
// the original request body byte-for-byte (modulo header housekeeping) and
// that the ledger row carries mode="passthrough" — and conversely that the
// default (unset) behavior still produces the mutate-mode rewrites and
// mode="mutate" rows.

/// Spawn the proxy with an explicit `OSTK_CACHE_PASSTHROUGH` env value and
/// optional cwd. Returns the child handle once the port is listening.
fn spawn_proxy_with_passthrough(
    proxy_port: u16,
    upstream_url: &str,
    passthrough_value: Option<&str>,
    cwd: Option<&std::path::Path>,
) -> std::process::Child {
    let bin = env!("CARGO_BIN_EXE_ostk-cache");
    let mut cmd = Command::new(bin);
    cmd.env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url);
    match passthrough_value {
        Some(v) => {
            cmd.env("OSTK_CACHE_PASSTHROUGH", v);
        }
        None => {
            cmd.env_remove("OSTK_CACHE_PASSTHROUGH");
        }
    }
    if let Some(p) = cwd {
        cmd.current_dir(p);
    }
    cmd.spawn().expect("Failed to start proxy")
}

#[tokio::test]
async fn passthrough_mode_forwards_body_byte_identical_no_mutations() {
    // With OSTK_CACHE_PASSTHROUGH=1 the proxy must NOT:
    //   - collapse the system field into the [{type:"text", cache_control:1h}]
    //     wrapper,
    //   - prepend a HUD text block to the last user message,
    //   - strip cache_control from user message blocks.
    //
    // We send a request whose system is a plain string and whose last user
    // message is an array of blocks where one block carries cache_control.
    // After the proxy forwards it, the upstream-received body must be
    // structurally identical to what we sent.
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8103;
    let mut proxy = spawn_proxy_with_passthrough(proxy_port, &upstream_url, Some("1"), None);
    wait_for_proxy(proxy_port, 5000).await;

    let original_body = json!({
        "system": "You are a helpful assistant.",
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "block with cache_control",
                        "cache_control": {"type": "ephemeral", "ttl": "5m"}
                    },
                    {
                        "type": "text",
                        "text": "trailing block, no cache_control"
                    }
                ]
            }
        ],
        "model": "claude-test"
    });

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);
    let original_for_send = original_body.clone();
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let _ = client
            .post(&proxy_url)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "passthrough-noop-test")
            .json(&original_for_send)
            .send()
            .await
            .map(|r| r.bytes());
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let req_bytes = read_full_http_request(&mut up).await;
    write_usage_response(&mut up, 100).await;
    let _ = req_task.await;

    let _ = proxy.kill();
    let _ = proxy.wait();

    let body_idx =
        find_subseq(&req_bytes, b"\r\n\r\n").expect("upstream req has body separator") + 4;
    let body = &req_bytes[body_idx..];
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "upstream body must be valid JSON, got error {}: {:?}",
            e,
            String::from_utf8_lossy(body)
        )
    });

    // (1) system field is the original plain string — NOT the
    //     [{type:"text", cache_control:{ttl:"1h"}}] mutate-mode wrapper.
    assert!(
        parsed["system"].is_string(),
        "passthrough must keep system as a plain string, got {:?}",
        parsed["system"]
    );
    assert_eq!(
        parsed["system"].as_str(),
        Some("You are a helpful assistant."),
        "passthrough must forward the original system string verbatim, got {:?}",
        parsed["system"]
    );

    // (2) The last user message has the same number of blocks we sent —
    //     no HUD block was prepended.
    let last_msg = parsed["messages"]
        .as_array()
        .and_then(|a| a.last())
        .expect("messages array should have last");
    let content = last_msg["content"]
        .as_array()
        .expect("user content should still be an array in passthrough mode");
    assert_eq!(
        content.len(),
        2,
        "passthrough must NOT prepend a HUD block; expected 2 user content blocks, got {}: {:?}",
        content.len(),
        content
    );

    // (2b) None of the forwarded user blocks should be a HUD-style block
    //      (no `cache:` text, no 5m TTL block at index 0 we didn't send).
    let has_hud_text = content.iter().any(|b| {
        b.get("type").and_then(|t| t.as_str()) == Some("text")
            && b.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.contains("cache:") && s.contains("amp="))
                .unwrap_or(false)
    });
    assert!(
        !has_hud_text,
        "passthrough must NOT inject a HUD text block, got blocks: {:?}",
        content
    );

    // (3) The first block is the one we sent (with its cache_control intact).
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "block with cache_control");
    assert_eq!(
        content[0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "5m"}),
        "passthrough must NOT strip cache_control from user message blocks; \
         expected the original 5m ephemeral marker, got {:?}",
        content[0].get("cache_control")
    );

    // (4) The second block is unchanged.
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "trailing block, no cache_control");

    // (5) Strong byte-identity check: the JSON the proxy forwarded must
    //     parse equal to the JSON we sent. This catches any silent
    //     reshape or extra-key insertion that the field-by-field asserts
    //     above might miss.
    assert_eq!(
        parsed, original_body,
        "passthrough-forwarded body must equal the original request body byte-for-byte (semantically). \
         original: {}\nforwarded: {}",
        original_body, parsed
    );
}

#[tokio::test]
async fn mutate_mode_default_still_collapses_system_and_prepends_hud() {
    // Regression check: when OSTK_CACHE_PASSTHROUGH is unset (default
    // mutate mode), the proxy must still rewrite system into the wrapped
    // [{type:"text", cache_control:1h}] form AND prepend a HUD text block
    // to the last (non-tool-result) user message. This is the contract the
    // existing `proxy_firmware_byte_stability_across_user_message_length`
    // test relies on; we re-pin it here so the new passthrough branch
    // can't silently change default behavior.
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8104;
    let mut proxy = spawn_proxy_with_passthrough(
        proxy_port,
        &upstream_url,
        None, // env var unset → default mutate mode
        None,
    );
    wait_for_proxy(proxy_port, 5000).await;

    let req_body = json!({
        "system": "Plain system string",
        "messages": [
            {"role": "user", "content": "Hello"}
        ],
        "model": "claude-test"
    });

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);
    let req_task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let _ = client
            .post(&proxy_url)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "default-mutate-test")
            .json(&req_body)
            .send()
            .await
            .map(|r| r.bytes());
    });

    let (mut up, _) = mock_upstream.accept().await.unwrap();
    let req_bytes = read_full_http_request(&mut up).await;
    write_usage_response(&mut up, 50).await;
    let _ = req_task.await;

    let _ = proxy.kill();
    let _ = proxy.wait();

    let body_idx = find_subseq(&req_bytes, b"\r\n\r\n").expect("body separator") + 4;
    let body = &req_bytes[body_idx..];
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "upstream body must be valid JSON, got {}: {:?}",
            e,
            String::from_utf8_lossy(body)
        )
    });

    // System collapsed into the wrapped array form with 1h cache_control.
    let sys = &parsed["system"];
    assert!(
        sys.is_array(),
        "default mutate mode must collapse system into an array, got {:?}",
        sys
    );
    let sys_arr = sys.as_array().unwrap();
    assert_eq!(
        sys_arr.len(),
        1,
        "system array should have one element, got {:?}",
        sys_arr
    );
    assert_eq!(sys_arr[0]["type"], "text");
    assert_eq!(sys_arr[0]["text"], "Plain system string");
    assert_eq!(
        sys_arr[0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"}),
        "system block must carry the 1h ephemeral marker in mutate mode, got {:?}",
        sys_arr[0].get("cache_control")
    );

    // HUD prepended to user content (now an array with a HUD block first).
    let last_msg = parsed["messages"]
        .as_array()
        .and_then(|a| a.last())
        .expect("last message");
    let content = last_msg["content"]
        .as_array()
        .expect("default mode rewrites user content into an array");
    assert!(
        content.len() >= 2,
        "default mutate must prepend HUD block (≥2 entries), got {:?}",
        content
    );

    let first = &content[0];
    assert_eq!(first["type"], "text");
    let hud_text = first["text"].as_str().expect("HUD text should be a string");
    assert!(
        hud_text.contains("cache:") && hud_text.contains("amp="),
        "first user block should be the HUD line ('cache: ... amp=...'), got {:?}",
        hud_text
    );
    assert_eq!(
        first["cache_control"],
        json!({"type": "ephemeral", "ttl": "5m"}),
        "HUD block must carry the 5m ephemeral marker, got {:?}",
        first.get("cache_control")
    );
}

#[tokio::test]
async fn ledger_row_mode_field_reflects_passthrough_env_var() {
    // Phase A: run proxy with OSTK_CACHE_PASSTHROUGH=1 in tempdir A, fire
    //          one request, kill, expect ledger row with mode="passthrough".
    // Phase B: run proxy with OSTK_CACHE_PASSTHROUGH unset in tempdir B,
    //          fire one request, kill, expect ledger row with mode="mutate".
    //
    // We use two separate tempdirs so the two ledgers are independent —
    // no cross-contamination if either phase writes more than one row.
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    // ---- Phase A: passthrough ----
    let tmp_a = tempfile::tempdir().expect("tempdir A");
    let proxy_port_a = 8105;
    let mut proxy_a =
        spawn_proxy_with_passthrough(proxy_port_a, &upstream_url, Some("1"), Some(tmp_a.path()));
    wait_for_proxy(proxy_port_a, 5000).await;

    let url_a = format!("http://127.0.0.1:{}/v1/messages", proxy_port_a);
    let req_task_a = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&url_a)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "passthrough-ledger-a")
            .json(&json!({"system": "S", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await;
        if let Ok(r) = resp {
            let _ = r.bytes().await;
        }
    });

    let (mut up_a, _) = mock_upstream.accept().await.unwrap();
    let _ = read_full_http_request(&mut up_a).await;
    write_usage_response(&mut up_a, 42).await;
    let _ = req_task_a.await;

    let ledger_a = tmp_a.path().join(".ostk/memory/ledger.jsonl");
    let mut tries = 0;
    while tries < 100 {
        if let Ok(s) = std::fs::read_to_string(&ledger_a)
            && s.lines().filter(|l| !l.trim().is_empty()).count() >= 1
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        tries += 1;
    }
    let _ = proxy_a.kill();
    let _ = proxy_a.wait();

    let content_a =
        std::fs::read_to_string(&ledger_a).expect("ledger.jsonl must exist after Phase A request");
    let rows_a: Vec<serde_json::Value> = content_a
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!("Phase A ledger row not valid JSON: {} (line: {:?})", e, l)
            })
        })
        .collect();
    assert!(
        !rows_a.is_empty(),
        "Phase A: expected at least one ledger row, got: {:?}",
        content_a
    );
    let mode_a = rows_a[0]["mode"]
        .as_str()
        .unwrap_or_else(|| panic!("Phase A row must include mode field, got: {}", rows_a[0]));
    assert_eq!(
        mode_a, "passthrough",
        "Phase A: OSTK_CACHE_PASSTHROUGH=1 must produce mode=\"passthrough\" rows, got {:?} (full row: {})",
        mode_a, rows_a[0]
    );
    assert_eq!(
        rows_a[0]["session"].as_str(),
        Some("passthrough-ledger-a"),
        "Phase A row should carry the session id we sent: {}",
        rows_a[0]
    );

    // ---- Phase B: default mutate (env var unset) ----
    let tmp_b = tempfile::tempdir().expect("tempdir B");
    let proxy_port_b = 8106;
    let mut proxy_b =
        spawn_proxy_with_passthrough(proxy_port_b, &upstream_url, None, Some(tmp_b.path()));
    wait_for_proxy(proxy_port_b, 5000).await;

    let url_b = format!("http://127.0.0.1:{}/v1/messages", proxy_port_b);
    let req_task_b = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let resp = client
            .post(&url_b)
            .header("x-api-key", "k")
            .header("anthropic-session-id", "mutate-ledger-b")
            .json(&json!({"system": "S", "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await;
        if let Ok(r) = resp {
            let _ = r.bytes().await;
        }
    });

    let (mut up_b, _) = mock_upstream.accept().await.unwrap();
    let _ = read_full_http_request(&mut up_b).await;
    write_usage_response(&mut up_b, 84).await;
    let _ = req_task_b.await;

    let ledger_b = tmp_b.path().join(".ostk/memory/ledger.jsonl");
    let mut tries = 0;
    while tries < 100 {
        if let Ok(s) = std::fs::read_to_string(&ledger_b)
            && s.lines().filter(|l| !l.trim().is_empty()).count() >= 1
        {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        tries += 1;
    }
    let _ = proxy_b.kill();
    let _ = proxy_b.wait();

    let content_b =
        std::fs::read_to_string(&ledger_b).expect("ledger.jsonl must exist after Phase B request");
    let rows_b: Vec<serde_json::Value> = content_b
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!("Phase B ledger row not valid JSON: {} (line: {:?})", e, l)
            })
        })
        .collect();
    assert!(
        !rows_b.is_empty(),
        "Phase B: expected at least one ledger row, got: {:?}",
        content_b
    );
    let mode_b = rows_b[0]["mode"]
        .as_str()
        .unwrap_or_else(|| panic!("Phase B row must include mode field, got: {}", rows_b[0]));
    assert_eq!(
        mode_b, "mutate",
        "Phase B: unset OSTK_CACHE_PASSTHROUGH must produce mode=\"mutate\" rows, got {:?} (full row: {})",
        mode_b, rows_b[0]
    );
    assert_eq!(
        rows_b[0]["session"].as_str(),
        Some("mutate-ledger-b"),
        "Phase B row should carry the session id we sent: {}",
        rows_b[0]
    );
}

#[tokio::test]
async fn hop_header_ingress_guard_rejects_loops_on_all_handlers() {
    // →2035 fix 4(b): a request carrying our own hop header is a self-loop
    // and must be rejected with 508 on every routing surface — including
    // the catchall, which is where loops on unrecognised paths recurse.
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    let proxy_port = 8214;
    let mut proxy_process = spawn_proxy(proxy_port, &upstream_url, None);
    wait_for_proxy(proxy_port, 10_000).await;

    let client = reqwest::Client::new();
    let cases = [
        ("/v1/messages", "anthropic handler"),
        ("/v1/responses", "openai handler"),
        ("/v1/some/unknown/path", "catchall handler"),
    ];
    for (path, label) in cases {
        let url = format!("http://127.0.0.1:{}{}", proxy_port, path);
        let resp = client
            .post(&url)
            .header("x-ostk-cache-hop", "1")
            .header("anthropic-session-id", "loop-test")
            .json(&json!({"messages": []}))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{}: request failed: {}", label, e));
        assert_eq!(
            resp.status().as_u16(),
            508,
            "{} must reject hop-header requests with 508, got {}",
            label,
            resp.status()
        );
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("loop_detected"),
            "{}: 508 body should name loop_detected, got: {}",
            label,
            body
        );
    }

    let _ = proxy_process.kill();
    let _ = proxy_process.wait();
}
