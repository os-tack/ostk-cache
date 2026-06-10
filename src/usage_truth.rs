//! →1985 K6-P0: usage-truth passthrough + overflow translation.
//!
//! The projection gives cache-proxied seats a small upstream context, so
//! the usage numbers Claude Code sees never grow and its auto-compact
//! machinery never fires — the transcript marches into a transport wall
//! (empirically: axum's stock 2MiB body limit first, the 32MB
//! client/upstream cap behind it). Three remedies live here:
//!
//! - **Overflow translation (X2)**: when the inbound body crosses a soft
//!   threshold, answer with the Anthropic `invalid_request_error`
//!   "prompt is too long: N tokens > M maximum" shape instead of letting
//!   the request march toward a terminal transport reject. Claude Code
//!   2.1.168 carries the literal regex
//!   `prompt is too long[^0-9]*(\d+)\s*tokens?\s*>\s*(\d+)` and routes
//!   the match through reactive auto-compaction (binary-verified).
//! - **Usage-truth rewrite (X1)**: rewrite `usage.input_tokens` in the
//!   streamed response so the reported input side reflects the TRUE
//!   inbound body size (scaled), letting CC's own proactive auto-compact
//!   fire well before any wall. Config-gated: CC's tolerance of
//!   usage > model window is unverified, so this ships default-off.
//! - **Meminfo gauge (X3)**: append `body:<N>MB/32MB` to the projected
//!   `[meminfo]` line so seats and the lead can watch the march.

use serde_json::Value;

/// The hard transport wall the gauge is denominated in (MiB). Matches
/// `config::ANTHROPIC_HARD_LIMIT_MB`; duplicated as a plain const to keep
/// this module dependency-free for unit testing.
const HARD_WALL_MB: f64 = 32.0;

/// Estimate a token count from a serialized body size. `bytes_per_token`
/// is a calibration knob, not a tokenizer: 4.0 approximates honest
/// English/code JSON; larger values delay the compact point. Default
/// lives in config (`truth_bytes_per_token`).
pub fn estimate_tokens(body_bytes: u64, bytes_per_token: f64) -> u64 {
    if bytes_per_token <= 0.0 {
        return 0;
    }
    (body_bytes as f64 / bytes_per_token).round() as u64
}

/// X1(B): session-scoped enablement. Usage-truth rewriting fires when
/// the global flag is on, OR the request's wire-header session id is on
/// the allowlist. Keyed on the wire header ONLY — never the workspace
/// fallback id (→1973/K2: wire header is keying ground truth) — so a
/// request with no session header can never allowlist-hit. Empty
/// allowlist with the global flag off is byte-identical to pre-allowlist
/// behavior: non-listed sessions see zero diff.
pub fn enabled_for_session(global: bool, allowlist: &[String], wire_session: Option<&str>) -> bool {
    if global {
        return true;
    }
    match wire_session {
        Some(sid) => allowlist.iter().any(|s| s == sid),
        None => false,
    }
}

/// X2: the Anthropic error body Claude Code treats as a reactive
/// compaction trigger. Both numbers are derived from wire bytes at the
/// honest 4.0 scale so the harness's parsed `(actual, max)` pair is
/// plausible and `actual > max` always holds.
pub fn overflow_error_body(req_bytes: u64, threshold_bytes: u64) -> Value {
    let actual_tokens = estimate_tokens(req_bytes, 4.0).max(1);
    // Ensure strict inequality even at the boundary.
    let max_tokens = estimate_tokens(threshold_bytes, 4.0)
        .min(actual_tokens.saturating_sub(1))
        .max(1);
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!(
                "prompt is too long: {} tokens > {} maximum",
                actual_tokens, max_tokens
            )
        }
    })
}

/// X3: append ` body:<N.N>MB/32MB` to the `[meminfo]` line of an
/// envelope quadruplet. Lines other than `[meminfo]` pass through
/// untouched; an envelope without a `[meminfo]` line is returned as-is.
pub fn augment_meminfo_body(envelope: &str, body_bytes: u64) -> String {
    let gauge = format!(
        " body:{:.1}MB/{:.0}MB",
        body_bytes as f64 / (1024.0 * 1024.0),
        HARD_WALL_MB
    );
    envelope
        .lines()
        .map(|line| {
            if line.starts_with("[meminfo]") {
                format!("{}{}", line, gauge)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// X1: streaming SSE usage rewriter.
///
/// Feeds on raw response chunks, reassembles complete SSE events
/// (`\n\n`-delimited), and rewrites `usage.input_tokens` inside any
/// event whose data payload carries a usage object with an input side
/// (`message_start`, and `message_delta` on API versions that echo
/// input fields there). Events without `"usage"` pass through verbatim
/// via a byte-scan fast path — no JSON parse, no reserialization.
///
/// Rewrite rule: with `total_in = input + cache_read + cache_creation`,
/// when `est_tokens > total_in` the difference is added to
/// `input_tokens` (cache fields are left alone). Reported usage is
/// therefore monotonic-max: small bodies keep upstream truth.
///
/// Fail-open: any parse anomaly emits the original bytes unchanged.
pub struct SseUsageRewriter {
    carry: Vec<u8>,
    est_tokens: u64,
}

impl SseUsageRewriter {
    pub fn new(est_tokens: u64) -> Self {
        Self {
            carry: Vec::new(),
            est_tokens,
        }
    }

    /// Transform one upstream chunk into the bytes to forward.
    pub fn transform(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.carry.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(chunk.len());
        // Emit every complete event in the carry buffer.
        while let Some(pos) = find_double_newline(&self.carry) {
            let event: Vec<u8> = self.carry.drain(..pos).collect();
            out.extend_from_slice(&self.rewrite_event(&event));
        }
        out
    }

    /// Stream end: flush whatever remains (incomplete trailing event)
    /// verbatim.
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.carry)
    }

    /// Rewrite a single complete SSE event (delimiter included). Returns
    /// original bytes when no usage rewrite applies.
    fn rewrite_event(&self, event: &[u8]) -> Vec<u8> {
        // Fast path: no usage object anywhere in the event.
        if !contains_subslice(event, b"\"usage\"") {
            return event.to_vec();
        }
        let Ok(text) = std::str::from_utf8(event) else {
            return event.to_vec();
        };
        let mut rewritten = String::with_capacity(text.len() + 16);
        let mut changed = false;
        for line in text.split_inclusive('\n') {
            let stripped = line
                .strip_prefix("data: ")
                .or_else(|| line.strip_prefix("data:"));
            if let Some(payload) = stripped
                && let Ok(mut json) = serde_json::from_str::<Value>(payload.trim_end())
                && self.bump_usage(&mut json)
            {
                let prefix_len = line.len() - payload.len();
                rewritten.push_str(&line[..prefix_len]);
                rewritten.push_str(&json.to_string());
                if payload.ends_with('\n') {
                    rewritten.push('\n');
                }
                changed = true;
            } else {
                rewritten.push_str(line);
            }
        }
        if changed {
            rewritten.into_bytes()
        } else {
            event.to_vec()
        }
    }

    /// Apply the monotonic-max bump to a parsed event JSON. Returns true
    /// if the value was modified.
    fn bump_usage(&self, json: &mut Value) -> bool {
        // usage lives at .message.usage (message_start) or .usage
        // (message_delta).
        let usage = if let Some(u) = json.pointer_mut("/message/usage") {
            u
        } else if let Some(u) = json.pointer_mut("/usage") {
            u
        } else {
            return false;
        };
        let Some(obj) = usage.as_object_mut() else {
            return false;
        };
        // Only events that carry an input side are rewritten; pure
        // output_tokens deltas pass through.
        let Some(input) = obj.get("input_tokens").and_then(|v| v.as_u64()) else {
            return false;
        };
        let cache_read = obj
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_create = obj
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let total_in = input + cache_read + cache_create;
        if self.est_tokens <= total_in {
            return false;
        }
        obj.insert(
            "input_tokens".to_string(),
            Value::from(input + (self.est_tokens - total_in)),
        );
        true
    }
}

/// Position just past the first `\n\n` (or `\r\n\r\n`) in `buf`, i.e. the
/// length of the first complete SSE event including its delimiter.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- X1(B) session-scoped enablement ------------------------------

    fn allow(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn session_gate_global_on_wins_regardless() {
        assert!(enabled_for_session(true, &[], None));
        assert!(enabled_for_session(true, &allow(&["x"]), Some("y")));
    }

    #[test]
    fn session_gate_empty_allowlist_is_passthrough() {
        // Condition (1): empty allowlist + global off = current behavior.
        assert!(!enabled_for_session(false, &[], Some("any-session")));
        assert!(!enabled_for_session(false, &[], None));
    }

    #[test]
    fn session_gate_hit_enables() {
        let list = allow(&["d08903c2-aaaa-bbbb-cccc-000000000000"]);
        assert!(enabled_for_session(
            false,
            &list,
            Some("d08903c2-aaaa-bbbb-cccc-000000000000")
        ));
    }

    #[test]
    fn session_gate_miss_is_passthrough() {
        // Condition (2): allowlist-miss must behave exactly like off.
        let list = allow(&["d08903c2-aaaa-bbbb-cccc-000000000000"]);
        assert!(!enabled_for_session(false, &list, Some("other-session")));
    }

    #[test]
    fn session_gate_fallback_id_never_hits() {
        // No wire header → workspace-fallback keying → never allowlisted,
        // even if someone lists the fallback-shaped id itself.
        let list = allow(&["wkhash:deadbeef0123"]);
        assert!(!enabled_for_session(false, &list, None));
    }

    // The literal regex shipped in Claude Code 2.1.168 (binary-verified):
    // `prompt is too long[^0-9]*(\d+)\s*tokens?\s*>\s*(\d+)`. Replicated
    // as a hand check so the contract is pinned by test, not by hope.
    fn cc_regex_matches(msg: &str) -> Option<(u64, u64)> {
        let idx = msg.find("prompt is too long")?;
        let rest = &msg[idx + "prompt is too long".len()..];
        let first_digit = rest.find(|c: char| c.is_ascii_digit())?;
        let rest = &rest[first_digit..];
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let actual: u64 = rest[..end].parse().ok()?;
        let rest = rest[end..].trim_start();
        let rest = rest
            .strip_prefix("tokens")
            .or_else(|| rest.strip_prefix("token"))?;
        let rest = rest.trim_start().strip_prefix('>')?.trim_start();
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let max: u64 = rest[..end].parse().ok()?;
        Some((actual, max))
    }

    #[test]
    fn overflow_body_matches_cc_trigger_regex() {
        let body = overflow_error_body(25 * 1024 * 1024, 24 * 1024 * 1024);
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        let msg = body["error"]["message"].as_str().unwrap();
        let (actual, max) = cc_regex_matches(msg).expect("CC trigger regex must match");
        assert!(actual > max, "actual {} must exceed max {}", actual, max);
    }

    #[test]
    fn overflow_body_boundary_still_strict() {
        // req == threshold: numbers must still satisfy actual > max.
        let body = overflow_error_body(1000, 1000);
        let msg = body["error"]["message"].as_str().unwrap();
        let (actual, max) = cc_regex_matches(msg).unwrap();
        assert!(actual > max);
    }

    #[test]
    fn meminfo_gauge_appends_to_meminfo_line_only() {
        let env = "[procs] count:1\n[loadavg] needles: 0\n[meminfo] ctx: 0% 0k/800k Buffers:0k tok_calls:7\n[ctx] Δ1t:5s";
        let out = augment_meminfo_body(env, 25 * 1024 * 1024 + 512 * 1024);
        assert!(out.contains("[meminfo] ctx: 0% 0k/800k Buffers:0k tok_calls:7 body:25.5MB/32MB"));
        assert!(out.starts_with("[procs] count:1\n"));
        assert!(out.ends_with("[ctx] Δ1t:5s"));
    }

    #[test]
    fn meminfo_gauge_no_meminfo_line_passthrough() {
        let env = "just some text\nno envelope here";
        assert_eq!(augment_meminfo_body(env, 1024), env);
    }

    fn msg_start_event(input: u64, cache_read: u64) -> String {
        format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\",\"usage\":{{\"input_tokens\":{},\"cache_read_input_tokens\":{},\"cache_creation_input_tokens\":0,\"output_tokens\":1}}}}}}\n\n",
            input, cache_read
        )
    }

    #[test]
    fn sse_rewrites_message_start_monotonic_max() {
        let mut rw = SseUsageRewriter::new(500_000);
        let out = rw.transform(msg_start_event(100, 900).as_bytes());
        let text = String::from_utf8(out).unwrap();
        // total_in was 1000; est 500k → input becomes 100 + 499000.
        assert!(text.contains("\"input_tokens\":499100"), "got: {}", text);
        assert!(text.contains("\"cache_read_input_tokens\":900"));
        assert!(rw.flush().is_empty());
    }

    #[test]
    fn sse_small_body_keeps_upstream_truth() {
        let mut rw = SseUsageRewriter::new(50);
        let event = msg_start_event(100, 900);
        let out = rw.transform(event.as_bytes());
        assert_eq!(out, event.as_bytes(), "est below upstream → verbatim");
    }

    #[test]
    fn sse_event_split_across_chunks() {
        let mut rw = SseUsageRewriter::new(500_000);
        let event = msg_start_event(100, 0);
        let (a, b) = event.as_bytes().split_at(40);
        let mut out = rw.transform(a);
        assert!(out.is_empty(), "incomplete event must be held back");
        out.extend(rw.transform(b));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"input_tokens\":500000"), "got: {}", text);
    }

    #[test]
    fn sse_non_usage_events_pass_verbatim() {
        let mut rw = SseUsageRewriter::new(500_000);
        let delta = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        assert_eq!(rw.transform(delta.as_bytes()), delta.as_bytes());
    }

    #[test]
    fn sse_output_only_delta_passes_verbatim() {
        let mut rw = SseUsageRewriter::new(500_000);
        let delta = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n";
        assert_eq!(rw.transform(delta.as_bytes()), delta.as_bytes());
    }

    #[test]
    fn sse_flush_returns_incomplete_tail() {
        let mut rw = SseUsageRewriter::new(10);
        let partial = b"event: message_stop\ndata: {\"type\":\"message_st";
        assert!(rw.transform(partial).is_empty());
        assert_eq!(rw.flush(), partial.to_vec());
    }

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens(4096, 4.0), 1024);
        assert_eq!(estimate_tokens(4096, 16.0), 256);
        assert_eq!(estimate_tokens(100, 0.0), 0);
    }
}
