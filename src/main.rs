use async_stream::stream;
use axum::{
    Json, Router,
    body::Body,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, post},
};
use clap::Parser;
use dashmap::DashMap;
use ostk_cache::config::{CliArgs, Config, Mode};
use ostk_cache::rebuild::{RebuildConfig, RebuildOutcome, apply_rebuild};
use ostk_cache::rewrite_middleware::{RewriteConfig, RewriteOutcome, apply_rewrite};
use ostk_cache::{
    AccountInput, AnthropicRequest, DaemonAdapter, HookAdapter, HookEvent, HookEventKind,
    InMemoryPageTable, ProviderUsage, SessionId, SizeMetrics, account, fmt_bytes,
    persist_amp_row, project_hud,
};
use ostk_cache_core::{
    http::{is_hop_by_hop_request_header, is_sse_content_type, should_forward_response_header},
    usage::{UsageDialect, parse_usage},
};
use serde::Deserialize;
use serde_json::json;
use sha2::Digest;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

#[derive(Default, Clone, Debug)]
struct AmpAccumulator {
    cumulative_amp_mean: f64,
    turns_seen: u64,
    stored_count: usize,
    hot_count: usize,
}

type AmpStore = Arc<DashMap<SessionId, AmpAccumulator>>;
type HookAdapterHandle = Arc<Mutex<DaemonAdapter<InMemoryPageTable>>>;

#[derive(Clone)]
struct AppState {
    amp_store: AmpStore,
    hook_adapter: HookAdapterHandle,
    config: Arc<Config>,
}

#[tokio::main]
async fn main() {
    let cli = CliArgs::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let print_only = cli.print_config;
    let config = Config::resolve(cli, &cwd);

    if print_only {
        println!("{}", config.print_table());
        return;
    }

    let bind_addr = format!("127.0.0.1:{}", config.port.value);
    let listener = match TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[proxy] fatal: bind {} failed: {}", bind_addr, e);
            std::process::exit(1);
        }
    };

    println!(
        "ostk-cache {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        bind_addr
    );
    println!("{}", config.banner());
    if let Some(path) = &config.config_path {
        println!("  config: {} (use --print-config for full resolution)", path.display());
    }

    let state = AppState {
        amp_store: Arc::new(DashMap::new()),
        hook_adapter: Arc::new(Mutex::new(DaemonAdapter::new(InMemoryPageTable::new()))),
        config: Arc::new(config),
    };

    let app = Router::new()
        .route("/v1/messages", post(handle_anthropic_message))
        .route("/v1/responses", post(handle_openai_response))
        .route("/hook/event", post(handle_hook_event))
        .fallback(any(|| async { (StatusCode::NOT_FOUND, "Not Found") }))
        .with_state(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!("[proxy] signal received, starting graceful shutdown");
}

#[derive(Debug, Deserialize)]
struct HookEventRequest {
    #[serde(rename = "type")]
    type_: String,
    workspace_id: String,
    session_id: String,
    #[serde(default)]
    payload: Option<serde_json::Value>,
}

/// Map an inbound type string to HookEventKind.
///
/// Accepts both Claude Code's PascalCase event names (`SessionStart`,
/// `UserPromptSubmit`, ...) and snake_case (`session_start`,
/// `user_prompt_submit`, ...) for ergonomics from shell hooks.
fn parse_hook_event_kind(s: &str) -> Option<HookEventKind> {
    if let Some(k) = HookEventKind::parse_str(s) {
        return Some(k);
    }
    match s {
        "session_start" => Some(HookEventKind::SessionStart),
        "user_prompt_submit" => Some(HookEventKind::UserPromptSubmit),
        "pre_tool_use" => Some(HookEventKind::PreToolUse),
        "post_tool_use" => Some(HookEventKind::PostToolUse),
        "stop" => Some(HookEventKind::Stop),
        _ => None,
    }
}

async fn handle_hook_event(
    State(state): State<AppState>,
    Json(req): Json<HookEventRequest>,
) -> Response {
    let kind = match parse_hook_event_kind(&req.type_) {
        Some(k) => k,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": format!("unknown hook event type: {}", req.type_)
                    }
                })),
            )
                .into_response();
        }
    };

    let mut event = HookEvent::new(kind, req.workspace_id, req.session_id);
    if let Some(p) = req.payload {
        event = event.with_payload(p);
    }

    // Dispatch under the adapter mutex. We hold it for the duration of
    // a single hook write; the I/O is small (one append + maybe a
    // manifest snapshot), so contention is bounded.
    {
        let mut adapter = match state.hook_adapter.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        adapter.on_event(event);
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

async fn handle_anthropic_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    body_bytes: axum::body::Bytes,
) -> Result<Response, StatusCode> {
    let amp_store = state.amp_store.clone();
    let config = state.config.clone();
    let passthrough = matches!(config.mode.value, Mode::Passthrough);
    if config.verbose.value {
        println!("--- INTERCEPTED REQUEST ---");
    }
    let req_bytes_in = body_bytes.len() as u64;

    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let session_header = headers
        .get("anthropic-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

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
    let mut firmware_len = 0;
    let mut state_len = 0;
    let mut rebuild_mode_tag: Option<String> = None;
    let mut rebuild_report_capture: Option<ostk_cache::rebuild::RebuildReport> = None;
    // Capture only the fields we need for the per-turn line; the full
    // ostk_files_light::RewriteReport isn't re-exported.
    let mut rewrite_stats_capture: Option<(u32, u64, u64)> = None;
    let mut section_sizes_capture: Option<ostk_cache::rebuild::SectionSizes> = None;
    let mut reduction_report_capture: Option<ostk_cache::rebuild::ReductionReport> = None;
    let turn_started = std::time::Instant::now();
    let mut http_capture = ostk_cache::http_capture::HttpCapture::maybe_start(
        config.capture_http.value,
        &config.capture_http_dir.value,
        &session_id,
        "/v1/messages",
        &headers,
        &body_bytes,
    );

    // Build the rebuild config from the resolved Config; mode is the
    // canonical source for enabled+tag (CLI > env > toml > default).
    let mut rebuild_config = RebuildConfig::from_resolved(&config);

    // Federated mode (mode=rebuild-kernel): fetch a fresh envelope from
    // the kernel daemon over IPC. Any failure falls through to standalone
    // synthesis (the rebuild module extracts an envelope from the request
    // body's prior tool_results).
    if rebuild_config.enabled && rebuild_config.mode_tag == "rebuild_kernel" {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let opts = ostk_cache::kernel_client::ProjectionOpts {
            socket: config.kernel_socket.value.clone(),
            timeout: Some(std::time::Duration::from_millis(config.kernel_timeout_ms.value)),
        };
        if let Some(projection) = ostk_cache::kernel_client::fetch_projection_with(
            &cwd,
            "ostk-cache",
            opts,
        )
        .await
        {
            rebuild_config.live_envelope_override = Some(projection.envelope);
            if config.verbose.value {
                println!(
                    "[proxy] rebuild: fetched live envelope from kernel (wire_version={})",
                    projection.wire_version
                );
            }

            // →1856 P1.C: ALSO call kernel/templates with the request's
            // messages-history text. Templates returned are telemetry-only
            // in this MVP — we log counts but don't yet splice them into
            // the projection synthesis (that's the follow-up). The call
            // proves the verb plumbing end-to-end and gives us live data
            // on cluster sizes to decide where the templates pay off.
            //
            // Graceful fallback: if the haystack daemon predates the verb,
            // it returns a JSON-RPC method-not-found which fetch_templates
            // translates to None — no behavioral change to the proxy.
            let history_lines = extract_history_text_lines(&body_str, 200);
            if !history_lines.is_empty() {
                let templates_opts = ostk_cache::kernel_client::TemplatesOpts {
                    min_cluster_size: Some(2),
                    ..Default::default()
                };
                if let Some(kt) =
                    ostk_cache::kernel_client::fetch_templates(&cwd, history_lines, templates_opts)
                        .await
                {
                    let cluster_sum: usize = kt
                        .templates
                        .iter()
                        .map(|t| t.source_indices.len())
                        .sum();
                    let biggest = kt
                        .templates
                        .iter()
                        .map(|t| t.source_indices.len())
                        .max()
                        .unwrap_or(0);
                    if config.verbose.value {
                        println!(
                            "[proxy] rebuild: kernel/templates returned {} cluster(s) covering {} line(s), biggest={} (wire_version={})",
                            kt.templates.len(),
                            cluster_sum,
                            biggest,
                            kt.wire_version
                        );
                    }
                    // →1856 P1.D splice: render the clusters as a markdown
                    // section the rebuild module drops into the synthetic
                    // projection right after "Recent tool activity". Only
                    // attach when we have clusters with >1 source line —
                    // singletons add noise without compressing anything.
                    if !kt.templates.is_empty() {
                        let section = render_templates_section(&kt.templates);
                        if !section.is_empty() {
                            rebuild_config.templates_summary = Some(section);
                        }
                    }
                }
            }
        } else {
            // Demote to standalone for this request — kernel unavailable.
            rebuild_config.mode_tag = "rebuild_local".to_string();
            if config.verbose.value {
                println!("[proxy] rebuild: kernel projection unavailable, demoted to standalone");
            }
        }
    }

    // Layer 1 cycle digest read: if prior digests exist in the
    // standalone state dir, render the last K into a section the
    // synthesis can drop in as `## Recent assistant turns`. Independent
    // of the rebuild mode and the transcript tail.
    if rebuild_config.enabled {
        let cwd_for_digests =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let recent = ostk_cache::cycle_digest::read_recent(&cwd_for_digests, 5);
        if let Some(section) = ostk_cache::cycle_digest::render_recent_section(&recent) {
            if config.verbose.value {
                println!(
                    "[proxy] rebuild: composed {} recent assistant digests",
                    recent.len()
                );
            }
            rebuild_config.recent_assistant_digests = Some(section);
        }
    }

    // Layer 3 Pattern A: transcript tail. When enabled (`tail.transcript`
    // in config / `OSTK_CACHE_TAIL_TRANSCRIPT=1`), locate the harness's
    // session JSONL log and read the last K events for cross-session/
    // cross-window activity awareness. Composes with both standalone and
    // federated rebuild modes.
    if rebuild_config.enabled {
        let tail_config = ostk_cache::transcript_tail::TailConfig::from_resolved(&config);
        if tail_config.enabled {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            if let Some(session_path) = ostk_cache::transcript_tail::locate_session_file(
                &tail_config.claude_projects_dir,
                &cwd,
            ) {
                let events = ostk_cache::transcript_tail::read_tail_events(
                    &session_path,
                    tail_config.tail_limit,
                );
                if let Some(summary) =
                    ostk_cache::transcript_tail::render_cross_session_summary(&events, None)
                {
                    if config.verbose.value {
                        println!(
                            "[proxy] rebuild: transcript tail composed {} events from {}",
                            events.len(),
                            session_path.display()
                        );
                    }
                    rebuild_config.transcript_tail_summary = Some(summary);
                }
            } else if config.verbose.value {
                eprintln!(
                    "[proxy] rebuild: transcript tail enabled but no session file found in {}",
                    tail_config.claude_projects_dir.display()
                );
            }
        }
    }

    // ----------------------------------------------------------------
    // Rewrite passes (single parse/serialize cycle):
    //
    // 1. **Rebuild** (Layer 1, →1809 plan): when OSTK_CACHE_REBUILD is
    //    set, discard messages[0..last_user_idx] and replace with a
    //    synthetic kernel-projection context message. Preserves the
    //    in-flight chain. Tags AmpRow.mode = "rebuild_local" or
    //    "rebuild_kernel" (Layer 2 reserved).
    //
    // 2. **File-handle rewrite** (→1799): swap inline content for
    //    file_id refs when the FileCache holds a non-stale handle for
    //    the content's SHA-256. Runs after rebuild so it only operates
    //    on the surviving in-flight chain.
    //
    // Pass-through fallback: any failure leaves `body_str` unchanged
    // for downstream code. Breaking the proxy is far worse than missing
    // a rewrite opportunity.
    // ----------------------------------------------------------------
    let rewritten_body_str: std::borrow::Cow<'_, str> =
        match serde_json::from_str::<serde_json::Value>(&body_str) {
            Ok(mut value) => {
                let mut mutated = false;

                // Pass A: strip Claude Code's volatile per-turn
                // `cch=<hex>;` billing token from every string in the
                // request body (→1856 P1.B diagnostic + Issue #40652
                // full fix). The token lives INSIDE cache_control:1h
                // ephemeral on system AND can be embedded in
                // historical tool_result content the CLI rewrites
                // every turn. Recursive walk covers both. Unconditional
                // — applies to every mode. See src/billing_strip.rs
                // for the full narrative.
                let cch_stripped =
                    ostk_cache::billing_strip::strip_volatile_billing_tokens(&mut value);
                if cch_stripped > 0 {
                    if config.verbose.value {
                        println!(
                            "[proxy] cch-strip: removed {} volatile billing token(s) from request body",
                            cch_stripped
                        );
                    }
                    mutated = true;
                }

                // Pass A.5: strip <system-reminder>...</system-reminder>
                // blocks from past user turns. Anthropic/Claude Code
                // inject these out-of-band into user messages; they
                // accumulate in conversation history and bill against
                // input tokens every turn. The current user turn is
                // preserved (a load-bearing reminder may need to be
                // acted on this turn). See src/system_reminder_strip.rs
                // for the full narrative. Unconditional — applies to
                // every mode.
                let sr_stats =
                    ostk_cache::system_reminder_strip::strip_system_reminders_from_past_turns(
                        &mut value,
                    );
                if !sr_stats.is_empty() {
                    if config.verbose.value {
                        println!(
                            "[proxy] sr-strip: removed {} past-turn system-reminder block(s), ~{} bytes (~{} tokens)",
                            sr_stats.blocks_removed,
                            sr_stats.bytes_removed,
                            sr_stats.tokens_estimate()
                        );
                    }
                    mutated = true;
                }

                // Pass 0: kernel orientation in system tier (→1830).
                // Append the firmware-class discipline preamble to
                // req.system with cache_control:1h so it cache-hits on
                // every turn after the first. Idempotent — repeated
                // requests do not double-append.
                if rebuild_config.enabled
                    && ostk_cache::rebuild::append_kernel_orientation_to_system(&mut value)
                {
                    if config.verbose.value {
                        println!(
                            "[proxy] rebuild: appended kernel orientation to system (firmware-class, ttl=1h)"
                        );
                    }
                    mutated = true;
                }

                // Pass 1: rebuild (Layer 1 standalone or Layer 2
                // federated — distinguished by rebuild_config.mode_tag
                // and the optional live_envelope_override populated
                // above when mode == "rebuild_kernel").
                if rebuild_config.enabled {
                    match apply_rebuild(&mut value, &rebuild_config) {
                        RebuildOutcome::Applied(report) => {
                            if config.verbose.value {
                                println!(
                                    "[proxy] rebuild: dropped={} bytes_in={} bytes_out={} envelope={} native={} user_thread={}",
                                    report.turns_dropped,
                                    report.bytes_in,
                                    report.bytes_out,
                                    report.envelope_found,
                                    report.native_tool_calls_summarized,
                                    report.user_messages_summarized,
                                );
                            }
                            rebuild_mode_tag = Some(rebuild_config.mode_tag.clone());
                            rebuild_report_capture = Some(report.clone());
                            mutated = true;
                            // Standalone-mode bookkeeping: append the
                            // activation to ~/.ostk-cache/state/<hash>/
                            // journal.jsonl so the run is auditable.
                            let cwd = std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                            ostk_cache::standalone::log_activation(
                                &cwd,
                                &rebuild_config.mode_tag,
                                &session_id,
                                report.turns_dropped,
                            );
                        }
                        RebuildOutcome::Skipped(reason) => {
                            if config.verbose.value {
                                eprintln!("[proxy] rebuild skipped: {}", reason);
                            }
                            // Even when rebuild skips, claude-code's
                            // history can contain orphaned tool_result
                            // blocks (interrupt artifacts). Strip them
                            // defensively so Anthropic doesn't 400.
                            if let Some(messages) =
                                value.get_mut("messages").and_then(|m| m.as_array_mut())
                            {
                                let orphans =
                                    ostk_cache::rebuild::repair_orphaned_tool_results(messages);
                                if orphans > 0 {
                                    if config.verbose.value {
                                        eprintln!(
                                            "[proxy] rebuild_skip: stripped {} orphaned tool_result blocks",
                                            orphans
                                        );
                                    }
                                    mutated = true;
                                }
                            }
                        }
                        RebuildOutcome::Disabled => {}
                    }
                }

                // Pass 2: file-handle rewrite.
                let rewrite_config = RewriteConfig::from_resolved(&config, session_id.clone());
                match apply_rewrite(&mut value, &rewrite_config) {
                    RewriteOutcome::Applied(report) => {
                        if report.rewrites_applied > 0 {
                            if config.verbose.value {
                                println!(
                                    "[proxy] rewrite: applied={} bytes_in={} bytes_out={} hits={} misses={}",
                                    report.rewrites_applied,
                                    report.bytes_in,
                                    report.bytes_out,
                                    report.hits,
                                    report.misses,
                                );
                            }
                            rewrite_stats_capture = Some((
                                report.rewrites_applied,
                                report.bytes_in,
                                report.bytes_out,
                            ));
                            mutated = true;
                        }
                    }
                    RewriteOutcome::Disabled => {}
                    RewriteOutcome::CacheLoadFailed(err) => {
                        eprintln!(
                            "[proxy] rewrite cache load failed (forwarding original): {}",
                            err
                        );
                    }
                }

                // Pass 3: soft-cap enforcement. Runs only if a positive
                // cap is configured and the post-rewrite serialized body
                // exceeds it. Tiers A→C trim the request; Tier D leaves
                // the body untouched and signals irreducible so we 413
                // below.
                let soft_cap_bytes =
                    config.soft_cap_mb.value.saturating_mul(1024 * 1024);
                let reduction = ostk_cache::rebuild::enforce_soft_cap(&mut value, soft_cap_bytes);
                if reduction.applied_any() {
                    if config.verbose.value {
                        println!(
                            "[proxy] soft-cap: {}→{} tier_a={}({}) tier_b={} tier_c={} irreducible={}",
                            fmt_bytes(reduction.bytes_before),
                            fmt_bytes(reduction.bytes_after),
                            reduction.tier_a_ejected,
                            fmt_bytes(reduction.tier_a_bytes_recovered),
                            reduction.tier_b_pairs_pruned,
                            reduction.tier_c_tools_dropped,
                            reduction.irreducible,
                        );
                    }
                    reduction_report_capture = Some(reduction.clone());
                    if reduction.tier_a_ejected > 0
                        || reduction.tier_b_pairs_pruned > 0
                        || reduction.tier_c_tools_dropped > 0
                    {
                        mutated = true;
                    }
                }
                let must_413 = reduction.irreducible;

                // Capture post-rewrite + post-reduction section sizes
                // for telemetry. synthetic_present iff rebuild::Applied
                // this turn.
                section_sizes_capture = Some(ostk_cache::rebuild::section_sizes(
                    &value,
                    rebuild_report_capture.is_some(),
                ));

                // Diagnostic: per-content-type breakdown of the final
                // wire body (text / tool_use / tool_result / images).
                // SectionSizes tells you WHERE bytes live structurally;
                // this tells you WHAT they are. Verbose-only — pure
                // observability.
                if config.verbose.value {
                    let bd = ostk_cache::body_breakdown::measure_body(&value);
                    println!("[proxy] body: {}", bd.summarize());
                }

                if must_413 {
                    let (dominant_name, dominant_bytes) = section_sizes_capture
                        .map(|s| s.dominant())
                        .unwrap_or(("?", 0));
                    let body = serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": "request_too_large",
                            "message": format!(
                                "ostk-cache: post-reduction size {} exceeds soft cap {}. Largest section: {} ({}). Suggest /compact or trimming MCP servers.",
                                fmt_bytes(reduction.bytes_after),
                                fmt_bytes(soft_cap_bytes),
                                dominant_name,
                                fmt_bytes(dominant_bytes),
                            ),
                            "reduction": {
                                "bytes_before": reduction.bytes_before,
                                "bytes_after": reduction.bytes_after,
                                "tier_a_ejected": reduction.tier_a_ejected,
                                "tier_a_bytes_recovered": reduction.tier_a_bytes_recovered,
                                "tier_b_pairs_pruned": reduction.tier_b_pairs_pruned,
                                "tier_c_tools_dropped": reduction.tier_c_tools_dropped,
                            },
                        },
                    })
                    .to_string();
                    if let Some(capture) = http_capture.take() {
                        capture.finish(
                            StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
                            false,
                            &HeaderMap::new(),
                            body.as_bytes(),
                            turn_started.elapsed(),
                        );
                    }
                    return Ok((StatusCode::PAYLOAD_TOO_LARGE, body).into_response());
                }

                if mutated {
                    match serde_json::to_string(&value) {
                        Ok(s) => std::borrow::Cow::Owned(s),
                        Err(e) => {
                            eprintln!(
                                "[proxy] rewrite reserialize failed (forwarding original): {}",
                                e
                            );
                            std::borrow::Cow::Borrowed(body_str.as_ref())
                        }
                    }
                } else {
                    std::borrow::Cow::Borrowed(body_str.as_ref())
                }
            }
            Err(_) => {
                // Body is not valid JSON; the existing parse-failed path
                // below will surface a 400. Don't pre-empt it here.
                std::borrow::Cow::Borrowed(body_str.as_ref())
            }
        };
    let body_str = rewritten_body_str;

    // When rebuild is **enabled**, treat the request as effective
    // passthrough downstream — regardless of whether rebuild actually
    // applied this turn (it may have skipped: first turn, no real user
    // message, etc.). The legacy mutate path inserts a `5m` HUD block
    // at messages[0], which violates Anthropic's `longer-TTL-first`
    // ordering whenever the in-flight chain already carries a `1h`
    // cache_control (claude-code does this on its user messages).
    // Rebuild mode opts out of the mutate path entirely; if rebuild
    // skipped this turn, the original body forwards unmodified rather
    // than getting a stale-mode HUD bolted on.
    let effective_passthrough = passthrough || rebuild_config.enabled;

    let (payload, parse_failed) = if effective_passthrough {
        // Passthrough mode: forward the original body verbatim. We still
        // perform a cheap JSON validity check so malformed bodies error
        // the same way they do in mutate mode (the caller benefits from
        // the structured 400 response). Accounting (usage parsing,
        // ledger persist, amp_store update) still runs downstream.
        match serde_json::from_str::<serde_json::Value>(&body_str) {
            Ok(_) => (body_str.to_string(), false),
            Err(_) => (body_str.to_string(), true),
        }
    } else {
        match serde_json::from_str::<AnthropicRequest>(&body_str) {
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

            firmware_len = firmware.len();

            req.system = Some(json!([
                {
                    "type": "text",
                    "text": firmware,
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ]));

            if let Some(last_msg) = req.messages.iter_mut().rev().find(|m| m.role == "user") {
                let has_tool_result = last_msg
                    .content
                    .as_array()
                    .map(|arr| {
                        arr.iter().any(|b| {
                            b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        })
                    })
                    .unwrap_or(false);

                if has_tool_result {
                    state_len = serde_json::to_string(&last_msg.content)
                        .unwrap_or_default()
                        .len();
                } else {
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

                    let final_json = json!(new_content_array);
                    state_len = serde_json::to_string(&final_json).unwrap_or_default().len();
                    last_msg.content = final_json;
                }
            }

            match serde_json::to_string(&req) {
                Ok(s) => (s, false),
                Err(_) => (body_str.to_string(), false),
            }
        }
        Err(_) => (body_str.to_string(), true),
        }
    };

    if parse_failed {
        let body = json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": "invalid JSON request body"}
        })
        .to_string();
        if let Some(capture) = http_capture.take() {
            capture.finish(
                StatusCode::BAD_REQUEST.as_u16(),
                false,
                &HeaderMap::new(),
                body.as_bytes(),
                turn_started.elapsed(),
            );
        }
        return Ok((StatusCode::BAD_REQUEST, body).into_response());
    }

    let client = reqwest::Client::new();
    let url = format!("{}/v1/messages", config.upstream.value);
    let mut req_builder = client.post(url);

    for (k, v) in headers.iter() {
        if !is_hop_by_hop_request_header(k) {
            req_builder = req_builder.header(k, v);
        }
    }
    req_builder = req_builder.header("content-type", "application/json");
    req_builder = req_builder.header("accept-encoding", "identity");

    let req_bytes_out = payload.len() as u64;
    if let Some(capture) = http_capture.as_mut() {
        capture.record_outbound(payload.as_bytes());
    }
    let mut response = match req_builder.body(payload).send().await {
        Ok(r) => r,
        Err(e) => {
            let body = json!({
                "type": "error",
                "error": {"type": "upstream_error", "message": format!("{}", e)}
            })
            .to_string();
            if let Some(capture) = http_capture.take() {
                capture.finish(
                    StatusCode::BAD_GATEWAY.as_u16(),
                    false,
                    &HeaderMap::new(),
                    body.as_bytes(),
                    turn_started.elapsed(),
                );
            }
            return Ok((StatusCode::BAD_GATEWAY, body).into_response());
        }
    };

    let status = response.status();
    let mut resp_builder = Response::builder().status(status.as_u16());

    let mut is_sse = false;

    for (k, v) in response.headers().iter() {
        if !should_forward_response_header(k) {
            continue;
        }
        if k.as_str().eq_ignore_ascii_case("content-type")
            && is_sse_content_type(v)
        {
            is_sse = true;
        }
        resp_builder = resp_builder.header(k.as_str(), v.as_bytes());
    }
    let response_headers_capture = response.headers().clone();

    let session_id_clone = session_id.clone();
    // Capture rebuild-enabled state for the stream-side accounting
    // closure. `rebuild_config` itself is not available there (lives
    // in the sync prelude); a single bool is enough to disambiguate
    // the `rebuild_skip` ledger row.
    let rebuild_enabled_capture = rebuild_config.enabled;
    let verbose_capture = config.verbose.value;
    let rebuild_report_for_line = rebuild_report_capture.clone();
    let rewrite_stats_for_line = rewrite_stats_capture;
    let section_sizes_for_line = section_sizes_capture;
    let soft_cap_bytes_for_line = config.soft_cap_mb.value.saturating_mul(1024 * 1024);
    let reduction_for_line = reduction_report_capture.clone();

    let stream = stream! {
        let mut accumulated = Vec::<u8>::new();

        while let Ok(Some(chunk)) = response.chunk().await {
            if chunk.is_empty() {
                continue;
            }
            accumulated.extend_from_slice(&chunk);
            yield Ok::<_, std::io::Error>(chunk);
        }

        let resp_bytes_total = accumulated.len() as u64;
        if let Some(capture) = http_capture.take() {
            capture.finish(
                status.as_u16(),
                is_sse,
                &response_headers_capture,
                &accumulated,
                turn_started.elapsed(),
            );
        }

        let parsed_usage = parse_usage(UsageDialect::Anthropic, is_sse, &accumulated);

        // Layer 1 cycle digest harvest: scan the completed assistant
        // response for a `<turn-digest>{...}</turn-digest>` fence and
        // persist it to the standalone state dir's
        // cycle_digests.jsonl. The next request's projection will
        // include the last K digests as `## Recent assistant turns`.
        // Best-effort: any failure is logged and skipped.
        if rebuild_enabled_capture
            && let Some(mut digest) = ostk_cache::cycle_digest::parse_digest(
                std::str::from_utf8(&accumulated).unwrap_or(""),
            )
        {
            digest.session = session_id_clone.clone();
            let cwd_for_digest =
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            match ostk_cache::cycle_digest::write_digest(&cwd_for_digest, &digest) {
                Ok(()) => {
                    if verbose_capture {
                        println!(
                            "[proxy] rebuild: harvested cycle digest (intent={:?}, outcome={:?}, artifacts={})",
                            digest.intent.as_deref().unwrap_or("?"),
                            digest.outcome.as_deref().unwrap_or("?"),
                            digest.artifacts.len(),
                        );
                    }
                }
                Err(e) => {
                    eprintln!("[proxy] cycle_digest write error: {}", e);
                }
            }
        }

        let mode_str: &str = if let Some(tag) = rebuild_mode_tag.as_deref() {
            tag
        } else if rebuild_enabled_capture {
            "rebuild_skip"
        } else if passthrough {
            "passthrough"
        } else {
            "mutate"
        };

        if let Some(usage) = &parsed_usage {
            let prior_amp_for_write = prior_amp.clone();
            let sizes = SizeMetrics {
                req_bytes_in: Some(req_bytes_in),
                req_bytes_out: Some(req_bytes_out),
                resp_bytes: Some(resp_bytes_total),
            };
            let row = account(AccountInput {
                usage,
                session: session_id_clone.clone(),
                workspace_id: workspace.priority_hash.clone(),
                firmware_bytes: firmware_len,
                state_bytes: state_len,
                hot_count: prior_amp_for_write.hot_count,
                mode: mode_str,
                sizes,
                sections: section_sizes_capture,
                reduction: None,
            });
            if let Err(e) = persist_amp_row(&row) {
                eprintln!("[proxy] persist_amp_row error: {}", e);
            }

            if let Some(reduction) = &reduction_report_capture
                && reduction.applied_any()
            {
                let reduce_row = account(AccountInput {
                    usage: &ProviderUsage {
                        input_tokens: 0,
                        cache_read_tokens: 0,
                        cache_create_tokens: 0,
                    },
                    session: session_id_clone.clone(),
                    workspace_id: workspace.priority_hash.clone(),
                    firmware_bytes: 0,
                    state_bytes: 0,
                    hot_count: 0,
                    mode: "reduce",
                    sizes: SizeMetrics::default(),
                    sections: None,
                    reduction: Some(reduction.clone()),
                });
                if let Err(e) = persist_amp_row(&reduce_row) {
                    eprintln!("[proxy] persist_amp_row (reduce) error: {}", e);
                }
            }

            let mut acc = amp_store.entry(session_id_clone.clone()).or_default();
            let n = acc.turns_seen as f64;
            acc.cumulative_amp_mean = (acc.cumulative_amp_mean * n + row.amp_ratio) / (n + 1.0);
            acc.turns_seen += 1;
            acc.stored_count = acc.turns_seen as usize;
        }

        // Per-turn one-liner: single compact line summarizing the
        // round-trip. Replaces the multi-line spew in default mode;
        // --verbose keeps the per-pass detail above plus this line.
        let elapsed = turn_started.elapsed();
        emit_turn_line(
            &session_id_clone,
            mode_str,
            req_bytes_in,
            req_bytes_out,
            resp_bytes_total,
            parsed_usage.as_ref(),
            rebuild_report_for_line.as_ref(),
            rewrite_stats_for_line,
            section_sizes_for_line,
            soft_cap_bytes_for_line,
            reduction_for_line.as_ref(),
            elapsed,
        );
    };

    let body = Body::from_stream(stream);
    Ok(resp_builder.body(body).unwrap())
}

async fn handle_openai_response(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body_bytes: axum::body::Bytes,
) -> Result<Response, StatusCode> {
    let amp_store = state.amp_store.clone();
    let config = state.config.clone();
    let req_bytes_in = body_bytes.len() as u64;
    let turn_started = std::time::Instant::now();

    let workspace = ostk_cache::Workspace::from_path(
        &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    )
    .unwrap_or_else(|_| ostk_cache::Workspace {
        priority_hash: "unknown".to_string(),
        source: ostk_cache::WorkspaceSource::Cwd,
    });

    let session_id = headers
        .get("openai-session-id")
        .or_else(|| headers.get("x-client-request-id"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let mut h = sha2::Sha256::new();
            h.update(auth.as_bytes());
            format!("{}:{}", workspace.priority_hash, &format!("{:x}", h.finalize())[..12])
        });

    let prior_amp = amp_store
        .get(&session_id)
        .map(|r| r.clone())
        .unwrap_or_default();
    let path = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/v1/responses");
    let mut http_capture = ostk_cache::http_capture::HttpCapture::maybe_start(
        config.capture_http.value,
        &config.capture_http_dir.value,
        &session_id,
        path,
        &headers,
        &body_bytes,
    );

    let body_str = String::from_utf8_lossy(&body_bytes);
    let (payload, parse_failed) = optimize_openai_payload(&body_str, &workspace.priority_hash);
    if parse_failed {
        let body = json!({
            "error": {"type": "invalid_request_error", "message": "invalid JSON request body"}
        })
        .to_string();
        if let Some(capture) = http_capture.take() {
            capture.finish(
                StatusCode::BAD_REQUEST.as_u16(),
                false,
                &HeaderMap::new(),
                body.as_bytes(),
                turn_started.elapsed(),
            );
        }
        return Ok((StatusCode::BAD_REQUEST, body).into_response());
    }

    let client = reqwest::Client::new();
    let url = format!("{}{}", config.upstream.value, path);
    let mut req_builder = client.post(url);
    for (k, v) in headers.iter() {
        if !is_hop_by_hop_request_header(k) {
            req_builder = req_builder.header(k, v);
        }
    }
    req_builder = req_builder.header("content-type", "application/json");
    req_builder = req_builder.header("accept-encoding", "identity");

    let req_bytes_out = payload.len() as u64;
    if let Some(capture) = http_capture.as_mut() {
        capture.record_outbound(payload.as_bytes());
    }
    let mut response = match req_builder.body(payload).send().await {
        Ok(r) => r,
        Err(e) => {
            let body = json!({"error": {"type": "upstream_error", "message": format!("{}", e)}})
                .to_string();
            if let Some(capture) = http_capture.take() {
                capture.finish(
                    StatusCode::BAD_GATEWAY.as_u16(),
                    false,
                    &HeaderMap::new(),
                    body.as_bytes(),
                    turn_started.elapsed(),
                );
            }
            return Ok((StatusCode::BAD_GATEWAY, body).into_response());
        }
    };

    let status = response.status();
    let mut resp_builder = Response::builder().status(status.as_u16());
    let mut is_sse = false;
    for (k, v) in response.headers().iter() {
        if !should_forward_response_header(k) {
            continue;
        }
        if k.as_str().eq_ignore_ascii_case("content-type") && is_sse_content_type(v) {
            is_sse = true;
        }
        resp_builder = resp_builder.header(k.as_str(), v.as_bytes());
    }
    let response_headers_capture = response.headers().clone();
    let session_id_clone = session_id.clone();
    let workspace_id = workspace.priority_hash.clone();

    let stream = stream! {
        let mut accumulated = Vec::<u8>::new();
        while let Ok(Some(chunk)) = response.chunk().await {
            if chunk.is_empty() {
                continue;
            }
            accumulated.extend_from_slice(&chunk);
            yield Ok::<_, std::io::Error>(chunk);
        }

        let resp_bytes_total = accumulated.len() as u64;
        if let Some(capture) = http_capture.take() {
            capture.finish(
                status.as_u16(),
                is_sse,
                &response_headers_capture,
                &accumulated,
                turn_started.elapsed(),
            );
        }

        let parsed_usage = parse_usage(UsageDialect::OpenAi, is_sse, &accumulated);
        if let Some(usage) = &parsed_usage {
            let row = account(AccountInput {
                usage,
                session: session_id_clone.clone(),
                workspace_id: workspace_id.clone(),
                firmware_bytes: 0,
                state_bytes: 0,
                hot_count: prior_amp.hot_count,
                mode: "gpt",
                sizes: SizeMetrics {
                    req_bytes_in: Some(req_bytes_in),
                    req_bytes_out: Some(req_bytes_out),
                    resp_bytes: Some(resp_bytes_total),
                },
                sections: None,
                reduction: None,
            });
            if let Err(e) = persist_amp_row(&row) {
                eprintln!("[proxy] persist_amp_row error: {}", e);
            }

            let mut acc = amp_store.entry(session_id_clone.clone()).or_default();
            let n = acc.turns_seen as f64;
            acc.cumulative_amp_mean = (acc.cumulative_amp_mean * n + row.amp_ratio) / (n + 1.0);
            acc.turns_seen += 1;
            acc.stored_count = acc.turns_seen as usize;
        }

        emit_turn_line(
            &session_id_clone,
            "gpt",
            req_bytes_in,
            req_bytes_out,
            resp_bytes_total,
            parsed_usage.as_ref(),
            None,
            None,
            None,
            0,
            None,
            turn_started.elapsed(),
        );
    };

    Ok(resp_builder.body(Body::from_stream(stream)).unwrap())
}

fn optimize_openai_payload(body: &str, workspace_hash: &str) -> (String, bool) {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (body.to_string(), true);
    };
    let Some(obj) = value.as_object_mut() else {
        return (body.to_string(), true);
    };

    let model = obj.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let is_gpt55 = model == "gpt-5.5" || model.starts_with("gpt-5.5-");
    if is_gpt55 && !obj.contains_key("prompt_cache_retention") {
        obj.insert("prompt_cache_retention".to_string(), json!("24h"));
    }
    if !obj.contains_key("prompt_cache_key") {
        let short_workspace = &workspace_hash[..workspace_hash.len().min(16)];
        obj.insert(
            "prompt_cache_key".to_string(),
            json!(format!("ostk:gpt:{}", short_workspace)),
        );
    }

    match serde_json::to_string(&value) {
        Ok(s) => (s, false),
        Err(_) => (body.to_string(), false),
    }
}

/// Short session prefix (first 6 chars) for the per-turn line. Sessions
/// in the wild are 80+ char `<workspace>:<api-hash>` composites; the
/// full string isn't useful in a tail-able log.
fn short_session(s: &str) -> &str {
    let end = s.char_indices().nth(6).map(|(i, _)| i).unwrap_or(s.len());
    &s[..end]
}

/// →1856 P2: a template "has signal" when it meets all three:
///
///   1. ≥3 *informative* tokens. A token is informative when it
///      isn't `<*>`, is at least two chars long, and contains at
///      least one alphanumeric char. Patterns like `<*> fn <*> {`
///      pass the old single-anchor rule but carry too little of
///      *what* the cluster is to be worth a slot.
///   2. Fraction of `<*>` tokens is < 50%. Templates that wildcard
///      half their tokens or more (e.g. `<*> | gen <*> | <*>`)
///      have lost the data and kept only the scaffolding.
///   3. Does not start with a kernel envelope prefix (`[procs]`,
///      `[loadavg]`, `[meminfo]`, `[ctx]`, `[files]`). The live
///      envelope at the top of the projection re-renders those
///      fresh every cycle, so historical copies clustered in the
///      templates section are pure echo, not signal.
fn template_has_signal(template: &str) -> bool {
    const ENVELOPE_PREFIXES: &[&str] =
        &["[procs]", "[loadavg]", "[meminfo]", "[ctx]", "[files]"];
    let trimmed = template.trim_start();
    if ENVELOPE_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return false;
    }
    let tokens: Vec<&str> = template.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    let placeholders = tokens.iter().filter(|t| **t == "<*>").count();
    if placeholders * 2 >= tokens.len() {
        return false;
    }
    let informative = tokens
        .iter()
        .filter(|t| {
            **t != "<*>"
                && t.len() >= 2
                && t.chars().any(|c| c.is_alphanumeric())
        })
        .count();
    informative >= 3
}

/// →1856 P1.D: render kernel/templates clusters as a markdown section
/// the rebuild module drops into the synthetic projection right after
/// "Recent tool activity". Skips singleton clusters (no compression
/// value), drops low-signal clusters via [`template_has_signal`], and
/// caps the result at 12 entries.
fn render_templates_section(
    templates: &[ostk_cache::kernel_client::KernelTemplate],
) -> String {
    let mut multi: Vec<&ostk_cache::kernel_client::KernelTemplate> = templates
        .iter()
        .filter(|t| t.source_indices.len() >= 2)
        .filter(|t| template_has_signal(&t.template))
        .collect();
    if multi.is_empty() {
        return String::new();
    }
    multi.sort_by(|a, b| b.source_indices.len().cmp(&a.source_indices.len()));
    multi.truncate(12);
    let shown_sum: usize = multi.iter().map(|t| t.source_indices.len()).sum();
    let mut out = String::new();
    out.push_str(&format!(
        "## Inferred templates (paged from prior turns)\n\n_Compressed {} repeated line(s) into {} cluster(s)._\n\n",
        shown_sum,
        multi.len()
    ));
    for t in multi {
        out.push_str(&format!(
            "- ×{} `{}`\n",
            t.source_indices.len(),
            t.template
        ));
    }
    out.push('\n');
    out
}

/// →1856 P1.C: extract recent text content from a request body's
/// `messages` array for feeding to `kernel/templates`.
///
/// Walks the JSON parse of `body_str`, collecting every `text` field
/// found under any nested `content` block in `messages[*]`. Splits
/// each text on newlines and keeps the last `limit` non-empty lines
/// so the template inferrer sees recent activity, not the whole
/// (potentially very large) conversation history. The 200-line
/// default keeps the wire request small and the inferrer fast.
///
/// Failure-tolerant: any parse error returns an empty vec — the
/// templates call then no-ops and the proxy proceeds with its
/// existing rebuild path.
fn extract_history_text_lines(body_str: &str, limit: usize) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(body_str) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut lines: Vec<String> = Vec::new();
    if let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            collect_text_lines_into(msg, &mut lines);
        }
    }
    // Trim to last `limit` non-empty lines.
    lines.retain(|s| !s.trim().is_empty());
    if lines.len() > limit {
        lines.drain(0..lines.len() - limit);
    }
    lines
}

fn collect_text_lines_into(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(obj) => {
            // Block of the shape {"type":"text","text":"..."} —
            // split the text on newlines and append.
            if obj.get("type").and_then(|t| t.as_str()) == Some("text")
                && let Some(text) = obj.get("text").and_then(|t| t.as_str())
            {
                for line in text.lines() {
                    out.push(line.to_string());
                }
            }
            for (_, v) in obj.iter() {
                collect_text_lines_into(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_text_lines_into(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod history_extract_tests {
    use super::*;

    #[test]
    fn extracts_text_lines_from_user_messages() {
        let body = r#"{
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello\nworld"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "hi there"}]}
            ]
        }"#;
        let lines = extract_history_text_lines(body, 100);
        assert_eq!(lines, vec!["hello", "world", "hi there"]);
    }

    #[test]
    fn caps_lines_at_limit() {
        let body = r#"{
            "messages": [{"role": "user", "content": [{"type": "text",
                "text": "a\nb\nc\nd\ne\nf\ng\nh\ni\nj"}]}]
        }"#;
        let lines = extract_history_text_lines(body, 3);
        assert_eq!(lines, vec!["h", "i", "j"]);
    }

    #[test]
    fn empty_for_unparseable_or_no_messages() {
        assert!(extract_history_text_lines("not-json", 100).is_empty());
        assert!(extract_history_text_lines(r#"{"foo":"bar"}"#, 100).is_empty());
    }

    #[test]
    fn walks_into_nested_tool_result_content() {
        let body = r#"{
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "content": [
                        {"type": "text", "text": "+ exit:0\nstdout line one\nstdout line two"}
                    ]
                }]
            }]
        }"#;
        let lines = extract_history_text_lines(body, 100);
        assert_eq!(lines, vec!["+ exit:0", "stdout line one", "stdout line two"]);
    }

    #[test]
    fn drops_blank_lines() {
        let body = r#"{
            "messages": [{"role": "user", "content": [{"type": "text",
                "text": "real\n\n\nalso real"}]}]
        }"#;
        let lines = extract_history_text_lines(body, 100);
        assert_eq!(lines, vec!["real", "also real"]);
    }
}

#[cfg(test)]
mod template_signal_tests {
    use super::*;
    use ostk_cache::kernel_client::KernelTemplate;

    #[test]
    fn drops_pure_wildcard_and_punctuation_only_templates() {
        // None of these say *what* the cluster is — drop them.
        assert!(!template_has_signal("<*>"));
        assert!(!template_has_signal("<*> <*>"));
        assert!(!template_has_signal("<*> <*> <*> <*> <*> <*>"));
        assert!(!template_has_signal("<*> }"));
        assert!(!template_has_signal("<*> <*> <*> {"));
        assert!(!template_has_signal("<*> => <*>"));
    }

    #[test]
    fn keeps_templates_with_three_informative_anchors() {
        // ≥3 informative tokens, <50% wildcards, no envelope prefix.
        assert!(template_has_signal("test result: ok. all passed clean"));
        assert!(template_has_signal("alpha beta gamma <*> delta"));
        assert!(template_has_signal("collect_text_lines_into helper called twice"));
        assert!(template_has_signal("fn render_templates_section() returns String"));
    }

    #[test]
    fn drops_envelope_prefix_templates() {
        // Live envelope re-renders these fresh — keeping templated
        // copies is pure echo, not signal.
        assert!(!template_has_signal(
            "[procs] count:2 active:1 stale:1 dead:0 ctx_p95:0 concern:stale"
        ));
        assert!(!template_has_signal(
            "[loadavg] needles: 0 open (0 P0) | fleet: 0/0 alive | nudges: 0"
        ));
        assert!(!template_has_signal("[meminfo] ctx: 0% 0k/800k Buffers:0k <*>"));
        assert!(!template_has_signal(
            "[ctx] Δ4t:13m | audit:+114 | needles:3 | fleet:2/2 | nudge:0"
        ));
        assert!(!template_has_signal("[files]"));
    }

    #[test]
    fn drops_high_placeholder_ratio_templates() {
        // ≥50% wildcards → the cluster is structure without content.
        assert!(!template_has_signal("<*> | gen <*> | <*>")); // 3/5
        assert!(!template_has_signal("<*> fn <*> {")); // 2/4
        assert!(!template_has_signal("<*> let <*> = <*>")); // 3/5
        assert!(!template_has_signal("<*> use <*>")); // 2/3
        assert!(!template_has_signal("<*> mod <*> {")); // 2/4
    }

    #[test]
    fn drops_too_few_informative_tokens() {
        // 1-2 informative tokens — pattern shape, not enough *what*.
        assert!(!template_has_signal("M src/main.rs")); // `M` len 1
        assert!(!template_has_signal("+ exit:0 <*>")); // only `exit:0`
        assert!(!template_has_signal("<*> #[test]")); // only `#[test]`, ratio also kills
        assert!(!template_has_signal("fn run() {")); // `fn` + `run()` = 2
    }

    fn tpl(s: &str, count: usize) -> KernelTemplate {
        KernelTemplate {
            template: s.to_string(),
            slots: Vec::new(),
            source_indices: (0..count).collect(),
        }
    }

    #[test]
    fn render_drops_singletons_and_noise_keeps_signal() {
        let templates = vec![
            tpl("<*> }", 18),                                  // noise
            tpl("<*> <*> <*> <*> <*> <*>", 6),                 // all wildcard
            tpl("[procs] count:2 active:1 <*>", 6),            // envelope, drop
            tpl("<*> | gen <*> | <*>", 20),                    // 60% wildcards
            tpl("alone with three keywords", 1),               // singleton
            tpl("alpha beta gamma delta", 3),                  // 4 inform → keep
            tpl("test result: ok. all passed clean", 5),       // keep
        ];
        let out = render_templates_section(&templates);
        assert!(out.contains("alpha beta gamma delta"));
        assert!(out.contains("test result: ok. all passed clean"));
        assert!(!out.contains("[procs]"));
        assert!(!out.contains("gen <*>"));
        assert!(!out.contains("alone with three keywords"));
        // shown_sum: 5 + 3 == 8, cluster count == 2
        assert!(out.contains("Compressed 8 repeated line(s) into 2 cluster(s)"));
    }

    #[test]
    fn render_returns_empty_when_everything_is_noise() {
        let templates = vec![
            tpl("<*>", 9),
            tpl("<*> }", 12),
            tpl("<*> <*> <*> {", 8),
        ];
        assert_eq!(render_templates_section(&templates), "");
    }
}

/// Emit the compact per-turn telemetry line.
///
/// Format (one line, no markdown):
///   [turn s=… mode=… req=A→B resp=C tok_in=N tok_out=M cache_r=K% drop=T/X→Y elapsed=Zs]
///
/// Sections present only when relevant: `drop=…` only when rebuild
/// applied this turn; `rewrite=…` only when a file-handle swap fired.
/// Threshold (bytes) at which `emit_turn_line` adds an indented section
/// breakdown: any single section above this triggers the second line.
const SECTION_BREAKDOWN_THRESHOLD_BYTES: u64 = 5 * 1024 * 1024;

#[allow(clippy::too_many_arguments)]
fn emit_turn_line(
    session: &str,
    mode: &str,
    req_in: u64,
    req_out: u64,
    resp: u64,
    usage: Option<&ProviderUsage>,
    rebuild: Option<&ostk_cache::rebuild::RebuildReport>,
    rewrite_stats: Option<(u32, u64, u64)>,
    sections: Option<ostk_cache::rebuild::SectionSizes>,
    soft_cap_bytes: u64,
    reduction: Option<&ostk_cache::rebuild::ReductionReport>,
    elapsed: std::time::Duration,
) {
    let mut out = String::new();
    out.push_str("[turn s=");
    out.push_str(short_session(session));
    out.push_str(" mode=");
    out.push_str(mode);
    out.push_str(" req=");
    out.push_str(&fmt_bytes(req_in));
    out.push('→');
    out.push_str(&fmt_bytes(req_out));
    out.push_str(" resp=");
    out.push_str(&fmt_bytes(resp));

    if let Some(u) = usage {
        let total_in = u.input_tokens + u.cache_read_tokens + u.cache_create_tokens;
        out.push_str(&format!(" tok_in={}", total_in));
        if total_in > 0 {
            let pct = (u.cache_read_tokens as f64 / total_in as f64) * 100.0;
            out.push_str(&format!(" cache_r={:.0}%", pct));
        }
    }

    if let Some(r) = rebuild {
        out.push_str(&format!(
            " drop={}/{}→{}",
            r.turns_dropped,
            fmt_bytes(r.bytes_in as u64),
            fmt_bytes(r.bytes_out as u64),
        ));
    }

    if let Some((n, b_in, b_out)) = rewrite_stats {
        out.push_str(&format!(
            " rewrite={}:{}→{}",
            n,
            fmt_bytes(b_in),
            fmt_bytes(b_out),
        ));
    }

    if let Some(r) = reduction.filter(|r| r.applied_any()) {
        out.push_str(&format!(
            " reduce={}→{} ej={}({}) prune={} tools={}",
            fmt_bytes(r.bytes_before),
            fmt_bytes(r.bytes_after),
            r.tier_a_ejected,
            fmt_bytes(r.tier_a_bytes_recovered),
            r.tier_b_pairs_pruned,
            r.tier_c_tools_dropped,
        ));
        if r.irreducible {
            out.push_str(" [413]");
        }
    }

    out.push_str(&format!(" elapsed={:.2}s]", elapsed.as_secs_f64()));
    println!("{}", out);

    // Indented section breakdown — only when something is bloated.
    // Triggers: any single section > 5MiB OR total > 80% of soft cap.
    if let Some(s) = sections {
        let any_big = s.system >= SECTION_BREAKDOWN_THRESHOLD_BYTES
            || s.tools >= SECTION_BREAKDOWN_THRESHOLD_BYTES
            || s.synthetic >= SECTION_BREAKDOWN_THRESHOLD_BYTES
            || s.in_flight >= SECTION_BREAKDOWN_THRESHOLD_BYTES;
        let near_cap = soft_cap_bytes > 0 && req_out * 5 >= soft_cap_bytes * 4;
        if any_big || near_cap {
            println!(
                "  └─ sys={} tools={} synthetic={} in_flight={} (dominant: {})",
                fmt_bytes(s.system),
                fmt_bytes(s.tools),
                fmt_bytes(s.synthetic),
                fmt_bytes(s.in_flight),
                s.dominant().0,
            );
        }
    }
}

