//! Cycle digest harvester — Layer 1 of the per-cycle assistant-summary
//! work (per the projection_doctrine_trichotomy decision and needle
//! →1829 which tracks Layer 2's kernel-native ternary-bonsai variant).
//!
//! # Mechanism
//!
//! The discipline preamble (rebuild.rs) teaches the assistant to emit
//! a `<turn-digest>{...}</turn-digest>` fenced JSON block at the end
//! of every response. The proxy observes the completed SSE stream,
//! extracts the digest, persists it to the standalone state directory,
//! and the rebuilder pulls the last K digests into the next
//! projection's `## Recent assistant turns` section.
//!
//! # Why a fence and not an MCP tool
//!
//! Layer 1 is proxy-only — no kernel changes. We can't define a new
//! MCP tool the assistant calls because that requires kernel
//! dispatcher work (and we already track that as part of Layer 2).
//! The fence-in-text discipline is shippable today and is *additive*:
//! when the assistant emits the fence we get a rich digest; when it
//! doesn't, the projection falls through to existing tool-call shape
//! summaries.
//!
//! # Failure mode
//!
//! Any failure (no fence, malformed JSON, unwritable state dir) is
//! silently logged and skipped. Digests are additive enrichment;
//! their absence never blocks the proxy.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

const FENCE_OPEN: &str = "<turn-digest>";
const FENCE_CLOSE: &str = "</turn-digest>";
const DIGEST_FILE: &str = "cycle_digests.jsonl";

/// One cycle digest, persisted as a JSONL row in the standalone state
/// directory. Fields mirror the schema we teach in the discipline
/// preamble; everything optional so partial digests still parse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleDigest {
    pub ts: String,
    pub session: String,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub narrative: Option<String>,
}

/// Parse a `<turn-digest>{...}</turn-digest>` fence out of the
/// assistant's full response body. Accepts either:
/// - the raw SSE stream (in which case we extract assistant text first)
/// - already-extracted assistant text
///
/// If multiple fences are present (the model retried), the LAST one wins.
pub fn parse_digest(body: &str) -> Option<CycleDigest> {
    // Cheap heuristic: if the body looks like SSE, extract text first.
    let text = if body.contains("event: ") || body.contains("\ndata: ") {
        extract_assistant_text_from_sse(body)
    } else {
        body.to_string()
    };

    let last_open = text.rfind(FENCE_OPEN)?;
    let after = &text[last_open + FENCE_OPEN.len()..];
    let close_rel = after.find(FENCE_CLOSE)?;
    let json_body = after[..close_rel].trim();

    let parsed: serde_json::Value = serde_json::from_str(json_body).ok()?;

    let ts = chrono_iso8601();
    let intent = parsed
        .get("intent")
        .and_then(|v| v.as_str())
        .map(String::from);
    let outcome = parsed
        .get("outcome")
        .and_then(|v| v.as_str())
        .map(String::from);
    let narrative = parsed
        .get("narrative")
        .and_then(|v| v.as_str())
        .map(String::from);
    let artifacts = parsed
        .get("artifacts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Some(CycleDigest {
        ts,
        session: String::new(),
        intent,
        outcome,
        artifacts,
        narrative,
    })
}

/// Extract concatenated assistant text from an Anthropic SSE stream by
/// walking `content_block_delta` events with text deltas.
fn extract_assistant_text_from_sse(body: &str) -> String {
    let mut out = String::new();
    let mut current_event: Option<&str> = None;
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            current_event = None;
        } else if let Some(rest) = line.strip_prefix("event: ") {
            current_event = Some(rest);
        } else if let Some(rest) = line.strip_prefix("data: ") {
            let event = current_event.unwrap_or("");
            if event != "content_block_delta" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest)
                && let Some(delta) = v.get("delta")
                && delta.get("type").and_then(|t| t.as_str()) == Some("text_delta")
                && let Some(text) = delta.get("text").and_then(|t| t.as_str())
            {
                out.push_str(text);
            }
        }
    }
    out
}

/// Append a digest to the standalone state dir's `cycle_digests.jsonl`.
/// The state dir is bootstrapped lazily.
pub fn write_digest(workspace: &Path, digest: &CycleDigest) -> std::io::Result<()> {
    let dir = crate::standalone::ensure_state_dir(workspace)?;
    let path = dir.join(DIGEST_FILE);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    let mut line = serde_json::to_string(digest).unwrap_or_else(|_| "{}".into());
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Read up to `n` most-recent digests from the standalone state dir.
/// Returns empty Vec on any read error (digests are best-effort
/// enrichment; no harm if missing).
pub fn read_recent(workspace: &Path, n: usize) -> Vec<CycleDigest> {
    read_recent_where(workspace, n, |_| true)
}

/// Read up to `n` most-recent digests stamped with `session`. The
/// digest file is shared per-workspace, so without this filter a
/// multi-seat fleet pools every seat's digests into each seat's
/// first-person history (→1973 digest-bleed → self-misID).
pub fn read_recent_for_session(workspace: &Path, session: &str, n: usize) -> Vec<CycleDigest> {
    read_recent_where(workspace, n, |d| d.session == session)
}

/// Read up to `n` most-recent digests NOT stamped with `session` —
/// peer seats sharing the workspace. These must only ever render under
/// an explicit labeled peer section, never as first-person turns.
pub fn read_recent_peers(workspace: &Path, session: &str, n: usize) -> Vec<CycleDigest> {
    read_recent_where(workspace, n, |d| d.session != session)
}

fn read_recent_where(
    workspace: &Path,
    n: usize,
    pred: impl Fn(&CycleDigest) -> bool,
) -> Vec<CycleDigest> {
    let dir = crate::standalone::state_dir_for(workspace);
    let path = dir.join(DIGEST_FILE);
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let mut matched: Vec<CycleDigest> = reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<CycleDigest>(&l).ok())
        .filter(|d| pred(d))
        .collect();
    let start = matched.len().saturating_sub(n);
    matched.split_off(start)
}

/// Render last-K digests into a projection-ready section. Returns None
/// if there are no digests to show.
pub fn render_recent_section(digests: &[CycleDigest]) -> Option<String> {
    if digests.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("## Recent assistant turns (digest)\n\n");
    for (i, d) in digests.iter().enumerate() {
        let label = format!("t-{}", digests.len() - 1 - i);
        let outcome = d.outcome.as_deref().unwrap_or("?");
        let intent = d.intent.as_deref().unwrap_or("");
        let narrative = d.narrative.as_deref().unwrap_or("");
        let artifacts = if d.artifacts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", d.artifacts.join(", "))
        };
        if !narrative.is_empty() {
            out.push_str(&format!(
                "- **[{}]** [{}] {}{}\n",
                label, outcome, narrative, artifacts
            ));
        } else if !intent.is_empty() {
            out.push_str(&format!(
                "- **[{}]** [{}] {}{}\n",
                label, outcome, intent, artifacts
            ));
        } else {
            out.push_str(&format!("- **[{}]** [{}]{}\n", label, outcome, artifacts));
        }
    }
    out.push('\n');
    Some(out)
}

/// Render peer-seat digests into an explicitly labeled section. Peer
/// turns are tagged with a short session prefix and never use the
/// first-person `[t-K]` labels — the reader must not be able to
/// mistake a peer's turn for its own history (→1973).
pub fn render_peer_section(digests: &[CycleDigest]) -> Option<String> {
    if digests.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("## Peer activity (other seats, digest — NOT your turns)\n\n");
    for d in digests {
        let tag: String = d.session.chars().take(8).collect();
        let tag = if tag.is_empty() { "?".to_string() } else { tag };
        let outcome = d.outcome.as_deref().unwrap_or("?");
        let text = d.narrative.as_deref().or(d.intent.as_deref()).unwrap_or("");
        let artifacts = if d.artifacts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", d.artifacts.join(", "))
        };
        out.push_str(&format!(
            "- **[peer:{}]** [{}] {}{}\n",
            tag, outcome, text, artifacts
        ));
    }
    out.push('\n');
    Some(out)
}

fn chrono_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Cheap ISO-8601 without an extra dep — `chrono` would be nicer
    // but we want zero new deps for Layer 1.
    let secs_in_day = secs % 86400;
    let h = secs_in_day / 3600;
    let m = (secs_in_day % 3600) / 60;
    let s = secs_in_day % 60;
    let days_since_epoch = secs / 86400;
    let (y, mo, d) = days_to_ymd(days_since_epoch as i64);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
}

fn days_to_ymd(days: i64) -> (i32, u32, u32) {
    // Civil-from-days (Howard Hinnant's algorithm). Avoids a chrono dep.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Serializes the HOME-mutating tests against ALL other
    /// HOME-mutating tests in the crate (state_dir_for reads HOME at
    /// call time) — a module-local mutex would still race standalone's.
    use crate::standalone::TEST_HOME_LOCK as HOME_LOCK;

    #[test]
    fn parse_digest_roundtrip() {
        let body = r#"some response text
<turn-digest>{"intent":"review carve","outcome":"agreed","artifacts":["rebuild.rs"],"narrative":"walked through the standalone path"}</turn-digest>
trailing"#;
        let d = parse_digest(body).expect("digest must parse");
        assert_eq!(d.intent.as_deref(), Some("review carve"));
        assert_eq!(d.outcome.as_deref(), Some("agreed"));
        assert_eq!(d.artifacts, vec!["rebuild.rs"]);
        assert!(d.narrative.is_some());
    }

    #[test]
    fn parse_digest_takes_last_fence_when_multiple() {
        let body = r#"
<turn-digest>{"intent":"first try","outcome":"flagged"}</turn-digest>
<turn-digest>{"intent":"second try","outcome":"agreed"}</turn-digest>"#;
        let d = parse_digest(body).unwrap();
        assert_eq!(d.intent.as_deref(), Some("second try"));
        assert_eq!(d.outcome.as_deref(), Some("agreed"));
    }

    #[test]
    fn parse_digest_returns_none_when_absent() {
        let body = "no fence here";
        assert!(parse_digest(body).is_none());
    }

    #[test]
    fn parse_digest_returns_none_on_malformed_json() {
        let body = r#"<turn-digest>not json{}</turn-digest>"#;
        assert!(parse_digest(body).is_none());
    }

    #[test]
    fn parse_digest_extracts_from_sse_stream() {
        let body = "event: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"<turn-digest>\"}}\n\nevent: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"intent\\\":\\\"x\\\",\\\"outcome\\\":\\\"agreed\\\"}\"}}\n\nevent: content_block_delta\ndata: {\"delta\":{\"type\":\"text_delta\",\"text\":\"</turn-digest>\"}}\n\n";
        let d = parse_digest(body).expect("digest must parse from SSE");
        assert_eq!(d.intent.as_deref(), Some("x"));
        assert_eq!(d.outcome.as_deref(), Some("agreed"));
    }

    #[test]
    fn write_and_read_roundtrip() {
        // HOME_LOCK note: this test mutates HOME like the standalone
        // tests; the standalone module's mutex serializes those, and
        // cycle_digest writes go through ensure_state_dir which itself
        // reads HOME at call time. Test-isolation works because tempdir
        // gives us an exclusive HOME for the duration.
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let workspace = std::path::Path::new("/some/work_digest");
        let d = CycleDigest {
            ts: "2026-05-07T19:00:00Z".into(),
            session: "test-session".into(),
            intent: Some("ship layer 1".into()),
            outcome: Some("committed".into()),
            artifacts: vec!["cycle_digest.rs".into()],
            narrative: Some("wrote the harvester".into()),
        };
        write_digest(workspace, &d).unwrap();
        write_digest(workspace, &d).unwrap();
        let recent = read_recent(workspace, 5);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].intent.as_deref(), Some("ship layer 1"));
    }

    #[test]
    fn render_recent_section_empty_returns_none() {
        assert!(render_recent_section(&[]).is_none());
    }

    #[test]
    fn render_recent_section_renders_narratives_with_t_minus_labels() {
        let digests = vec![
            CycleDigest {
                ts: "2026-05-07T18:00:00Z".into(),
                session: "s".into(),
                intent: Some("first".into()),
                outcome: Some("agreed".into()),
                artifacts: vec!["a.rs".into()],
                narrative: Some("did the first thing".into()),
            },
            CycleDigest {
                ts: "2026-05-07T19:00:00Z".into(),
                session: "s".into(),
                intent: Some("second".into()),
                outcome: Some("committed".into()),
                artifacts: vec![],
                narrative: Some("did the second thing".into()),
            },
        ];
        let section = render_recent_section(&digests).unwrap();
        assert!(section.contains("Recent assistant turns"));
        assert!(section.contains("[t-1]"), "older digest gets t-1");
        assert!(section.contains("[t-0]"), "most-recent digest gets t-0");
        assert!(section.contains("did the first thing"));
        assert!(section.contains("did the second thing"));
        assert!(section.contains("[agreed]"));
        assert!(section.contains("[committed]"));
    }

    fn digest_for(session: &str, narrative: &str) -> CycleDigest {
        CycleDigest {
            ts: "2026-06-07T16:00:00Z".into(),
            session: session.into(),
            intent: None,
            outcome: Some("committed".into()),
            artifacts: vec![],
            narrative: Some(narrative.into()),
        }
    }

    #[test]
    fn read_recent_for_session_excludes_peer_rows() {
        // →1973 digest-bleed regression: the shared per-workspace file
        // interleaves seats; first-person reads must filter by session.
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let workspace = std::path::Path::new("/some/work_bleed");
        write_digest(workspace, &digest_for("seat-a", "a turn 1")).unwrap();
        write_digest(workspace, &digest_for("seat-b", "b turn 1")).unwrap();
        write_digest(workspace, &digest_for("seat-a", "a turn 2")).unwrap();
        write_digest(workspace, &digest_for("seat-b", "b turn 2")).unwrap();

        let own = read_recent_for_session(workspace, "seat-a", 5);
        assert_eq!(own.len(), 2);
        assert!(own.iter().all(|d| d.session == "seat-a"));
        assert_eq!(own[1].narrative.as_deref(), Some("a turn 2"));

        let peers = read_recent_peers(workspace, "seat-a", 5);
        assert_eq!(peers.len(), 2);
        assert!(peers.iter().all(|d| d.session == "seat-b"));
    }

    #[test]
    fn read_recent_for_session_takes_last_n_of_own_rows_only() {
        // The n-budget must apply AFTER the session filter — a burst of
        // peer writes must not evict this seat's own history.
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        let workspace = std::path::Path::new("/some/work_budget");
        write_digest(workspace, &digest_for("seat-a", "own old")).unwrap();
        for i in 0..10 {
            write_digest(workspace, &digest_for("seat-b", &format!("peer {i}"))).unwrap();
        }
        let own = read_recent_for_session(workspace, "seat-a", 5);
        assert_eq!(own.len(), 1, "own row survives 10 newer peer rows");
        assert_eq!(own[0].narrative.as_deref(), Some("own old"));
    }

    #[test]
    fn render_peer_section_labels_and_never_uses_t_minus() {
        let digests = vec![digest_for("0123456789abcdef", "peer did a thing")];
        let section = render_peer_section(&digests).unwrap();
        assert!(section.contains("Peer activity"));
        assert!(section.contains("NOT your turns"));
        assert!(section.contains("[peer:01234567]"), "8-char session tag");
        assert!(section.contains("peer did a thing"));
        assert!(
            !section.contains("[t-"),
            "peer turns must never carry first-person t-K labels"
        );
        assert!(render_peer_section(&[]).is_none());
    }
}
