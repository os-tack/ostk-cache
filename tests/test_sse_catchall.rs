use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn catchall_infers_sse_from_accept_header_only_when_content_type_missing() {
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();

    let capture_dir = tempfile::tempdir().unwrap();

    let proxy_port = 8092;
    let mut proxy_process = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("ostk-cache")
        .arg("--")
        .arg("--port")
        .arg(proxy_port.to_string())
        .arg("--provider")
        .arg("gpt")
        .arg("--upstream")
        .arg(format!("http://127.0.0.1:{}", upstream_addr.port()))
        .arg("--mode")
        .arg("passthrough")
        .arg("--capture-http")
        .arg("--capture-http-dir")
        .arg(capture_dir.path())
        .spawn()
        .expect("Failed to start proxy");

    tokio::time::sleep(tokio::time::Duration::from_millis(2500)).await;

    let proxy_url = format!("http://127.0.0.1:{}/backend-api/codex/responses", proxy_port);
    let client = reqwest::Client::new();
    
    // Test Case 1: Positive case (No content-type, Accept: text/event-stream) -> is_sse = true
    let url1 = proxy_url.clone();
    let request_task1 = tokio::spawn(async move {
        let _ = client
            .post(&url1)
            .header("accept", "text/event-stream")
            .header("x-session-id", "test-session-1")
            .body("some payload")
            .send()
            .await;
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let mut buf = vec![0u8; 1024];
    let _ = stream.read(&mut buf).await.unwrap();

    let response1 = "HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n\
                    C\r\n\
                    data: hello\n\n\r\n\
                    0\r\n\
                    \r\n";
    stream.write_all(response1.as_bytes()).await.unwrap();
    drop(stream);
    let _ = request_task1.await;

    // Test Case 2: Negative case (Content-Type: application/json, Accept: text/event-stream) -> is_sse = false
    let client2 = reqwest::Client::new();
    let url2 = proxy_url.clone();
    let request_task2 = tokio::spawn(async move {
        let _ = client2
            .post(&url2)
            .header("accept", "text/event-stream")
            .header("x-session-id", "test-session-2")
            .body("some payload")
            .send()
            .await;
    });

    let (mut stream2, _) = mock_upstream.accept().await.unwrap();
    let _ = stream2.read(&mut buf).await.unwrap();

    let response2 = "HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\
                    Content-Type: application/json\r\n\
                    \r\n\
                    C\r\n\
                    data: hello\n\n\r\n\
                    0\r\n\
                    \r\n";
    stream2.write_all(response2.as_bytes()).await.unwrap();
    drop(stream2);
    let _ = request_task2.await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let _ = proxy_process.kill();
    let _ = proxy_process.wait();

    let mut found_meta1 = false;
    let mut found_meta2 = false;
    let mut stack = vec![capture_dir.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else if entry.file_name() == "metadata.json" {
                    let content = std::fs::read_to_string(entry.path()).unwrap();
                    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
                    let session_id = parsed["session"].as_str().unwrap_or("");
                    
                    if session_id == "test-session-1" {
                        assert_eq!(parsed["is_sse"].as_bool(), Some(true), "TC1: is_sse should be true when no Content-Type");
                        found_meta1 = true;
                    } else if session_id == "test-session-2" {
                        assert_eq!(parsed["is_sse"].as_bool(), Some(false), "TC2: is_sse should be false when Content-Type is application/json");
                        found_meta2 = true;
                    }
                }
            }
        }
    }
    assert!(found_meta1, "metadata.json for TC1 not found in capture dir");
    assert!(found_meta2, "metadata.json for TC2 not found in capture dir");
}
