use serde_json::json;
use std::process::Command;
use tokio::io::AsyncReadExt;
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
    assert_eq!(api_key_count, 1, "x-api-key header should appear exactly once, got {}", api_key_count);
    assert_eq!(auth_count, 1, "authorization header should appear exactly once, got {}", auth_count);
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
    let content = last_msg["content"].as_array().expect("content should be array");

    assert_eq!(content[0]["type"], "tool_result",
        "first block of tool_result-bearing user message must be tool_result, got {:?}",
        content[0]);

    let has_text_with_hud = content.iter().any(|b| {
        b.get("type").and_then(|t| t.as_str()) == Some("text")
            && b.get("text").and_then(|t| t.as_str()).map(|s| s.contains("cache:")).unwrap_or(false)
    });
    assert!(!has_text_with_hud,
        "no HUD text block should be inserted into a tool_result-bearing user message");
}
