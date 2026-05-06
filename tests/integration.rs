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
