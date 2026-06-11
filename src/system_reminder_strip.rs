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

use serde_json::{Value, json};

const OPEN_TAG: &str = "<system-reminder>";
const CLOSE_TAG: &str = "</system-reminder>";

// Inserted when stripping leaves a content array empty. Anthropic rejects
// messages with zero content blocks, so we keep the slot to preserve
// tool_use/tool_result pairings by index and substitute a minimal text.
const PLACEHOLDER_TEXT: &str = "(reminder)";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemReminderStats {
    pub blocks_removed: usize,
    pub bytes_removed: usize,
    // Empty {type:"text", text:""} blocks pruned after a strip emptied
    // them. The Anthropic API rejects empty text content blocks with
    // 400: "text content blocks must be non-empty".
    pub empty_blocks_pruned: usize,
    // Placeholder text blocks inserted because pruning would otherwise
    // have emptied a content array entirely.
    pub placeholders_inserted: usize,
}

impl SystemReminderStats {
    pub fn is_empty(self) -> bool {
        self.blocks_removed == 0 && self.empty_blocks_pruned == 0 && self.placeholders_inserted == 0
    }

    pub fn add(&mut self, other: Self) {
        self.blocks_removed += other.blocks_removed;
        self.bytes_removed += other.bytes_removed;
        self.empty_blocks_pruned += other.empty_blocks_pruned;
        self.placeholders_inserted += other.placeholders_inserted;
    }

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
/// Recursively prune `{type:"text", text:""}` from every array reachable
/// from `value`. When an array is emptied as a result, insert a single
/// placeholder text block so callers (Anthropic) don't reject the request
/// for a zero-block content array. Returns (pruned, placeholders).
fn prune_empty_text_blocks(value: &mut Value) -> (usize, usize) {
    let mut pruned = 0usize;
    let mut placeholders = 0usize;
    match value {
        Value::Array(arr) => {
            let before = arr.len();
            arr.retain(|item| {
                if let Value::Object(obj) = item {
                    let is_text = obj.get("type").and_then(|t| t.as_str()) == Some("text");
                    let is_empty = obj.get("text").and_then(|t| t.as_str()) == Some("");
                    !(is_text && is_empty)
                } else {
                    true
                }
            });
            let local_pruned = before - arr.len();
            pruned += local_pruned;
            if local_pruned > 0 && arr.is_empty() {
                arr.push(json!({"type": "text", "text": PLACEHOLDER_TEXT}));
                placeholders += 1;
            }
            for item in arr.iter_mut() {
                let (p, ph) = prune_empty_text_blocks(item);
                pruned += p;
                placeholders += ph;
            }
        }
        Value::Object(obj) => {
            for (_, v) in obj.iter_mut() {
                let (p, ph) = prune_empty_text_blocks(v);
                pruned += p;
                placeholders += ph;
            }
        }
        _ => {}
    }
    (pruned, placeholders)
}

fn is_tool_result_user_message(msg: &Value) -> bool {
    if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let Some(arr) = msg.get("content").and_then(|c| c.as_array()) else {
        return false;
    };
    !arr.is_empty()
        && arr
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

fn protected_current_user_idx(messages: &[Value]) -> Option<usize> {
    messages
        .iter()
        .rposition(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("user")
                && !is_tool_result_user_message(m)
        })
        .or_else(|| {
            messages
                .iter()
                .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        })
}

pub fn strip_system_reminders_from_past_turns(value: &mut Value) -> SystemReminderStats {
    let messages = match value.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return SystemReminderStats::default(),
    };
    let protected_user_idx = protected_current_user_idx(messages);
    let mut acc = SystemReminderStats::default();
    for (i, msg) in messages.iter_mut().enumerate() {
        if Some(i) == protected_user_idx {
            continue;
        }
        let Some(content) = msg.get_mut("content") else {
            continue;
        };
        let strip_stats = strip_reminders_recursive(content);
        if strip_stats.blocks_removed == 0 {
            continue;
        }
        acc.add(strip_stats);
        let (pruned, placeholders) = prune_empty_text_blocks(content);
        acc.empty_blocks_pruned += pruned;
        acc.placeholders_inserted += placeholders;
        // Bare-string content (older shape): strip may leave "".
        if let Value::String(s) = content {
            if s.is_empty() {
                *s = PLACEHOLDER_TEXT.to_string();
                acc.placeholders_inserted += 1;
            }
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
    fn preserves_current_real_user_before_tool_result_wrapper() {
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "old <system-reminder>past</system-reminder>"
                    }]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ack"}]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": "current <system-reminder>load-bearing</system-reminder>"
                    }]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {}}]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_a",
                        "content": [{"type": "text", "text": "ok"}]
                    }]
                }
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 1);
        assert_eq!(v["messages"][0]["content"][0]["text"], "old ");
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
            empty_blocks_pruned: 0,
            placeholders_inserted: 0,
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

    // Repros the production 400: Claude Code's first user turn contains
    // several reminder-only text blocks followed by the real user text.
    // On turn 2, this becomes a "past" message; pre-fix, strip left three
    // empty text blocks and Anthropic returned:
    //   400 messages: text content blocks must be non-empty
    #[test]
    fn prunes_empty_text_blocks_left_by_strip() {
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "<system-reminder>MCP instructions</system-reminder>"},
                        {"type": "text", "text": "<system-reminder>skills list</system-reminder>"},
                        {"type": "text", "text": "<system-reminder>auto mode</system-reminder>"},
                        {"type": "text", "text": "Hello, what were we working on?"}
                    ]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "Hi!"}]},
                {"role": "user", "content": [{"type": "text", "text": "follow-up"}]}
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 3);
        assert_eq!(stats.empty_blocks_pruned, 3);
        assert_eq!(stats.placeholders_inserted, 0);
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "Hello, what were we working on?");
        for blk in content {
            if blk["type"] == "text" {
                assert_ne!(blk["text"], "", "no empty text block must remain");
            }
        }
    }

    #[test]
    fn inserts_placeholder_when_all_blocks_were_reminders() {
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "<system-reminder>only this</system-reminder>"}
                    ]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "ack"}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]}
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 1);
        assert_eq!(stats.empty_blocks_pruned, 1);
        assert_eq!(stats.placeholders_inserted, 1);
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert!(
            !content[0]["text"].as_str().unwrap().is_empty(),
            "placeholder must be non-empty"
        );
    }

    #[test]
    fn does_not_prune_whitespace_only_text() {
        // Whitespace-only text is accepted by Anthropic; leave it alone.
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "\n<system-reminder>x</system-reminder>"}
                    ]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "x"}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]}
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 1);
        assert_eq!(stats.empty_blocks_pruned, 0);
        assert_eq!(stats.placeholders_inserted, 0);
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "\n");
    }

    #[test]
    fn prunes_empty_text_inside_tool_result_content() {
        let mut v = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_a",
                        "content": [
                            {"type": "text", "text": "<system-reminder>only</system-reminder>"}
                        ]
                    }]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "ack"}]},
                {"role": "user", "content": [{"type": "text", "text": "next"}]}
            ]
        });
        let stats = strip_system_reminders_from_past_turns(&mut v);
        assert_eq!(stats.blocks_removed, 1);
        assert_eq!(stats.empty_blocks_pruned, 1);
        assert_eq!(stats.placeholders_inserted, 1);
        let tool_content = v["messages"][0]["content"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(tool_content.len(), 1);
        assert!(!tool_content[0]["text"].as_str().unwrap().is_empty());
    }
}
