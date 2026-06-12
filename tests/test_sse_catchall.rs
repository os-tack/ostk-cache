use serde_json::json;
use std::process::Command;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn catchall_infers_sse_from_accept_header_when_content_type_missing() {
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
    
    let request_task = tokio::spawn(async move {
        let _ = client
            .post(&proxy_url)
            .header("accept", "text/event-stream")
            .header("session-id", "test-session")
            .body("some payload")
            .send()
            .await;
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let mut buf = vec![0u8; 1024];
    let _ = stream.read(&mut buf).await.unwrap();

    let response = "HTTP/1.1 200 OK\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n\
                    C\r\n\
                    data: hello\n\n\r\n\
                    0\r\n\
                    \r\n";
    stream.write_all(response.as_bytes()).await.unwrap();
    drop(stream);

    let _ = request_task.await;

    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    let _ = proxy_process.kill();
    let _ = proxy_process.wait();

    let mut found_meta = false;
    let mut stack = vec![capture_dir.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_type().unwrap().is_dir() {
                    stack.push(entry.path());
                } else if entry.file_name() == "metadata.json" {
                    let content = std::fs::read_to_string(entry.path()).unwrap();
                    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
                    assert_eq!(parsed["is_sse"].as_bool(), Some(true), "is_sse should be true when Accept header asks for event-stream");
                    found_meta = true;
                }
            }
        }
    }
    assert!(found_meta, "metadata.json not found in capture dir");
}
