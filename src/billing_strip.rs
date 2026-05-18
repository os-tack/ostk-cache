//! Strip Claude Code's volatile per-turn `cch=<hex>;` billing token
//! from every string field in the request body so the surrounding
//! `cache_control: 1h ephemeral` markers actually catch prefix-cache
//! hits.
//!
//! # Why this exists
//!
//! Claude Code prefixes every request's system text with an
//! `x-anthropic-billing-header:` line that includes three tokens:
//!
//! ```text
//! cc_version=2.1.143.593;  // stable
//! cc_entrypoint=cli;        // stable
//! cch=<5-hex-chars>;        // CHANGES EVERY TURN
//! ```
//!
//! The `cch=` value (likely a per-call correlator — 5 hex chars =
//! 20 bits of entropy, far too small to be a content hash) drifts
//! turn-to-turn. It lives at byte ~139 of the system block, INSIDE
//! the cache_control region. Anthropic's prompt cache compares
//! prefixes byte-for-byte: that single drifting token defeats the
//! entire ~1MB cached region downstream of it.
//!
//! # The systemic bug (Issue #40652)
//!
//! Worse than just the system block: Claude Code performs a
//! find-and-replace of `cch=<hex>` across **all message content**
//! before each API call. Tool_result content that captured a
//! billing header (e.g., proxy log output, curl trace, anything
//! that incidentally embeds an outgoing request) gets its cch=
//! value rewritten on every subsequent turn. Once a session
//! captures a cch= in any tool result, the prompt cache is
//! permanently broken and never self-heals.
//!
//! Public bug: https://github.com/anthropics/claude-code/issues/40652
//!
//! # The fix
//!
//! Walk the entire request JSON recursively. Every string field is
//! scanned for `cch=<lowercase-hex>;` and ALL occurrences are
//! excised. Applies to:
//!
//!   * system: [{text: ...}]              ← primary cache-bust site
//!   * messages: [{content: [{text:...}]}] ← Issue #40652 propagation
//!   * tool_use_results, nested content blocks, any future shape
//!     that carries Claude Code's billing-header text.
//!
//! # Empirical impact (measured live on this proxy)
//!
//! Stripping just the first system-block occurrence yielded
//! byte-identical first 1MB across three consecutive turns.
//! cache_read_input_tokens went from 0 → 963K per turn.
//! Per-turn cost: ~\$2.50 → ~\$0.29 at Sonnet pricing (~8.5x).
//!
//! Extending the strip recursively defends against Issue #40652's
//! never-self-heals failure mode for any session that captures a
//! cch= value in a tool result.
//!
//! # Safety
//!
//! The token is at most a billing correlator. Stripping costs
//! Anthropic the ability to thread one specific billing row back
//! to one specific Claude Code call; it does not change request
//! semantics, model behavior, or response shape. Anthropic's API
//! accepts the stripped requests without complaint.
//!
//! # Mode coverage
//!
//! Unconditional. Every mode (passthrough / mutate / rebuild /
//! rebuild-kernel) forwards the same Claude Code request body;
//! none of them have a reason to keep the drifting token.

use serde_json::Value;

/// Strip every `cch=<lowercase hex>;` occurrence from `s`.
/// Returns the stripped string (allocating only if at least one
/// match was found) and the number of tokens removed.
fn strip_cch_tokens_in_str(s: &str) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let needle = b"cch=";
    let mut out = String::with_capacity(s.len());
    let mut last_emit = 0usize;
    let mut i = 0usize;
    let mut count = 0usize;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let value_start = i + needle.len();
            let mut j = value_start;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit()
                && !bytes[j].is_ascii_uppercase()
            {
                j += 1;
            }
            if j > value_start && j < bytes.len() && bytes[j] == b';' {
                // Emit everything up to the start of `cch=`, then
                // skip the token (including the `;`).
                out.push_str(&s[last_emit..i]);
                last_emit = j + 1;
                count += 1;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    if count == 0 {
        return None;
    }
    out.push_str(&s[last_emit..]);
    Some((out, count))
}

/// Recursively walk `value`, stripping every `cch=<hex>;` from every
/// JSON string. Returns the total number of tokens stripped across
/// the whole tree.
///
/// This covers Claude Code's full cache-bust footprint per
/// Issue #40652: not only the system block but any tool_result
/// content (and any other string field) that captured a billing
/// header. The walk is generic — schema-free — so future request
/// shapes work without changes here.
pub fn strip_volatile_billing_tokens(value: &mut Value) -> usize {
    match value {
        Value::String(s) => {
            if let Some((stripped, n)) = strip_cch_tokens_in_str(s) {
                *s = stripped;
                n
            } else {
                0
            }
        }
        Value::Array(arr) => arr.iter_mut().map(strip_volatile_billing_tokens).sum(),
        Value::Object(obj) => obj
            .iter_mut()
            .map(|(_, v)| strip_volatile_billing_tokens(v))
            .sum(),
        _ => 0,
    }
}

/// Legacy entry point (system-block-only) preserved so callers can
/// opt into the narrower strip if they have a reason. New code
/// should prefer `strip_volatile_billing_tokens` which covers the
/// full Issue #40652 surface.
pub fn strip_volatile_billing_token(value: &mut Value) -> bool {
    let system = match value.get_mut("system") {
        Some(s) => s,
        None => return false,
    };
    let total = match system {
        Value::Array(_) | Value::Object(_) | Value::String(_) => {
            strip_volatile_billing_tokens(system)
        }
        _ => 0,
    };
    total > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------
    // strip_cch_tokens_in_str — primitive
    // -----------------------------------------------------------------

    #[test]
    fn strips_one_cch_token_with_count() {
        let (out, n) = strip_cch_tokens_in_str("alpha cch=abcde;beta").unwrap();
        assert_eq!(out, "alpha beta");
        assert_eq!(n, 1);
    }

    #[test]
    fn strips_all_occurrences_in_one_string() {
        let (out, n) =
            strip_cch_tokens_in_str("alpha cch=aaaaa;beta cch=bbbbb;gamma cch=ccccc;delta")
                .unwrap();
        assert_eq!(out, "alpha beta gamma delta");
        assert_eq!(n, 3);
    }

    #[test]
    fn returns_none_when_no_match() {
        assert!(strip_cch_tokens_in_str("nothing here").is_none());
        assert!(strip_cch_tokens_in_str("cch=without-semi").is_none());
        assert!(strip_cch_tokens_in_str("cch=ABCDE;uppercase-rejected").is_none());
    }

    // -----------------------------------------------------------------
    // strip_volatile_billing_tokens — recursive entry point
    // -----------------------------------------------------------------

    #[test]
    fn strips_from_modern_array_system() {
        let mut v = json!({
            "system": [{
                "type": "text",
                "cache_control": {"ttl": "1h", "type": "ephemeral"},
                "text": "x-anthropic-billing-header: cc_version=2.1; cc_entrypoint=cli; cch=abcde;You are Claude Code, the cli."
            }]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        assert_eq!(n, 1);
        let after = v["system"][0]["text"].as_str().unwrap();
        assert!(!after.contains("cch="), "cch token should be gone; got: {after}");
        assert!(after.contains("cc_version=2.1"));
        assert!(after.contains("cc_entrypoint=cli"));
        assert!(after.contains("You are Claude Code"));
    }

    #[test]
    fn strips_from_tool_result_content_in_messages() {
        // Issue #40652 scenario: a historical tool_result captured a
        // cch= billing header value (e.g., proxy log output). On
        // subsequent turns Claude Code rewrites the captured cch= to
        // the new request's value, busting the cache.
        let mut v = json!({
            "system": [{"type": "text", "text": "cch=11111;Claude Code system text"}],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_xyz",
                            "content": [
                                {
                                    "type": "text",
                                    "text": "+ exit:0 proxy log: cch=abcde;observed; more output"
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        // 1 in system + 1 in the tool_result text = 2.
        assert_eq!(n, 2);
        let sys = v["system"][0]["text"].as_str().unwrap();
        assert!(!sys.contains("cch="));
        let tr = v["messages"][0]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(!tr.contains("cch="), "tool_result text should be stripped; got: {tr}");
        assert!(tr.contains("+ exit:0 proxy log: observed"));
    }

    #[test]
    fn strips_cch_token_in_tool_result_log_excerpt() {
        // The real-world Issue #40652 example: a tool_result that
        // includes a proxy log line which embedded `cch=<hex>;` from
        // some captured request.
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "content": [
                                {
                                    "type": "text",
                                    "text": "request: cch=14f72;system... response: ok"
                                }
                            ]
                        }
                    ]
                }
            ]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        assert_eq!(n, 1);
        let after = v["messages"][0]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(after, "request: system... response: ok");
    }

    #[test]
    fn strips_multiple_per_string_and_multiple_locations() {
        let mut v = json!({
            "system": [{"text": "first cch=11111;and second cch=22222;tail"}],
            "messages": [
                {"role": "user", "content": [{"text": "third cch=33333;tail"}]},
                {"role": "assistant", "content": [{"text": "fourth cch=44444;tail"}]}
            ]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        // 2 in system + 1 in user + 1 in assistant = 4.
        assert_eq!(n, 4);
        assert_eq!(v["system"][0]["text"], "first and second tail");
        assert_eq!(v["messages"][0]["content"][0]["text"], "third tail");
        assert_eq!(v["messages"][1]["content"][0]["text"], "fourth tail");
    }

    #[test]
    fn no_op_on_request_without_cch() {
        let mut v = json!({
            "system": [{"text": "no billing header at all"}],
            "messages": [{"role": "user", "content": [{"text": "hello"}]}]
        });
        let before = v.clone();
        let n = strip_volatile_billing_tokens(&mut v);
        assert_eq!(n, 0);
        assert_eq!(v, before);
    }

    #[test]
    fn legacy_system_only_function_still_works() {
        let mut v = json!({
            "system": [{"text": "cch=11111;system content"}],
            "messages": [{"role": "user", "content": [{"text": "cch=22222;user content"}]}]
        });
        // The legacy function reports a strip happened (true) but
        // leaves messages untouched — narrow-by-design.
        assert!(strip_volatile_billing_token(&mut v));
        assert_eq!(v["system"][0]["text"], "system content");
        assert_eq!(
            v["messages"][0]["content"][0]["text"], "cch=22222;user content",
            "legacy strip leaves messages alone"
        );
    }

    #[test]
    fn preserves_cache_control_metadata_through_walk() {
        let mut v = json!({
            "system": [{
                "type": "text",
                "cache_control": {"ttl": "1h", "type": "ephemeral"},
                "text": "cch=cafe1;You are Claude Code..."
            }],
            "tools": [{
                "name": "Read",
                "cache_control": {"ttl": "5m", "type": "ephemeral"}
            }]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        assert_eq!(n, 1);
        assert_eq!(
            v["system"][0]["cache_control"],
            json!({"ttl": "1h", "type": "ephemeral"}),
            "system cache_control survives"
        );
        assert_eq!(
            v["tools"][0]["cache_control"],
            json!({"ttl": "5m", "type": "ephemeral"}),
            "tool cache_control survives"
        );
    }

    #[test]
    fn ignores_uppercase_and_missing_semicolon_globally() {
        let mut v = json!({
            "system": [{"text": "cch=ABCDE;uppercase rejected"}],
            "messages": [{"role": "user", "content": [{"text": "cch=deadbeef no semi"}]}]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        assert_eq!(n, 0, "neither uppercase nor missing-semi should be stripped");
    }

    #[test]
    fn handles_deeply_nested_content_blocks() {
        // Future-proof: even if Anthropic adds new schema shapes,
        // the recursive walk keeps working.
        let mut v = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "content": [{
                        "type": "text",
                        "text": "cch=cafe1;nested deep"
                    }, {
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "irrelevant"
                        }
                    }]
                }]
            }]
        });
        let n = strip_volatile_billing_tokens(&mut v);
        assert_eq!(n, 1);
        assert_eq!(
            v["messages"][0]["content"][0]["content"][0]["text"],
            "nested deep"
        );
    }

    #[test]
    fn legacy_handles_string_system_shape() {
        let mut v = json!({"system": "preamble cch=12345;rest"});
        assert!(strip_volatile_billing_token(&mut v));
        assert_eq!(v["system"], "preamble rest");
    }
}
