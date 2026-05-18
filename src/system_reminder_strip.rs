//! Strip `<system-reminder>...</system-reminder>` blocks from past
//! user turns in the request body.
//!
//! # Why this exists
//!
//! Claude Code (and Anthropic's server) inject out-of-band guidance
//! into user messages, e.g.:
//!
//! ```text
//! <system-reminder>
//! The task tools haven't been used recently. ...
//! </system-reminder>
//! ```
//!
//! These reminders bill against the user's input-token budget on
//! every subsequent turn — once injected, they become part of the
//! conversation history that the API re-sends on every request.
//! They are noise the user never typed, never asked for, and cannot
//! see in their UI.
//!
//! # Strategy
//!
//! Walk every message EXCEPT the most-recent user turn. For each
//! string field encountered, strip every `<system-reminder>...
//! </system-reminder>` block (including a single trailing newline
//! so we don't leave a blank line behind). Track block count and
//! byte count for telemetry.
//!
//! # Why not the current turn
//!
//! The most-recent user message can contain a load-bearing reminder
//! the assistant needs to react to this turn (a hook output, an IDE
//! selection notice). Stripping it could change current-turn
//! behavior. Past-turn reminders are already-acted-upon history;
//! removing them is loss-free.
//!
//! # Mode coverage
//!
//! Unconditional. Every mode (passthrough / mutate / rebuild /
//! rebuild-kernel) carries the same accumulated message history;
//! none of them benefit from re-billing yesterday's nudges.

use serde_json::Value;

const OPEN_TAG: &str = "<system-reminder>";
const CLOSE_TAG: &str = "</system-reminder>";

/// Aggregate statistics from one strip pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemReminderStats {
    /// Number of `<system-reminder>` blocks removed across all
    /// stripped messages.
    pub blocks_removed: usize,
    /// Total bytes removed (length of the stripped substrings,
    /// tags included).
    pub bytes_removed: usize,
}

impl SystemReminderStats {
    pub fn is_empty(self) -> bool {
        self.blocks_removed == 0
    }

    pub fn add(&mut self, other: Self) {
        self.blocks_removed += other.blocks_removed;
        self.bytes_removed += other.bytes_removed;
    }

    /// Rough token estimate. English text averages ~4 bytes/token;
    /// this number is used for telemetry only.
    pub fn tokens_estimate(self) -> usize {
        self.bytes_removed / 4
    }
}

/// Strip every `<system-reminder>...</system-reminder>` block from
/// `s`. Returns `None` if no open tag is present (no allocation).
/// Otherwise returns the stripped string and the stats.
///
/// Unclosed open tags are left intact — we never throw away content
/// past an unmatched `<system-reminder>`.
fn strip_reminders_in_str(s: &str) -> Option<(String, SystemReminderStats)> {
    if !s.contains(OPEN_TAG) {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut stats = SystemReminderStats::default();
    let mut cursor = 0usize;
    while let Some(open_rel) = s[cursor..].find(OPEN_TAG) {
        let abs_open = cursor + open_rel;
        let body_start = abs_open + OPEN_TAG.len();
        let close_rel = match s[body_start..].find(CLOSE_TAG) {
            Some(idx) => idx,
            None => break,
        };
        let abs_close_end = body_start + close_rel + CLOSE_TAG.len();
        let drop_end = if s.as_bytes().get(abs_close_end) == Some(&b'\n') {
            abs_close_end + 1
        } else {
            abs_close_end
        };
        out.push_str(&s[cursor..abs_open]);
        stats.blocks_removed += 1;
        stats.bytes_removed += drop_end - abs_open;
        cursor = drop_end;
    }
    if stats.blocks_removed == 0 {
        return None;
    }
    out.push_str(&s[cursor..]);
    Some((out, stats))
}

/// Recursively walk `value`, stripping every reminder block from
/// every string field. Used internally on a single message's
/// `content`. Schema-free: works for `[{type:"text", text:...}]`,
/// `[{type:"tool_result", content:[{type:"text", text:...}]}]`, or
/// future shapes.
fn strip_reminders_recursive(value: &mut Value) -> SystemReminderStats {
    let mut acc = SystemReminderStats::default();
    match value {
        Value::String(s) => {
            if let Some((stripped, stats)) = strip_reminders_in_str(s) {
                *s = stripped;
                acc.add(stats);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                acc.add(strip_reminders_recursive(item));
            }
        }
        Value::Object(obj) => {
            for (_, v) in obj.iter_mut() {
                acc.add(strip_reminders_recursive(v));
            }
        }
        _ => {}
    }
    acc
}

/// Strip `<system-reminder>` blocks from every message in
/// `value["messages"]` EXCEPT the most-recent user message (the
/// "current turn").
///
/// Returns aggregate stats; `SystemReminderStats::default()` if
/// `messages` is missing, not an array, or contains no reminders.
pub fn strip_system_reminders_from_past_turns(value: &mut Value) -> SystemReminderStats {
    let messages = match value.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return SystemReminderStats::default(),
    };
    let last_user_idx = messages
        .iter()
        .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"));
    let mut acc = SystemReminderStats::default();
    for (i, msg) in messages.iter_mut().enumerate() {
        if Some(i) == last_user_idx {
            continue;
        }
        if let Some(content) = msg.get_mut("content") {
            acc.add(strip_reminders_recursive(content));
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_one_block_with_count_and_bytes() {
        let s = "before <system-reminder>nudge</system-reminder>\nafter";
        let (out, stats) = strip_reminders_in_str(s).unwrap();
        assert_eq!(out, "before after");
        assert_eq!(stats.blocks_removed, 1);
        assert_eq!(stats.bytes_removed, 41);
    }

    #[test]
    fn strips_multiple_blocks_in_one_string() {
        let s = "a<system-reminder>x</system-reminder>b<system-reminder>y</system-reminder>c";
        let (out, stats) = strip_reminders_in_str(s).unwrap();
        assert_eq!(out, "abc");
        assert_eq!(stats.blocks_removed, 2);
    }

    #[test]
    fn returns_none_when_no_open_tag() {
        assert!(strip_reminders_in_str("no tags here").is_none());
        assert!(strip_reminders_in_str("</system-reminder> orphan close only").is_none());
    }

    #[test]
    fn leaves_unclosed_open_tag_alone() {
        let s = "ok <system-reminder>unterminated tail";
        assert!(strip_reminders_in_str(s).is_none());
    }

    #[test]
    fn handles_block_at_string_start_and_end() {
        let s = "<system-reminder>nudge</system-reminder>";
        let (out, stats) = strip_reminders_in_str(s).unwrap();
        assert_eq!(out, "");
        assert_eq!(stats.blocks_removed, 1);
    }

    #[test]
    fn strips_multiline_block_body() {
        let s = "lead\n<system-reminder>\nThe task tools haven't been used recently.\nHere are existing tasks:\n#1 foo\n</system-reminder>\ntail";
        let (out, stats) = strip_reminders_in_str(s).unwrap();
        assert_eq!(out, "lead\ntail");
        assert_eq!(stats.blocks_removed, 1);
    }

    #[test]
    fn strips_from_past_user_messages_only() {
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "hello <system-reminder>past nudge</system-reminder>"
                    }]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "hi"}]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "current <system-reminder>load-bearing</system-reminder>"
                    }]
                }
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 1);
        assert_eq!(v["messages"][0]["content"][0]["text"], "hello ");
        assert_eq!(
            v["messages"][2]["content"][0]["text"],
            "current <system-reminder>load-bearing</system-reminder>"
        );
    }

    #[test]
    fn strips_from_past_tool_result_content() {
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_a",
                        "content": [{
                            "type": "text",
                            "text": "log line\n<system-reminder>embedded</system-reminder>\nmore log"
                        }]
                    }]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ack"}]
                },
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "next"}]
                }
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 1);
        let stripped = v["messages"][0]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(stripped, "log line\nmore log");
    }

    #[test]
    fn no_messages_field_is_noop() {
        let mut v = json!({"system": [{"text": "hi"}]});
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert!(stats.is_empty());
    }

    #[test]
    fn empty_messages_array_is_noop() {
        let mut v = json!({"messages": []});
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert!(stats.is_empty());
    }

    #[test]
    fn single_user_message_preserved_as_current_turn() {
        let mut v = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "<system-reminder>do not strip</system-reminder>"
                }]
            }]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 0);
        assert_eq!(
            v["messages"][0]["content"][0]["text"],
            "<system-reminder>do not strip</system-reminder>"
        );
    }

    #[test]
    fn strips_across_multiple_past_messages() {
        let mut v = json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "<system-reminder>a</system-reminder>"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
                {"role": "user", "content": [{"type": "text", "text": "<system-reminder>b</system-reminder>"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "y"}]},
                {"role": "user", "content": [{"type": "text", "text": "<system-reminder>c-current</system-reminder>"}]}
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 2);
        assert_eq!(
            v["messages"][4]["content"][0]["text"],
            "<system-reminder>c-current</system-reminder>"
        );
    }

    #[test]
    fn no_match_preserves_body_exactly() {
        let mut v = json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "no reminders"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
                {"role": "user", "content": [{"type": "text", "text": "still none"}]}
            ]
        });
        let before = v.clone();
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 0);
        assert_eq!(stats.bytes_removed, 0);
        assert_eq!(v, before);
    }

    #[test]
    fn tokens_estimate_is_bytes_div_four() {
        let stats = SystemReminderStats {
            blocks_removed: 1,
            bytes_removed: 400,
        };
        assert_eq!(stats.tokens_estimate(), 100);
    }

    #[test]
    fn back_to_back_blocks_with_no_separator() {
        let s = "<system-reminder>a</system-reminder><system-reminder>b</system-reminder>tail";
        let (out, stats) = strip_reminders_in_str(s).unwrap();
        assert_eq!(out, "tail");
        assert_eq!(stats.blocks_removed, 2);
    }
}
