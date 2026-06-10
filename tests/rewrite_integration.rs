//! End-to-end integration test for the rewrite middleware (→1799).
//!
//! Spins up the proxy as a subprocess pointed at a temp `.ostk/` dir
//! pre-seeded with a FileCache entry whose SHA-256 matches a content
//! block in the request. Asserts the forwarded body has the inline
//! text swapped for `{type:"document",source:{type:"file",file_id:".."}}`.

use ostk_files_light::{FileCache, FileCacheEntry};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::process::{Command, Stdio};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn seed_cache(ostk_dir: &std::path::Path, content: &str, file_id: &str) {
    std::fs::create_dir_all(ostk_dir).unwrap();
    let mut cache = FileCache::load(ostk_dir);
    cache
        .put(FileCacheEntry {
            path: "fixture.txt".to_string(),
            file_id: file_id.to_string(),
            uploaded_at: "2026-05-06T20:00:00Z".to_string(),
            size: content.len() as u64,
            last_seen_gen: 0,
            stale: false,
            sha256: Some(sha256_hex(content.as_bytes())),
        })
        .unwrap();
}

#[tokio::test]
async fn rewrite_middleware_swaps_inline_to_file_id() {
    // Mock upstream Anthropic
    let mock_upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = mock_upstream.local_addr().unwrap();
    let upstream_url = format!("http://127.0.0.1:{}", upstream_addr.port());

    // Workspace with seeded cache. Content >= 512 bytes (default
    // min_size_threshold) so the swap actually fires.
    let tmp = tempfile::tempdir().unwrap();
    let ostk_dir = tmp.path().join(".ostk");
    let content = "A".repeat(2048);
    let file_id = "file_test_xyz_42";
    seed_cache(&ostk_dir, &content, file_id);

    let proxy_port = 8120;
    let mut proxy = Command::new("cargo")
        .arg("run")
        .arg("--bin")
        .arg("ostk-cache")
        .env("PROXY_PORT", proxy_port.to_string())
        .env("ANTHROPIC_BASE_URL", upstream_url)
        .env("OSTK_DIR", ostk_dir.to_string_lossy().into_owned())
        .env("OSTK_REWRITE_ENABLED", "1")
        // Force passthrough OFF so we can inspect the rewriter on its own.
        .env("OSTK_CACHE_PASSTHROUGH", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn proxy");

    tokio::time::sleep(tokio::time::Duration::from_millis(2500)).await;

    let req_body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "system": "system prompt",
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": content}
                ]
            }
        ]
    });

    let proxy_url = format!("http://127.0.0.1:{}/v1/messages", proxy_port);
    let client = reqwest::Client::new();
    let req_task = tokio::spawn(async move {
        let _ = client
            .post(&proxy_url)
            .header("anthropic-session-id", "rewrite-test-session")
            .json(&req_body)
            .send()
            .await;
    });

    let (mut stream, _) = mock_upstream.accept().await.unwrap();
    let mut buf = vec![0u8; 256 * 1024];
    let n = stream.read(&mut buf).await.unwrap();
    let received = String::from_utf8_lossy(&buf[..n]).to_string();

    drop(stream);
    let _ = req_task.await;
    let _ = proxy.kill();
    let _ = proxy.wait();

    // Find the body
    let body_start = received.find("\r\n\r\n").expect("HTTP body delimiter");
    let body_str = &received[body_start + 4..];
    let parsed: serde_json::Value =
        serde_json::from_str(body_str).expect("upstream body parses as JSON");

    // The user content block should have been swapped to a document
    // file reference. Walk messages[0].content and look for the swap.
    let user_content = &parsed["messages"][0]["content"];
    let arr = user_content.as_array().expect("content is an array");

    // The proxy in mutate mode prepends a HUD text block at index 0,
    // followed by the (now-swapped) original block. Either way, SOMEWHERE
    // in the array there must be a document-with-file_id block carrying
    // our seeded file_id.
    let found_swap = arr.iter().any(|b| {
        b.get("type").and_then(|t| t.as_str()) == Some("document")
            && b.get("source")
                .and_then(|s| s.get("type"))
                .and_then(|t| t.as_str())
                == Some("file")
            && b.get("source")
                .and_then(|s| s.get("file_id"))
                .and_then(|t| t.as_str())
                == Some(file_id)
    });
    assert!(
        found_swap,
        "expected a document/file block with file_id={}; got: {}",
        file_id, user_content
    );

    // The original 2048-byte 'A' string should NOT appear inline.
    assert!(
        !body_str.contains(&"A".repeat(2048)),
        "inline content was not swapped out"
    );

    // Telemetry log should exist with at least one row.
    let log = ostk_dir.join("rewrite-events.jsonl");
    assert!(
        log.exists(),
        "telemetry log should exist at {}",
        log.display()
    );
    let log_content = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_content.lines().count() >= 1,
        "expected >= 1 telemetry row, got 0"
    );
}
