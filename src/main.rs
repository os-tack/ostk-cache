use async_stream::stream;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, post},
};
use dashmap::DashMap;
use ostk_cache::{
    AnthropicRequest, ProviderUsage, SessionId, account, persist_amp_row, project_hud,
};
use serde_json::json;
use sha2::Digest;
use std::sync::Arc;
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
    println!("Capture Proxy running on {}", bind_addr);

    let amp_store: AmpStore = Arc::new(DashMap::new());

    let app = Router::new()
        .route("/v1/messages", post(handle_anthropic_message))
        .fallback(any(|| async { (StatusCode::NOT_FOUND, "Not Found") }))
        .with_state(amp_store);

    axum::serve(listener, app).await.unwrap();
}

async fn handle_anthropic_message(
    State(amp_store): State<AmpStore>,
    headers: HeaderMap,
    body_bytes: axum::body::Bytes,
) -> Result<Response, StatusCode> {
    println!("--- INTERCEPTED REQUEST ---");

    let mut api_key = String::new();
    let mut anthropic_version = String::new();
    let mut session_header: Option<String> = None;

    if let Some(v) = headers.get("x-api-key") {
        api_key = v.to_str().unwrap_or("").to_string();
    }
    if let Some(v) = headers.get("anthropic-version") {
        anthropic_version = v.to_str().unwrap_or("").to_string();
    }
    if let Some(v) = headers.get("anthropic-session-id") {
        session_header = Some(v.to_str().unwrap_or("").to_string());
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
        amp_store
            .get(&session_id)
            .map(|r| r.clone())
            .unwrap_or_default()
    };

    let body_str = String::from_utf8_lossy(&body_bytes);
    let (payload, parse_failed) = match serde_json::from_str::<AnthropicRequest>(&body_str) {
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

                new_content_array.push(json!({
                    "type": "text",
                    "text": format!("{}\n", hud),
                    "cache_control": {"type": "ephemeral", "ttl": "5m"}
                }));

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
                Err(_) => (body_str.to_string(), false),
            }
        }
        Err(_) => (body_str.to_string(), true),
    };

    if parse_failed {
        return Ok((
            StatusCode::BAD_REQUEST,
            json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": "invalid JSON request body"}
            })
            .to_string(),
        )
            .into_response());
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

    let mut response = match req_builder.body(payload).send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok((
                StatusCode::BAD_GATEWAY,
                json!({
                    "type": "error",
                    "error": {"type": "upstream_error", "message": format!("{}", e)}
                })
                .to_string(),
            )
                .into_response());
        }
    };

    let status = response.status();
    let mut resp_builder = Response::builder().status(status.as_u16());

    let mut is_sse = false;

    for (k, v) in response.headers().iter() {
        let key_lower = k.as_str().to_lowercase();
        if key_lower == "transfer-encoding"
            || key_lower == "connection"
            || key_lower == "content-length"
        {
            continue;
        }
        if key_lower == "content-type"
            && v.to_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("text/event-stream")
        {
            is_sse = true;
        }
        resp_builder = resp_builder.header(k.as_str(), v.as_bytes());
    }

    let session_id_clone = session_id.clone();

    let stream = stream! {
        let mut accumulated = Vec::<u8>::new();

        while let Ok(Some(chunk)) = response.chunk().await {
            if chunk.is_empty() {
                continue;
            }
            accumulated.extend_from_slice(&chunk);
            yield Ok::<_, std::io::Error>(chunk);
        }

        if let Some(usage) = if is_sse {
            parse_usage_from_sse(&accumulated)
        } else {
            parse_usage_from_json(&accumulated)
        } {
            let row = account(&usage, session_id_clone.clone());
            if let Err(e) = persist_amp_row(&row) {
                eprintln!("[proxy] persist_amp_row error: {}", e);
            }

            let mut acc = amp_store.entry(session_id_clone).or_default();
            let n = acc.turns_seen as f64;
            acc.cumulative_amp_mean = (acc.cumulative_amp_mean * n + row.amp_ratio) / (n + 1.0);
            acc.turns_seen += 1;
            acc.stored_count = acc.turns_seen as usize;
        }
    };

    let body = Body::from_stream(stream);
    Ok(resp_builder.body(body).unwrap())
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
                    if let Some(it) = u.get("input_tokens").and_then(|x| x.as_u64()) {
                        input_tokens += it;
                    }
                    if let Some(cr) = u.get("cache_read_input_tokens").and_then(|x| x.as_u64()) {
                        cache_read += cr;
                    }
                    if let Some(cc) = u
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                    {
                        cache_create += cc;
                    }
                    found = true;
                }
            }
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
