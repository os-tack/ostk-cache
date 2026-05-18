//! Strip Claude Code's volatile per-turn `cch=<hex>;` billing token
//! from the system block so the surrounding `cache_control: 1h
//! ephemeral` marker actually catches prefix-cache hits.
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
//! turn-to-turn and lives at byte ~139 of the system block, INSIDE
//! the cache_control region. Anthropic's prompt cache compares the
//! prefix byte-for-byte: that single drifting token defeats the
//! entire ~1MB cached region downstream of it.
//!
//! Empirical measurement (this commit's diagnostic): a Claude Code
//! session through this proxy showed `cache_r=0%` on consecutive
//! turns of a 1.36MB request body. Stripping just the first
//! `cch=<hex>;` token rendered the first 1MB of three consecutive
//! requests byte-identical (SHA-256 of first 1MB matched).
//! Expected impact post-strip: ~75% prefix-cache hit rate on the
//! same workload.
//!
//! # Safety
//!
//! The token is at most a billing correlator. Stripping costs
//! Anthropic the ability to thread one specific billing row back to
//! one specific Claude Code call; it does not change request
//! semantics, model behavior, or response shape.
//!
//! # Mode coverage
//!
//! Unconditional. Every mode (passthrough / mutate / rebuild /
//! rebuild-kernel) forwards the same Claude Code system block;
//! none of them have a reason to keep the drifting token.

use serde_json::Value;

/// Find and excise `cch=<lowercase hex>;` from `s`. Returns the
/// stripped string (allocating only if a match was found) and
/// whether a strip happened.
fn strip_cch_token(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let needle = b"cch=";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            // Found "cch="; advance past it and consume the hex run.
            let value_start = i + needle.len();
            let mut j = value_start;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit()
                && !bytes[j].is_ascii_uppercase()
            {
                j += 1;
            }
            // Require at least one hex char and a trailing `;`.
            if j > value_start && j < bytes.len() && bytes[j] == b';' {
                let mut out = String::with_capacity(s.len() - (j + 1 - i));
                out.push_str(&s[..i]);
                out.push_str(&s[j + 1..]);
                return Some(out);
            }
        }
        i += 1;
    }
    None
}

/// →1856 P1.B-diagnostic: strip the volatile `cch=<hex>;` token
/// from every system text block. Returns true iff anything changed.
///
/// Supports both the array shape (`system: [{type:"text", text:...}]`,
/// modern Claude Code) and the legacy flat-string shape
/// (`system: "..."`, older API consumers).
pub fn strip_volatile_billing_token(value: &mut Value) -> bool {
    let system = match value.get_mut("system") {
        Some(s) => s,
        None => return false,
    };

    if let Some(arr) = system.as_array_mut() {
        let mut any = false;
        for block in arr.iter_mut() {
            let is_text =
                block.get("type").and_then(|t| t.as_str()) == Some("text");
            if !is_text {
                continue;
            }
            let new_text = match block.get("text").and_then(|t| t.as_str()) {
                Some(text) => strip_cch_token(text),
                None => None,
            };
            if let Some(stripped) = new_text {
                block["text"] = Value::String(stripped);
                any = true;
            }
        }
        return any;
    }

    if let Some(s) = system.as_str() {
        if let Some(stripped) = strip_cch_token(s) {
            *system = Value::String(stripped);
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_cch_from_modern_array_system() {
        let mut v = json!({
            "system": [{
                "type": "text",
                "cache_control": {"ttl": "1h", "type": "ephemeral"},
                "text": "x-anthropic-billing-header: cc_version=2.1; cc_entrypoint=cli; cch=abcde;You are Claude Code, the cli."
            }]
        });
        assert!(strip_volatile_billing_token(&mut v));
        let after = v["system"][0]["text"].as_str().unwrap();
        assert!(
            !after.contains("cch="),
            "cch token should be gone; got: {after}"
        );
        assert!(
            after.contains("cc_version=2.1"),
            "stable cc_version token preserved"
        );
        assert!(
            after.contains("cc_entrypoint=cli"),
            "stable cc_entrypoint token preserved"
        );
        assert!(
            after.contains("You are Claude Code"),
            "actual system content preserved verbatim"
        );
    }

    #[test]
    fn idempotent_when_no_cch_token() {
        let mut v = json!({
            "system": [{
                "type": "text",
                "text": "x-anthropic-billing-header: cc_version=2.1;You are Claude Code..."
            }]
        });
        let before = v.clone();
        assert!(!strip_volatile_billing_token(&mut v));
        assert_eq!(v, before, "no-op should leave value byte-identical");
    }

    #[test]
    fn no_op_when_system_field_missing() {
        let mut v = json!({"messages": []});
        let before = v.clone();
        assert!(!strip_volatile_billing_token(&mut v));
        assert_eq!(v, before);
    }

    #[test]
    fn handles_legacy_string_system_shape() {
        let mut v = json!({"system": "preamble cch=12345;rest"});
        assert!(strip_volatile_billing_token(&mut v));
        assert_eq!(v["system"], "preamble rest");
    }

    #[test]
    fn only_strips_first_occurrence_in_a_block() {
        // The system block has cch only once; historical cch values
        // in the messages history are intentionally preserved.
        let mut v = json!({
            "system": [{"type": "text", "text": "alpha cch=aaaaa;beta cch=bbbbb;gamma"}]
        });
        assert!(strip_volatile_billing_token(&mut v));
        let after = v["system"][0]["text"].as_str().unwrap();
        // After stripping the first one: "alpha beta cch=bbbbb;gamma"
        // Second occurrence is left alone — keeps behavior conservative
        // and avoids touching history.
        assert!(
            after.contains("cch=bbbbb;"),
            "second occurrence preserved; got: {after}"
        );
        assert!(
            !after.contains("cch=aaaaa;"),
            "first occurrence stripped; got: {after}"
        );
    }

    #[test]
    fn rejects_malformed_cch_without_semicolon() {
        // Defensive: only strip when the trailing semicolon is
        // present. A bare "cch=deadbeef" without `;` could be part
        // of a different token (e.g. in user content); leave it alone.
        let mut v = json!({
            "system": [{"type": "text", "text": "cch=deadbeef rest"}]
        });
        assert!(!strip_volatile_billing_token(&mut v));
    }

    #[test]
    fn rejects_cch_with_uppercase_hex() {
        // The Claude Code token is lowercase hex. Anything else is
        // not the volatile billing token — leave it alone.
        let mut v = json!({
            "system": [{"type": "text", "text": "cch=ABCDE;rest"}]
        });
        assert!(!strip_volatile_billing_token(&mut v));
    }

    #[test]
    fn strips_independently_across_multiple_system_blocks() {
        let mut v = json!({
            "system": [
                {"type": "text", "text": "first cch=11111;tail"},
                {"type": "text", "text": "second cch=22222;tail"},
            ]
        });
        assert!(strip_volatile_billing_token(&mut v));
        assert_eq!(v["system"][0]["text"], "first tail");
        assert_eq!(v["system"][1]["text"], "second tail");
    }

    #[test]
    fn preserves_cache_control_metadata() {
        let mut v = json!({
            "system": [{
                "type": "text",
                "cache_control": {"ttl": "1h", "type": "ephemeral"},
                "text": "cch=cafe1;You are Claude Code..."
            }]
        });
        strip_volatile_billing_token(&mut v);
        assert_eq!(
            v["system"][0]["cache_control"],
            json!({"ttl": "1h", "type": "ephemeral"}),
            "cache_control block must survive the strip"
        );
    }
}
