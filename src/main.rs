use dashmap::DashMap;
use ostk_cache::{
    AnthropicRequest, ProviderUsage, SessionId, account, persist_amp_row, project_hud,
};
use serde_json::json;
use sha2::Digest;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Default, Clone, Debug)]
struct AmpAccumulator {
    cumulative_amp_mean: f64,
    turns_seen: u64,
    stored_count: usize,
    hot_count: usize,
}

type AmpStore = Arc<DashMap<SessionId, AmpAccumulator>>;

#[tokio::main]
async fn main() {
    let port = std::env::var("PROXY_PORT").unwrap_or_else(|_| "8080".to_string());
    let bind_addr = format!("127.0.0.1:{}", port);
    let listener = match TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[proxy] fatal: bind {} failed: {}", bind_addr, e);
            std::process::exit(1);
        }
    };
    println!("Capture Proxy running on 8080");

    let amp_store: AmpStore = Arc::new(DashMap::new());

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[proxy] accept error: {} (continuing)", e);
                continue;
            }
        };
        let store = Arc::clone(&amp_store);
        tokio::spawn(async move {
            handle_connection(stream, store).await;
        });
    }
}

async fn handle_connection(mut stream: tokio::net::TcpStream, amp_store: AmpStore) {
    let mut buffer = vec![0u8; 1024 * 1024];
    let bytes_read = match stream.read(&mut buffer).await {
        Ok(0) => return,
        Ok(n) => n,
        Err(e) => {
            eprintln!("[proxy] stream read error: {}", e);
            return;
        }
    };

    let request_str = String::from_utf8_lossy(&buffer[..bytes_read]);
    println!("--- INTERCEPTED REQUEST ---");

    let req_string = request_str.to_string();
    let parts: Vec<&str> = req_string.splitn(2, "\r\n\r\n").collect();
    if parts.len() < 2 {
        let _ = write_client_error(&mut stream, 400, "missing request body").await;
        return;
    }
    let headers = parts[0];
    let body = parts[1];

    let mut api_key = String::new();
    let mut anthropic_version = String::new();
    let mut session_header: Option<String> = None;
    for line in headers.lines() {
        let lower = line.to_lowercase();
        if let Some(rest) = line.get(10..).filter(|_| lower.starts_with("x-api-key:")) {
            api_key = rest.trim().to_string();
        } else if let Some(rest) = line
            .get(18..)
            .filter(|_| lower.starts_with("anthropic-version:"))
        {
            anthropic_version = rest.trim().to_string();
        } else if lower.starts_with("anthropic-session-id:")
            && let Some(rest) = line.get("anthropic-session-id:".len()..)
        {
            session_header = Some(rest.trim().to_string());
        }
    }

    let workspace = ostk_cache::Workspace::from_path(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    )
    .unwrap_or_else(|_| ostk_cache::Workspace {
        priority_hash: "unknown".to_string(),
        source: ostk_cache::WorkspaceSource::Cwd,
    });

    let session_id: SessionId = session_header.unwrap_or_else(|| {
        format!("{}:{}", workspace.priority_hash, {
            let mut h = sha2::Sha256::new();
            h.update(api_key.as_bytes());
            format!("{:x}", h.finalize())[..12].to_string()
        })
    });

    let prior_amp = {
        amp_store.get(&session_id).map(|r| r.clone()).unwrap_or_default()
    };

    let (payload, parse_failed) = match serde_json::from_str::<AnthropicRequest>(body) {
        Ok(mut req) => {
            let firmware: String = match &req.system {
                Some(sys) if sys.is_string() => sys.as_str().unwrap().to_string(),
                Some(sys) if sys.is_array() => sys
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };

            req.system = Some(json!([
                {
                    "type": "text",
                    "text": firmware,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ]));

            if let Some(last_msg) = req.messages.iter_mut().rev().find(|m| m.role == "user") {
                let amp_for_hud = if prior_amp.turns_seen == 0 {
                    1.0
                } else {
                    prior_amp.cumulative_amp_mean
                };
                let hud = project_hud(amp_for_hud, prior_amp.stored_count, prior_amp.hot_count);

                let mut new_content_array = Vec::new();

                // 1. Add HUD as the first text block with the 5m ephemeral marker
                new_content_array.push(json!({
                    "type": "text",
                    "text": format!("{}\n", hud),
                    "cache_control": {"type": "ephemeral", "ttl": "5m"}
                }));

                // 2. Preserve original content (including images/documents)
                // stripping existing cache_control markers to avoid Anthropic's 4-marker limit
                if let Some(s) = last_msg.content.as_str() {
                    new_content_array.push(json!({
                        "type": "text",
                        "text": s
                    }));
                } else if let Some(arr) = last_msg.content.as_array() {
                    for item in arr {
                        let mut block = item.clone();
                        if let Some(obj) = block.as_object_mut() {
                            obj.remove("cache_control");
                        }
                        new_content_array.push(block);
                    }
                }

                last_msg.content = json!(new_content_array);
            }

            match serde_json::to_string(&req) {
                Ok(s) => (s, false),
                Err(_) => (body.to_string(), false),
            }
        }
        Err(_) => (body.to_string(), true),
    };

    if parse_failed {
        let _ = write_client_error(&mut stream, 400, "invalid JSON request body").await;
        return;
    }

    let client = reqwest::Client::new();
    let base_url = std::env::var("ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let url = format!("{}/v1/messages", base_url);
    let mut req_builder = client.post(url);

    if !api_key.is_empty() {
        req_builder = req_builder.header("x-api-key", api_key);
    }
    if !anthropic_version.is_empty() {
        req_builder = req_builder.header("anthropic-version", anthropic_version);
    }
    req_builder = req_builder.header("content-type", "application/json");

    let res = req_builder.body(payload).send().await;

    let mut response = match res {
        Ok(r) => r,
        Err(e) => {
            let body = json!({
                "type": "error",
                "error": {"type": "upstream_error", "message": format!("{}", e)}
            })
            .to_string();
            let _ = write_status(&mut stream, 502, "Bad Gateway", &body).await;
            return;
        }
    };

    let status = response.status();
    let resp_headers = response.headers().clone();
    let mut is_chunked = false;
    let mut is_sse = false;

    let mut resp_str = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Unknown")
    );
    for (k, v) in resp_headers.iter() {
        let key_lower = k.as_str().to_lowercase();
        let val_str = v.to_str().unwrap_or("");
        if key_lower == "transfer-encoding" && val_str.to_lowercase().contains("chunked") {
            is_chunked = true;
        }
        if key_lower == "content-type" && val_str.to_lowercase().contains("text/event-stream") {
            is_sse = true;
        }
        resp_str.push_str(&format!("{}: {}\r\n", k.as_str(), val_str));
    }
    if !is_chunked && !resp_headers.contains_key("content-length") {
        resp_str.push_str("Transfer-Encoding: chunked\r\n");
        is_chunked = true;
    }
    resp_str.push_str("\r\n");

    if let Err(e) = stream.write_all(resp_str.as_bytes()).await {
        eprintln!("[proxy] client write (headers) error: {}", e);
        return;
    }

    let mut accumulated = Vec::<u8>::new();

    loop {
        tokio::select! {
            chunk_res = response.chunk() => {
                match chunk_res {
                    Ok(Some(chunk)) => {
                        if chunk.is_empty() { continue; }
                        accumulated.extend_from_slice(&chunk);
                        let write_res = if is_chunked {
                            stream
                                .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                                .await
                                .and(stream.write_all(&chunk).await)
                                .and(stream.write_all(b"\r\n").await)
                        } else {
                            stream.write_all(&chunk).await
                        };
                        if let Err(e) = write_res {
                            eprintln!("[proxy] client write (chunk) error: {} — closing", e);
                            return;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("[proxy] upstream chunk error: {} — closing client", e);
                        return;
                    }
                }
            }
            _ = stream.readable() => {
                let mut buf = [0u8; 1];
                if stream.try_read(&mut buf).is_err() {
                    eprintln!("[proxy] client disconnected mid-stream; aborting upstream read");
                    break;
                }
            }
        }
    }

    if is_chunked {
        let _ = stream.write_all(b"0\r\n\r\n").await;
    }

    if let Some(usage) = if is_sse {
        parse_usage_from_sse(&accumulated)
    } else {
        parse_usage_from_json(&accumulated)
    } {
        let row = account(&usage, session_id.clone());
        if let Err(e) = persist_amp_row(&row) {
            eprintln!("[proxy] persist_amp_row error: {}", e);
        }

        let mut acc = amp_store.entry(session_id).or_default();
        let n = acc.turns_seen as f64;
        acc.cumulative_amp_mean = (acc.cumulative_amp_mean * n + row.amp_ratio) / (n + 1.0);
        acc.turns_seen += 1;
        // Stored/hot counts are not yet observable from upstream usage; they
        // become real once the page-table side of the daemon lands. For now
        // we surface turns_seen as a stand-in for "stored" so the HUD shows
        // movement across turns.
        acc.stored_count = acc.turns_seen as usize;
    }
}

fn parse_usage_from_json(body: &[u8]) -> Option<ProviderUsage> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let usage = v.get("usage")?;
    Some(ProviderUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
        cache_read_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
        cache_create_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0) as usize,
    })
}

fn parse_usage_from_sse(body: &[u8]) -> Option<ProviderUsage> {
    let text = std::str::from_utf8(body).ok()?;
    let mut current_event: Option<&str> = None;
    let mut input_tokens = 0u64;
    let mut cache_read = 0u64;
    let mut cache_create = 0u64;
    let mut found = false;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            current_event = None;
        } else if let Some(rest) = line.strip_prefix("event: ") {
            current_event = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data: ") {
            let event = current_event.unwrap_or("");
            if (event == "message_start" || event == "message_delta")
                && let Ok(v) = serde_json::from_str::<serde_json::Value>(rest)
            {
                let usage = v
                    .get("usage")
                    .or_else(|| v.get("message").and_then(|m| m.get("usage")));
                if let Some(u) = usage {
                    if let Some(n) = u.get("input_tokens").and_then(|x| x.as_u64()) {
                        input_tokens = n;
                        found = true;
                    }
                    if let Some(n) = u.get("cache_read_input_tokens").and_then(|x| x.as_u64()) {
                        cache_read = n;
                        found = true;
                    }
                    if let Some(n) = u
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                    {
                        cache_create = n;
                        found = true;
                    }
                }
            }
        } else if line.is_empty() {
            current_event = None;
        }
    }

    if found {
        Some(ProviderUsage {
            input_tokens: input_tokens as usize,
            cache_read_tokens: cache_read as usize,
            cache_create_tokens: cache_create as usize,
        })
    } else {
        None
    }
}

async fn write_client_error(
    stream: &mut tokio::net::TcpStream,
    code: u16,
    message: &str,
) -> std::io::Result<()> {
    let body = json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message}
    })
    .to_string();
    let reason = match code {
        400 => "Bad Request",
        502 => "Bad Gateway",
        _ => "Error",
    };
    write_status(stream, code, reason, &body).await
}

async fn write_status(
    stream: &mut tokio::net::TcpStream,
    code: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        code,
        reason,
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await
}
