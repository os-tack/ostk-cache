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

use serde::Deserialize;
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
        TailEventKind::ToolResult => extract_tool_result_summary(&raw.message)
            .unwrap_or_else(|| "(tool_result)".to_string()),
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
    if parts.is_empty() { None } else { Some(parts.join(" ")) }
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

/// Compose a cross-session activity summary from tail events. Returns
/// None if there's nothing useful to report.
pub fn render_cross_session_summary(events: &[TailEvent], current_session_marker: Option<&str>) -> Option<String> {
    if events.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("## Cross-session activity (from harness transcript)\n\n");

    let mut count = 0usize;
    for ev in events {
        if let Some(marker) = current_session_marker
            && ev
                .timestamp
                .as_deref()
                .is_some_and(|ts| ts >= marker)
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
        write_jsonl(
            &f,
            &lines.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        );

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
        let out =
            render_cross_session_summary(&events, Some("2026-05-07T12:00:00Z")).unwrap();
        assert!(out.contains("old"));
        assert!(!out.contains("new"));
    }
}
