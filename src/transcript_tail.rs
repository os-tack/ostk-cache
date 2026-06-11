//! Transcript tail — Layer 3 Pattern A of the rewrite (per
//! `docs/draft/ostk-cache-adapter.md` §6 and the plan at
//! `~/.claude/plans/yes-exactly-now-that-lazy-thimble.md`).
//!
//! Reads the foreign harness's session JSONL log on demand at request
//! time — simpler than a background watcher and sufficient for the
//! MVP's cross-session-continuity goal. The harness writes JSONL
//! append-only, so reading the last N lines on each request gives us
//! a recent activity window across session boundaries.
//!
//! # Trade-offs vs a watcher
//!
//! - On-demand reads work even after the adapter restarts (no in-memory
//!   index to rebuild). Each request is self-contained.
//! - We re-parse the same lines every request — bounded by the K-tail
//!   limit so cost stays small.
//! - We may miss events that landed *during* the current request (the
//!   transcript-flush race). Mitigation: trust the request body for
//!   current-cycle events; the tail enriches *prior* cycles only.
//!
//! # Discovery
//!
//! Claude Code stores sessions at `~/.claude/projects/<encoded-cwd>/*.jsonl`.
//! The encoding is not formally documented, so we use a robust fallback:
//! glob all `*/*.jsonl` files, sort by mtime, and prefer the one whose
//! first JSONL event's `cwd` field matches the current workspace.
//!
//! # Privacy
//!
//! Reads stay local. The synthesized activity summary is composed into
//! outbound requests, not the raw transcript content. Same trust
//! boundary as the kernel reading `.ostk/journal.jsonl`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const DEFAULT_TAIL_LIMIT: usize = 50;

/// Configuration for transcript tailing.
#[derive(Debug, Clone)]
pub struct TailConfig {
    pub enabled: bool,
    pub tail_limit: usize,
    pub claude_projects_dir: PathBuf,
}

impl TailConfig {
    pub fn from_env() -> Self {
        let enabled = matches!(
            std::env::var("OSTK_CACHE_TAIL_TRANSCRIPT")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes"
        );
        let tail_limit = std::env::var("OSTK_CACHE_TAIL_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TAIL_LIMIT);
        let claude_projects_dir = std::env::var("OSTK_CACHE_CLAUDE_PROJECTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".claude").join("projects")
            });
        Self {
            enabled,
            tail_limit,
            claude_projects_dir,
        }
    }

    /// Build a tail config from the proxy's resolved [`crate::config::Config`].
    pub fn from_resolved(cfg: &crate::config::Config) -> Self {
        Self {
            enabled: cfg.tail_transcript.value,
            tail_limit: cfg.tail_limit.value,
            claude_projects_dir: cfg.claude_projects_dir.value.clone(),
        }
    }
}

/// Summary of a single transcript event observed at the tail.
#[derive(Debug, Clone)]
pub struct TailEvent {
    pub kind: TailEventKind,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub summary: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailEventKind {
    User,
    Assistant,
    ToolUse,
    ToolResult,
    Other(String),
}

/// Locate the most relevant session JSONL file for `workspace_cwd`.
///
/// Strategy:
/// 1. Glob all `<projects_dir>/*/*.jsonl`.
/// 2. Sort by mtime descending.
/// 3. For each candidate, read the first JSONL line and check if its
///    `cwd` field matches `workspace_cwd`. Return the first match.
/// 4. If no `cwd` match, return the most recently modified file
///    (fallback for sessions without a cwd anchor).
///
/// Returns None if `projects_dir` doesn't exist or contains no
/// .jsonl files.
pub fn locate_session_file(projects_dir: &Path, workspace_cwd: &Path) -> Option<PathBuf> {
    let project_dirs = std::fs::read_dir(projects_dir).ok()?;

    let mut candidates: Vec<(PathBuf, SystemTime)> = Vec::new();
    for entry in project_dirs.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let inner = match std::fs::read_dir(&p) {
            Ok(it) => it,
            Err(_) => continue,
        };
        for f in inner.flatten() {
            let fp = f.path();
            if fp.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = f
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((fp, mtime));
        }
    }

    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));

    let workspace_str = workspace_cwd.to_string_lossy();
    for (path, _) in &candidates {
        if first_line_cwd_matches(path, &workspace_str) {
            return Some(path.clone());
        }
    }

    candidates.first().map(|(p, _)| p.clone())
}

fn first_line_cwd_matches(path: &Path, workspace: &str) -> bool {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return false;
    }
    #[derive(Deserialize)]
    struct CwdProbe {
        cwd: Option<String>,
    }
    match serde_json::from_str::<CwdProbe>(&line) {
        Ok(p) => p.cwd.as_deref() == Some(workspace),
        Err(_) => false,
    }
}

/// Read the last `limit` JSONL events from a session file, parsed into
/// `TailEvent`s. Reads the entire file (bounded — claude-code rotates
/// per session) and returns the tail. Returns empty Vec on any error.
pub fn read_tail_events(path: &Path, limit: usize) -> Vec<TailEvent> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();

    let start = lines.len().saturating_sub(limit);
    lines[start..]
        .iter()
        .filter_map(|l| parse_event(l))
        .collect()
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "type")]
    type_: Option<String>,
    timestamp: Option<String>,
    message: Option<serde_json::Value>,
    #[serde(rename = "toolUseID")]
    tool_use_id_camel: Option<String>,
    tool_use_id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

fn parse_event(line: &str) -> Option<TailEvent> {
    let raw: RawEvent = serde_json::from_str(line).ok()?;
    let kind = match raw.type_.as_deref() {
        Some("user") => TailEventKind::User,
        Some("assistant") => TailEventKind::Assistant,
        Some("tool_use") => TailEventKind::ToolUse,
        Some("tool_result") => TailEventKind::ToolResult,
        Some(other) => TailEventKind::Other(other.to_string()),
        None => return None,
    };
    let tool_use_id = raw.tool_use_id.or(raw.tool_use_id_camel);
    let tool_name = raw.name.clone();

    let summary = match &kind {
        TailEventKind::User => extract_message_text(&raw.message).unwrap_or_default(),
        TailEventKind::Assistant => extract_assistant_summary(&raw.message).unwrap_or_default(),
        TailEventKind::ToolUse => format!(
            "{}: {}",
            tool_name.as_deref().unwrap_or("?"),
            raw.input
                .as_ref()
                .map(|v| truncate_chars(&v.to_string(), 120))
                .unwrap_or_default()
        ),
        TailEventKind::ToolResult => {
            extract_tool_result_summary(&raw.message).unwrap_or_else(|| "(tool_result)".to_string())
        }
        TailEventKind::Other(_) => String::new(),
    };

    Some(TailEvent {
        kind,
        tool_use_id,
        tool_name,
        summary: truncate_chars(&summary, 240),
        timestamp: raw.timestamp,
    })
}

fn extract_message_text(message: &Option<serde_json::Value>) -> Option<String> {
    let m = message.as_ref()?;
    if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from))
            .collect();
        if !texts.is_empty() {
            return Some(texts.join("\n"));
        }
    }
    None
}

fn extract_assistant_summary(message: &Option<serde_json::Value>) -> Option<String> {
    let m = message.as_ref()?;
    let arr = m.get("content").and_then(|c| c.as_array())?;
    let mut parts: Vec<String> = Vec::new();
    for block in arr {
        let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match t {
            "text" => {
                if let Some(s) = block.get("text").and_then(|s| s.as_str()) {
                    parts.push(format!("text:\"{}\"", truncate_chars(s, 80)));
                }
            }
            "tool_use" => {
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                parts.push(format!("tool_use:{}", name));
            }
            other => parts.push(format!("({})", other)),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn extract_tool_result_summary(message: &Option<serde_json::Value>) -> Option<String> {
    let m = message.as_ref()?;
    let arr = m.get("content").and_then(|c| c.as_array())?;
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            return match block.get("content") {
                Some(serde_json::Value::String(s)) => Some(format!("out:{}b", s.len())),
                Some(serde_json::Value::Array(a)) => {
                    let total: usize = a
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(|s| s.len()))
                        .sum();
                    Some(format!("out:{}b", total))
                }
                _ => Some("out:0b".into()),
            };
        }
    }
    None
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// Truncate at the last whitespace within `max` chars so messages don't
/// get chopped mid-word. Falls back to a hard char cut if no whitespace
/// is found in the second half of the window.
fn truncate_at_word_boundary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let prefix: String = s.chars().take(max).collect();
    let cut = prefix
        .rfind(char::is_whitespace)
        .filter(|i| *i > max / 2)
        .unwrap_or(prefix.len());
    let mut out = prefix[..cut].trim_end().to_string();
    out.push('\u{2026}');
    out
}

/// Read user-role text events from the transcript JSONL and return them
/// in chronological order, word-boundary-truncated at `per_msg_chars`.
///
/// Drops the most recent user-text event under the assumption it is the
/// current cycle's trigger (which the rebuild keeps in the in-flight
/// chain and re-renders separately under `## Current cycle`). Skips
/// tool_result wrapper events (`type:"user"` whose content is entirely
/// `tool_result` blocks) — those carry no user intent text.
///
/// Caller caps by K (last-N turns) on its side; this function only
/// handles the per-message budget. Returns an empty Vec on any
/// read/parse error.
pub fn read_recent_user_messages(path: &Path, per_msg_chars: usize) -> Vec<String> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);

    let mut texts: Vec<String> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let raw: RawEvent = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if raw.type_.as_deref() != Some("user") {
            continue;
        }
        if is_tool_result_user_event(&raw.message) {
            continue;
        }
        if let Some(t) = extract_message_text(&raw.message) {
            let trimmed = t.trim();
            if !trimmed.is_empty() {
                texts.push(trimmed.to_string());
            }
        }
    }

    if !texts.is_empty() {
        texts.pop();
    }

    texts
        .into_iter()
        .map(|t| truncate_at_word_boundary(&t, per_msg_chars))
        .collect()
}

fn is_tool_result_user_event(message: &Option<serde_json::Value>) -> bool {
    let Some(m) = message else { return false };
    let Some(arr) = m.get("content").and_then(|c| c.as_array()) else {
        return false;
    };
    if arr.is_empty() {
        return false;
    }
    arr.iter()
        .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
}

/// Compose a cross-session activity summary from tail events. Returns
/// None if there's nothing useful to report.
pub fn render_cross_session_summary(
    events: &[TailEvent],
    current_session_marker: Option<&str>,
) -> Option<String> {
    if events.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("## Cross-session activity (from harness transcript)\n\n");

    let mut count = 0usize;
    for ev in events {
        if let Some(marker) = current_session_marker
            && ev.timestamp.as_deref().is_some_and(|ts| ts >= marker)
        {
            continue;
        }

        let kind_label = match &ev.kind {
            TailEventKind::User => "user",
            TailEventKind::Assistant => "asst",
            TailEventKind::ToolUse => "tool",
            TailEventKind::ToolResult => "tres",
            TailEventKind::Other(s) => s.as_str(),
        };
        let ts = ev.timestamp.as_deref().unwrap_or("?");
        out.push_str(&format!("- [{}] {}: {}\n", ts, kind_label, ev.summary));
        count += 1;
    }

    if count == 0 {
        return None;
    }
    Some(out)
}

/// Locator for a tool_result body in the transcript JSONL.
///
/// Byte offsets are relative to the start of the file and point at the
/// raw JSONL line — callers can `seek + read_exact` to recover the
/// exact bytes, then parse the line and pull `.message.content[…].content`
/// out of it. Line index is 0-based and meant for human debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolBodyLocator {
    pub tool_use_id: String,
    pub line_index: u64,
    pub byte_offset: u64,
    pub byte_len: u64,
    pub is_error: bool,
    pub tool_name: Option<String>,
}

/// Result of a single tail-scan pass over a transcript JSONL.
///
/// The proxy builds this once per inbound request when `tail.transcript`
/// is on, then feeds three slots into the projection:
/// - `events` → the cross-session activity summary (existing path)
/// - `user_turns` → less-lossy prior-user-intent thread (no 240 chop)
/// - `by_tool_id` → handle-resolve interface for tool_result bodies
///
/// `path` is preserved so a resolve_handle verb can seek+read by
/// `byte_offset/byte_len` without re-scanning.
#[derive(Debug, Clone)]
pub struct TailIndex {
    pub path: PathBuf,
    pub events: Vec<TailEvent>,
    pub user_turns: Vec<String>,
    pub by_tool_id: HashMap<String, ToolBodyLocator>,
}

/// Single-pass scan over the JSONL: tracks byte offsets for each line,
/// retains the last `limit` events (preserving the existing `read_tail_events`
/// semantics), accumulates ALL user turns (full text, untruncated), and
/// indexes EVERY tool_result observed by `tool_use_id`.
///
/// The user-turn and tool-id slots are intentionally NOT tail-limited:
/// they're cheap (string clones and a small struct), and a session-wide
/// view makes the index useful for resolving handles older than `limit`.
pub fn build_tail_index(path: &Path, limit: usize) -> TailIndex {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return TailIndex {
                path: path.to_path_buf(),
                events: Vec::new(),
                user_turns: Vec::new(),
                by_tool_id: HashMap::new(),
            };
        }
    };
    let reader = BufReader::new(file);

    let mut events: Vec<TailEvent> = Vec::new();
    let mut user_turns: Vec<String> = Vec::new();
    let mut by_tool_id: HashMap<String, ToolBodyLocator> = HashMap::new();

    let mut byte_offset: u64 = 0;
    let mut line_index: u64 = 0;
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };
        // BufRead::lines strips the trailing \n. Round-trip length back
        // for the offset cursor.
        let raw_len = line.len() as u64 + 1;

        if let Some(ev) = parse_event(&line) {
            // Full-text user turn (no truncation). parse_event truncates
            // the summary to 240; re-extract from raw for fidelity.
            if matches!(ev.kind, TailEventKind::User)
                && let Ok(raw_val) = serde_json::from_str::<serde_json::Value>(&line)
                && let Some(text) = extract_message_text(&raw_val.get("message").cloned())
            {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    user_turns.push(trimmed.to_string());
                }
            }

            if matches!(ev.kind, TailEventKind::ToolResult)
                && let Some(id) = ev.tool_use_id.clone()
            {
                let is_error = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| {
                        v.get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_array())
                            .and_then(|arr| {
                                arr.iter()
                                    .find_map(|b| b.get("is_error").and_then(|e| e.as_bool()))
                            })
                    })
                    .unwrap_or(false);
                by_tool_id.insert(
                    id.clone(),
                    ToolBodyLocator {
                        tool_use_id: id,
                        line_index,
                        byte_offset,
                        byte_len: line.len() as u64,
                        is_error,
                        tool_name: ev.tool_name.clone(),
                    },
                );
            }

            events.push(ev);
        }

        byte_offset += raw_len;
        line_index += 1;
    }

    // Apply tail limit ONLY to the events slot — it's what feeds the
    // cross-session summary. user_turns + by_tool_id stay session-wide.
    if events.len() > limit {
        let drop_n = events.len() - limit;
        events.drain(..drop_n);
    }

    TailIndex {
        path: path.to_path_buf(),
        events,
        user_turns,
        by_tool_id,
    }
}

/// Short tag for a tool_use_id — last 8 hex chars, lowercased. The
/// transcript ids are claude-code's `toolu_<base32-ish>` strings; the
/// suffix is sufficiently unique within a session and addressable by
/// eye.
pub fn short_tag(tool_use_id: &str) -> String {
    let len = tool_use_id.chars().count();
    let tail: String = tool_use_id.chars().skip(len.saturating_sub(8)).collect();
    tail.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_jsonl(path: &Path, events: &[&str]) {
        let mut f = fs::File::create(path).unwrap();
        for e in events {
            writeln!(f, "{}", e).unwrap();
        }
    }

    #[test]
    fn locate_session_returns_none_for_missing_dir() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(locate_session_file(&missing, dir.path()).is_none());
    }

    #[test]
    fn locate_session_finds_jsonl() {
        let projects = tempdir().unwrap();
        let project_a = projects.path().join("a");
        fs::create_dir(&project_a).unwrap();
        let session_file = project_a.join("session1.jsonl");
        write_jsonl(
            &session_file,
            &[r#"{"type":"user","cwd":"/some/path","message":{"content":"hi"}}"#],
        );

        let found = locate_session_file(projects.path(), Path::new("/some/path"));
        assert_eq!(found, Some(session_file));
    }

    #[test]
    fn locate_session_prefers_cwd_match_over_mtime() {
        let projects = tempdir().unwrap();
        let project_a = projects.path().join("a");
        let project_b = projects.path().join("b");
        fs::create_dir(&project_a).unwrap();
        fs::create_dir(&project_b).unwrap();

        // Write the cwd-matching file FIRST (older mtime).
        let older_match = project_b.join("older.jsonl");
        write_jsonl(
            &older_match,
            &[r#"{"type":"user","cwd":"/right","message":{"content":"hi"}}"#],
        );

        // Sleep so mtimes differ at fs granularity.
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Write a NEWER file with a non-matching cwd.
        let recent = project_a.join("recent.jsonl");
        write_jsonl(
            &recent,
            &[r#"{"type":"user","cwd":"/wrong","message":{"content":"hi"}}"#],
        );

        // Despite `recent` being newer, locate_session_file should
        // prefer the cwd-matched `older_match`.
        let found = locate_session_file(projects.path(), Path::new("/right"));
        assert_eq!(found.as_deref(), Some(older_match.as_path()));
    }

    #[test]
    fn read_tail_returns_last_n() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        let lines: Vec<String> = (0..10)
            .map(|i| {
                format!(
                    r#"{{"type":"user","timestamp":"t{}","message":{{"content":"msg{}"}}}}"#,
                    i, i
                )
            })
            .collect();
        write_jsonl(&f, &lines.iter().map(|s| s.as_str()).collect::<Vec<_>>());

        let events = read_tail_events(&f, 3);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].timestamp.as_deref(), Some("t7"));
        assert_eq!(events[2].timestamp.as_deref(), Some("t9"));
    }

    #[test]
    fn parse_tool_use_event() {
        let line = r#"{"type":"tool_use","timestamp":"2026-05-07T13:00:00Z","name":"Bash","tool_use_id":"tu_X","input":{"cmd":"ls"}}"#;
        let ev = parse_event(line).unwrap();
        assert_eq!(ev.kind, TailEventKind::ToolUse);
        assert_eq!(ev.tool_use_id.as_deref(), Some("tu_X"));
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        assert!(ev.summary.contains("Bash"));
    }

    #[test]
    fn render_summary_returns_none_for_empty() {
        assert!(render_cross_session_summary(&[], None).is_none());
    }

    #[test]
    fn render_summary_excludes_events_after_marker() {
        let events = vec![
            TailEvent {
                kind: TailEventKind::User,
                tool_use_id: None,
                tool_name: None,
                summary: "old".into(),
                timestamp: Some("2026-05-07T10:00:00Z".into()),
            },
            TailEvent {
                kind: TailEventKind::User,
                tool_use_id: None,
                tool_name: None,
                summary: "new".into(),
                timestamp: Some("2026-05-07T15:00:00Z".into()),
            },
        ];
        let out = render_cross_session_summary(&events, Some("2026-05-07T12:00:00Z")).unwrap();
        assert!(out.contains("old"));
        assert!(!out.contains("new"));
    }

    #[test]
    fn truncate_at_word_boundary_keeps_whole_short_strings() {
        assert_eq!(truncate_at_word_boundary("hi there", 20), "hi there");
    }

    #[test]
    fn truncate_at_word_boundary_cuts_at_whitespace() {
        let s = "alpha beta gamma delta epsilon";
        let out = truncate_at_word_boundary(s, 15);
        assert!(out.ends_with('\u{2026}'));
        assert!(!out.contains("gamm"), "should not chop mid-word: {out}");
        assert!(out.starts_with("alpha beta"));
    }

    #[test]
    fn read_recent_user_messages_drops_last_and_skips_tool_results() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"user","message":{"content":"first user turn"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ack"}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ignored wrapper"}]}}"#,
                r#"{"type":"user","message":{"content":"second user turn"}}"#,
                r#"{"type":"user","message":{"content":"current cycle trigger"}}"#,
            ],
        );

        let msgs = read_recent_user_messages(&f, 1000);
        assert_eq!(msgs.len(), 2, "got: {:?}", msgs);
        assert_eq!(msgs[0], "first user turn");
        assert_eq!(msgs[1], "second user turn");
    }

    #[test]
    fn read_recent_user_messages_applies_word_boundary_budget() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("s.jsonl");
        let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        write_jsonl(
            &f,
            &[
                &format!(r#"{{"type":"user","message":{{"content":"{}"}}}}"#, long),
                r#"{"type":"user","message":{"content":"current cycle trigger"}}"#,
            ],
        );

        let msgs = read_recent_user_messages(&f, 20);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].ends_with('\u{2026}'));
        assert!(
            !msgs[0].contains("kappa"),
            "long message should be truncated: {}",
            msgs[0]
        );
        assert!(msgs[0].starts_with("alpha"));
    }
}

// ---------------------------------------------------------------------------
// Codex rollout tail (C1, cross-provider lane)
// ---------------------------------------------------------------------------
//
// Codex stores sessions GLOBALLY at
// `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` — unlike
// Claude Code there is no per-project directory, and a foreign-project
// codex may be writing concurrently. Identity is therefore the TUPLE
// {rollout UUID, session_meta.cwd, seat alias, PPID chain} — NEVER
// mtime alone (spec-gpt-cache-lane-20260612.md §5; protocol
// contributed by the codex seat, adopted 2026-06-11). The file-level
// elements this tail can verify:
//
// - `session_meta.cwd` (first line) must equal the workspace root —
//   the load-bearing filter against the foreign codex.
// - the UUID embedded in the filename must equal `session_meta.id` —
//   rejects renamed/copied rollouts.
//
// Seat alias and PPID chain are process-runtime elements verified by
// the kernel, not recoverable from the file; this module's contract is
// the file-level pair. There is deliberately NO most-recent-any
// fallback (locate_session_file's step 4): in a global sessions dir a
// recency fallback IS the foreign-codex bug. No cwd match → None.
//
// Recency among legitimate matches is resolved by the rollout
// filename's embedded timestamp (lexicographically sortable
// YYYY-MM-DDTHH-MM-SS), not mtime.

/// Default codex sessions root: `~/.codex/sessions`.
pub fn default_codex_sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".codex").join("sessions")
}

/// File-level identity of a codex rollout: the verifiable half of the
/// §5 identity tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRolloutIdentity {
    /// `session_meta.payload.id`, cross-checked against the filename.
    pub uuid: String,
    /// `session_meta.payload.cwd` — the workspace this rollout belongs to.
    pub cwd: String,
}

/// Parse and verify the file-level identity of a rollout.
///
/// Returns `None` unless ALL hold: first line parses, `type` is
/// `session_meta`, payload carries `id` + `cwd`, and the filename's
/// embedded UUID equals `payload.id`.
pub fn codex_rollout_identity(path: &Path) -> Option<CodexRolloutIdentity> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;

    #[derive(Deserialize)]
    struct MetaProbe {
        #[serde(rename = "type")]
        type_: String,
        payload: MetaPayload,
    }
    #[derive(Deserialize)]
    struct MetaPayload {
        id: String,
        cwd: String,
    }
    let probe: MetaProbe = serde_json::from_str(&line).ok()?;
    if probe.type_ != "session_meta" {
        return None;
    }
    // Filename shape: rollout-<YYYY-MM-DDTHH-MM-SS>-<uuid>.jsonl — the
    // UUID is the trailing 36 chars of the stem.
    let stem = path.file_stem()?.to_str()?;
    let fname_uuid = stem.get(stem.len().checked_sub(36)?..)?;
    if fname_uuid != probe.payload.id {
        return None;
    }
    Some(CodexRolloutIdentity {
        uuid: probe.payload.id,
        cwd: probe.payload.cwd,
    })
}

/// Locate the most recent codex rollout belonging to `workspace_cwd`.
///
/// Walks `<sessions_dir>/YYYY/MM/DD/rollout-*.jsonl`, keeps only files
/// whose verified identity (`codex_rollout_identity`) has
/// `cwd == workspace_cwd`, and resolves recency by filename timestamp
/// (descending). Returns `None` when nothing matches — never falls
/// back to recency-without-identity.
pub fn locate_codex_rollout(sessions_dir: &Path, workspace_cwd: &Path) -> Option<PathBuf> {
    let workspace = workspace_cwd.to_string_lossy();
    let mut matches: Vec<PathBuf> = Vec::new();
    let mut stack = vec![sessions_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                && codex_rollout_identity(&p).is_some_and(|id| id.cwd == workspace)
            {
                matches.push(p);
            }
        }
    }
    // Filename embeds a lexicographically sortable timestamp; latest
    // session = greatest filename. Deterministic, mtime-free.
    matches.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    matches.into_iter().next()
}

/// Read the last `limit` *mapped* events from a codex rollout, parsed
/// into the same [`TailEvent`] vocabulary the Claude tail produces.
///
/// Mapping (event shapes pinned from live rollout 019eb8a0,
/// 2026-06-11):
/// - `event_msg/user_message`        → [`TailEventKind::User`]
/// - `event_msg/agent_message`       → [`TailEventKind::Assistant`]
/// - `response_item/function_call`   → [`TailEventKind::ToolUse`]
/// - `response_item/function_call_output` → [`TailEventKind::ToolResult`]
/// - everything else (reasoning, token_count, turn_context, …) is
///   skipped — same altitude as the Claude tail's summary stream.
pub fn read_codex_tail_events(path: &Path, limit: usize) -> Vec<TailEvent> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let events: Vec<TailEvent> = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| parse_codex_event(&l))
        .collect();
    let start = events.len().saturating_sub(limit);
    events[start..].to_vec()
}

fn parse_codex_event(line: &str) -> Option<TailEvent> {
    #[derive(Deserialize)]
    struct RawCodexEvent {
        #[serde(rename = "type")]
        type_: String,
        timestamp: Option<String>,
        payload: serde_json::Value,
    }
    let raw: RawCodexEvent = serde_json::from_str(line).ok()?;
    let ptype = raw.payload.get("type").and_then(|t| t.as_str())?;
    let event = match (raw.type_.as_str(), ptype) {
        ("event_msg", "user_message") => TailEvent {
            kind: TailEventKind::User,
            tool_use_id: None,
            tool_name: None,
            summary: raw
                .payload
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            timestamp: raw.timestamp,
        },
        ("event_msg", "agent_message") => TailEvent {
            kind: TailEventKind::Assistant,
            tool_use_id: None,
            tool_name: None,
            summary: raw
                .payload
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or_default()
                .to_string(),
            timestamp: raw.timestamp,
        },
        ("response_item", "function_call") => {
            // Codex namespaces MCP tools (`namespace: "mcp__ostk"`,
            // `name: "bash"`); render as namespace::name to match the
            // fleet's qualified-tool vocabulary.
            let name = raw.payload.get("name").and_then(|n| n.as_str());
            let namespace = raw.payload.get("namespace").and_then(|n| n.as_str());
            let qualified = match (namespace, name) {
                (Some(ns), Some(n)) => format!("{ns}::{n}"),
                (None, Some(n)) => n.to_string(),
                _ => "?".to_string(),
            };
            let args = raw
                .payload
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or_default();
            TailEvent {
                kind: TailEventKind::ToolUse,
                tool_use_id: raw
                    .payload
                    .get("call_id")
                    .and_then(|c| c.as_str())
                    .map(String::from),
                tool_name: Some(qualified.clone()),
                summary: format!("{}: {}", qualified, truncate_chars(args, 120)),
                timestamp: raw.timestamp,
            }
        }
        ("response_item", "function_call_output") => {
            let out_len = raw
                .payload
                .get("output")
                .and_then(|o| o.as_str())
                .map(str::len)
                .unwrap_or(0);
            TailEvent {
                kind: TailEventKind::ToolResult,
                tool_use_id: raw
                    .payload
                    .get("call_id")
                    .and_then(|c| c.as_str())
                    .map(String::from),
                tool_name: None,
                summary: format!("out:{out_len}b"),
                timestamp: raw.timestamp,
            }
        }
        _ => return None,
    };
    let summary = truncate_chars(&event.summary, 240);
    Some(TailEvent { summary, ..event })
}

#[cfg(test)]
mod codex_tests {
    use super::*;

    fn meta_line(uuid: &str, cwd: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-06-11T21:40:18.403Z","type":"session_meta","payload":{{"id":"{uuid}","cwd":"{cwd}","originator":"codex-tui","cli_version":"0.139.0","source":"cli","model_provider":"openai"}}}}"#
        )
    }

    fn write_rollout(dir: &Path, ts: &str, uuid: &str, lines: &[String]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(format!("rollout-{ts}-{uuid}.jsonl"));
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    const UUID_A: &str = "019eb8a0-d837-7ca3-bc9a-0ed5bb6ff73c";
    const UUID_B: &str = "0199aaaa-bbbb-7ccc-8ddd-eeeeffff0000";

    #[test]
    fn identity_verifies_uuid_and_cwd() {
        let tmp = tempfile::TempDir::new().unwrap();
        let day = tmp.path().join("2026/06/11");
        let p = write_rollout(
            &day,
            "2026-06-11T23-40-09",
            UUID_A,
            &[meta_line(UUID_A, "/work/haystack")],
        );
        let id = codex_rollout_identity(&p).unwrap();
        assert_eq!(id.uuid, UUID_A);
        assert_eq!(id.cwd, "/work/haystack");
    }

    #[test]
    fn identity_rejects_filename_uuid_mismatch() {
        // Renamed/copied rollout: payload.id != filename uuid.
        let tmp = tempfile::TempDir::new().unwrap();
        let day = tmp.path().join("2026/06/11");
        let p = write_rollout(
            &day,
            "2026-06-11T23-40-09",
            UUID_B,
            &[meta_line(UUID_A, "/work/haystack")],
        );
        assert_eq!(codex_rollout_identity(&p), None);
    }

    #[test]
    fn identity_rejects_non_session_meta_first_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let day = tmp.path().join("2026/06/11");
        let p = write_rollout(
            &day,
            "2026-06-11T23-40-09",
            UUID_A,
            &[
                r#"{"type":"event_msg","payload":{"type":"user_message","message":"x"}}"#
                    .to_string(),
            ],
        );
        assert_eq!(codex_rollout_identity(&p), None);
    }

    #[test]
    fn locate_filters_by_cwd_foreign_codex_negative() {
        // The §5 scenario: global dir, foreign-project codex active.
        let tmp = tempfile::TempDir::new().unwrap();
        let day = tmp.path().join("2026/06/11");
        write_rollout(
            &day,
            "2026-06-11T23-50-00", // foreign is NEWER
            UUID_B,
            &[meta_line(UUID_B, "/work/other-project")],
        );
        let ours = write_rollout(
            &day,
            "2026-06-11T23-40-09",
            UUID_A,
            &[meta_line(UUID_A, "/work/haystack")],
        );
        let found = locate_codex_rollout(tmp.path(), Path::new("/work/haystack"));
        assert_eq!(
            found,
            Some(ours),
            "must pick ours despite foreign being newer"
        );
    }

    #[test]
    fn locate_no_match_returns_none_never_recency_fallback() {
        let tmp = tempfile::TempDir::new().unwrap();
        let day = tmp.path().join("2026/06/11");
        write_rollout(
            &day,
            "2026-06-11T23-50-00",
            UUID_B,
            &[meta_line(UUID_B, "/work/other-project")],
        );
        assert_eq!(
            locate_codex_rollout(tmp.path(), Path::new("/work/haystack")),
            None,
            "global sessions dir: recency fallback IS the foreign-codex bug"
        );
    }

    #[test]
    fn locate_picks_latest_among_matching_by_filename_ts() {
        let tmp = tempfile::TempDir::new().unwrap();
        let older_day = tmp.path().join("2026/06/10");
        let newer_day = tmp.path().join("2026/06/11");
        write_rollout(
            &older_day,
            "2026-06-10T09-00-00",
            UUID_B,
            &[meta_line(UUID_B, "/work/haystack")],
        );
        let newer = write_rollout(
            &newer_day,
            "2026-06-11T23-40-09",
            UUID_A,
            &[meta_line(UUID_A, "/work/haystack")],
        );
        assert_eq!(
            locate_codex_rollout(tmp.path(), Path::new("/work/haystack")),
            Some(newer)
        );
    }

    #[test]
    fn parses_live_wire_event_shapes() {
        // Shapes verbatim-derived from live rollout 019eb8a0 (2026-06-11).
        let user = r#"{"timestamp":"2026-06-11T21:40:20.000Z","type":"event_msg","payload":{"type":"user_message","message":":boot","images":[],"local_images":[],"text_elements":[]}}"#;
        let e = parse_codex_event(user).unwrap();
        assert_eq!(e.kind, TailEventKind::User);
        assert_eq!(e.summary, ":boot");

        let agent = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Booting the project kernel now.","phase":"commentary","memory_citation":null}}"#;
        let e = parse_codex_event(agent).unwrap();
        assert_eq!(e.kind, TailEventKind::Assistant);
        assert_eq!(e.summary, "Booting the project kernel now.");

        let call = r#"{"type":"response_item","payload":{"type":"function_call","name":"bash","namespace":"mcp__ostk","arguments":"{\"cmd\":\"ostk boot\"}","call_id":"call_6yteRpHnR7O"}}"#;
        let e = parse_codex_event(call).unwrap();
        assert_eq!(e.kind, TailEventKind::ToolUse);
        assert_eq!(e.tool_name.as_deref(), Some("mcp__ostk::bash"));
        assert_eq!(e.tool_use_id.as_deref(), Some("call_6yteRpHnR7O"));
        assert!(e.summary.starts_with("mcp__ostk::bash: "));

        let output = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_6yteRpHnR7O","output":"Wall time: 4.65 seconds"}}"#;
        let e = parse_codex_event(output).unwrap();
        assert_eq!(e.kind, TailEventKind::ToolResult);
        assert_eq!(e.tool_use_id.as_deref(), Some("call_6yteRpHnR7O"));
        assert_eq!(e.summary, "out:23b");

        // Skipped kinds: reasoning, token_count, turn_context.
        for skipped in [
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
            r#"{"type":"turn_context","payload":{"type":"turn_context","cwd":"/x"}}"#,
        ] {
            assert!(parse_codex_event(skipped).is_none(), "must skip: {skipped}");
        }
    }

    #[test]
    fn codex_tail_respects_limit_and_order() {
        let tmp = tempfile::TempDir::new().unwrap();
        let day = tmp.path().join("2026/06/11");
        let mut lines = vec![meta_line(UUID_A, "/work/haystack")];
        for i in 0..10 {
            lines.push(format!(
                r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"msg-{i}"}}}}"#
            ));
        }
        let p = write_rollout(&day, "2026-06-11T23-40-09", UUID_A, &lines);
        let tail = read_codex_tail_events(&p, 3);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].summary, "msg-7");
        assert_eq!(tail[2].summary, "msg-9");
    }

    /// Live smoke (inert without env): set OSTK_CODEX_SMOKE_DIR +
    /// OSTK_CODEX_SMOKE_CWD to run the locate+tail path against a real
    /// sessions tree.
    #[test]
    fn live_sessions_smoke_when_env_set() {
        let (Ok(dir), Ok(cwd)) = (
            std::env::var("OSTK_CODEX_SMOKE_DIR"),
            std::env::var("OSTK_CODEX_SMOKE_CWD"),
        ) else {
            return;
        };
        let found = locate_codex_rollout(Path::new(&dir), Path::new(&cwd))
            .expect("smoke: no rollout located for cwd");
        let id = codex_rollout_identity(&found).expect("smoke: identity must verify");
        assert_eq!(id.cwd, cwd);
        let tail = read_codex_tail_events(&found, 10);
        assert!(!tail.is_empty(), "smoke: tail must yield mapped events");
    }
}
