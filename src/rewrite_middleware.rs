//! Rewrite middleware — hooks ostk-files-light into the proxy's
//! `/v1/messages` request path.
//!
//! # Architecture (→1798, →1799)
//!
//! This module is the proxy-side adoption of `ostk-files-light`'s
//! `rewrite_messages_inline_to_handles`. It serves as the wedge that
//! delivers cost savings to Claude Code (and any other Anthropic
//! Messages-API caller routing through this proxy):
//!
//! 1. Inbound `POST /v1/messages` body arrives as JSON bytes.
//! 2. We load the workspace's `.ostk/file_cache.jsonl` (containing
//!    `path → file_id` mappings with content SHA-256 digests).
//! 3. The rewriter walks every content block; any inline text whose
//!    SHA-256 matches a non-stale FileCache entry above the size
//!    threshold is swapped for a `{type:"document",source:{type:"file",
//!    file_id:"..."}}` reference.
//! 4. The rewritten body is forwarded upstream. Anthropic charges by
//!    input tokens; the document-handle wrapper is ~96 bytes vs.
//!    arbitrarily large inline content, so swaps yield real savings.
//!
//! # Pass-through fallback
//!
//! Every failure mode (cache load error, body not JSON, rewriter
//! anomaly) MUST fall back to forwarding the ORIGINAL body unchanged.
//! Breaking the proxy is far worse than missing a cache hit.
//!
//! # Telemetry
//!
//! One JSONL row per rewrite call is appended to
//! `<ostk_dir>/rewrite-events.jsonl`. The schema is documented in
//! [`RewriteEventRow`] and enforced by `tests/telemetry_schema.rs`.

use ostk_files_light::{
    FileCache, RewriteOptions, RewriteReport, rewrite_messages_inline_to_handles,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Default file name for the rewrite-events log inside `.ostk/`.
pub const REWRITE_EVENTS_FILENAME: &str = "rewrite-events.jsonl";

/// Configuration for one rewrite pass.
///
/// Constructed once per request from the proxy's app state plus
/// per-request workspace info. The `enabled` flag is the runtime gate
/// (env: `OSTK_REWRITE_ENABLED` — defaults ON; set to "0"/"false" to
/// disable). When disabled the rewriter is a no-op and emits no
/// telemetry.
#[derive(Debug, Clone)]
pub struct RewriteConfig {
    /// True if rewriting is enabled. False makes [`apply_rewrite`] a
    /// no-op that returns `None` without touching the body, the cache,
    /// or the telemetry log.
    pub enabled: bool,
    /// Directory containing `file_cache.jsonl`. Typically `.ostk/`
    /// resolved from `OSTK_DIR` or the current workspace.
    pub ostk_dir: PathBuf,
    /// Knobs forwarded to the rewriter (max_swaps, dry_run,
    /// min_size_threshold).
    pub options: RewriteOptions,
    /// Session ID for telemetry attribution. Empty string is allowed
    /// (the JSONL row will record an empty session).
    pub session_id: String,
}

impl RewriteConfig {
    /// Construct a config from environment variables and a workspace dir.
    ///
    /// Resolution order for `ostk_dir`:
    /// 1. `OSTK_DIR` env var, if set.
    /// 2. `<cwd>/.ostk` if it exists.
    /// 3. Otherwise an empty path (cache load will fall back to empty
    ///    cache; rewriter will report misses across the board).
    pub fn from_env(session_id: String) -> Self {
        let enabled = env_truthy_default_on("OSTK_REWRITE_ENABLED");
        let ostk_dir = resolve_ostk_dir();

        Self {
            enabled,
            ostk_dir,
            options: RewriteOptions::default(),
            session_id,
        }
    }

    /// Build a rewrite config from the proxy's resolved [`crate::config::Config`].
    ///
    /// Honours `cfg.rewrite_enabled` and `cfg.ostk_dir`; falls back to
    /// the same cwd-based `.ostk/` discovery as `from_env` when the
    /// resolved `ostk_dir` is `None`.
    pub fn from_resolved(cfg: &crate::config::Config, session_id: String) -> Self {
        let ostk_dir = cfg.ostk_dir.value.clone().unwrap_or_else(resolve_ostk_dir);
        Self {
            enabled: cfg.rewrite_enabled.value,
            ostk_dir,
            options: RewriteOptions::default(),
            session_id,
        }
    }
}

/// One JSONL row appended per rewrite pass.
///
/// Schema is locked: changes to field names or types are a breaking
/// change covered by `tests/telemetry_schema.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RewriteEventRow {
    /// ISO-8601 UTC timestamp (e.g. "2026-05-06T12:34:56Z").
    pub ts: String,
    /// Session ID copied from [`RewriteConfig::session_id`].
    pub session: String,
    pub rewrites_applied: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub hits: u32,
    pub misses: u32,
    pub skipped_stale: u32,
    pub skipped_below_threshold: u32,
    pub errors: u32,
    /// Rough byte-to-token estimate: `(bytes_in - bytes_out) / 4`.
    /// Saturates at 0 if `bytes_out > bytes_in` (theoretically
    /// impossible but defended against).
    pub tokens_saved_estimate: u64,
    // →2030(a) TTL forecast telemetry fields. Added with serde defaults so
    // old rows (missing these fields) still parse — they get the default
    // "keep1h"/"default"/None values which match pre-feature behavior.
    /// TTL forecast decision: "keep1h" | "demote5m" | "die_cold".
    #[serde(default = "default_ttl_decision")]
    pub ttl_decision: String,
    /// The observed gap (seconds) that drove the decision: the window
    /// minimum for `demote5m` (the gap that survived the veto), the
    /// median for an observed `keep1h`; None when the decision came
    /// from an identity hint or the default path.
    #[serde(default)]
    pub observed_gap_s: Option<u64>,
    /// Source of the cadence classification: "observed" | "identity_hint" | "default".
    #[serde(default = "default_cadence_source")]
    pub cadence_source: String,
}

fn default_ttl_decision() -> String {
    "keep1h".to_string()
}

fn default_cadence_source() -> String {
    "default".to_string()
}

/// Outcome of [`apply_rewrite`].
#[derive(Debug, Clone)]
pub enum RewriteOutcome {
    /// Rewriting was disabled — body unchanged, no telemetry emitted.
    Disabled,
    /// Cache load failed — body unchanged, telemetry NOT emitted (the
    /// failure is logged to stderr by the caller). The error message
    /// is preserved for caller inspection.
    CacheLoadFailed(String),
    /// Rewrite ran successfully. Note that `report.rewrites_applied`
    /// may be 0 (no swaps) — that's still a successful rewrite.
    Applied(RewriteReport),
}

/// Run the rewriter against `req` in place.
///
/// On any failure mode the original body is left UNCHANGED and the
/// call returns a non-`Applied` variant. Callers MUST forward the
/// (possibly unchanged) body upstream regardless of outcome.
///
/// This function does NOT panic. Even if the rewriter were to panic
/// internally (it shouldn't — it's pure data), we'd want the caller's
/// outer fallback to forward the original body. We don't `catch_unwind`
/// here because that would hide programmer errors during dev; the
/// rewriter's contract is "no panics."
pub fn apply_rewrite(req: &mut Value, config: &RewriteConfig) -> RewriteOutcome {
    apply_rewrite_with_ttl(req, config, None)
}

/// Like [`apply_rewrite`] but also records the →2030(a) TTL forecast in telemetry.
pub fn apply_rewrite_with_ttl(
    req: &mut Value,
    config: &RewriteConfig,
    forecast: Option<&crate::ttl_forecast::ForecastResult>,
) -> RewriteOutcome {
    if !config.enabled {
        return RewriteOutcome::Disabled;
    }

    // Cache load: this can fail for an empty/missing dir, but
    // FileCache::load is defensive (it returns an empty cache when no
    // log exists) and uses fs::read_to_string which doesn't error if
    // the file is missing. Defensive in case the directory itself is
    // unreadable.
    if !config.ostk_dir.exists() {
        return RewriteOutcome::CacheLoadFailed(format!(
            "ostk_dir does not exist: {}",
            config.ostk_dir.display()
        ));
    }
    let cache = FileCache::load(&config.ostk_dir);

    let report = rewrite_messages_inline_to_handles(req, &cache, &config.options);

    // Best-effort telemetry. If the log write fails (e.g. read-only fs)
    // we still report success — the rewrite already mutated the body.
    if let Err(e) = emit_telemetry_with_ttl(&report, config, forecast) {
        eprintln!(
            "[ostk-cache rewrite] telemetry write failed (continuing): {}",
            e
        );
    }

    RewriteOutcome::Applied(report)
}

/// Append one [`RewriteEventRow`] to `<ostk_dir>/rewrite-events.jsonl`.
///
/// Public so tests can call it directly without going through
/// `apply_rewrite`.
pub fn emit_telemetry(report: &RewriteReport, config: &RewriteConfig) -> std::io::Result<()> {
    emit_telemetry_with_ttl(report, config, None)
}

/// Like [`emit_telemetry`] but also records the →2030(a) TTL forecast.
pub fn emit_telemetry_with_ttl(
    report: &RewriteReport,
    config: &RewriteConfig,
    forecast: Option<&crate::ttl_forecast::ForecastResult>,
) -> std::io::Result<()> {
    let row = build_event_row_with_ttl(report, &config.session_id, forecast);
    let path = config.ostk_dir.join(REWRITE_EVENTS_FILENAME);
    write_event_row(&path, &row)
}

/// Build a [`RewriteEventRow`] from a [`RewriteReport`] and session id.
///
/// Public so tests can verify the schema deterministically without
/// touching disk.
pub fn build_event_row(report: &RewriteReport, session: &str) -> RewriteEventRow {
    build_event_row_with_ttl(report, session, None)
}

/// Build a [`RewriteEventRow`] including →2030(a) TTL forecast fields.
pub fn build_event_row_with_ttl(
    report: &RewriteReport,
    session: &str,
    forecast: Option<&crate::ttl_forecast::ForecastResult>,
) -> RewriteEventRow {
    let tokens_saved_estimate = report.bytes_in.saturating_sub(report.bytes_out) / 4;

    let (ttl_decision, observed_gap_s, cadence_source) = match forecast {
        Some(f) => (
            f.forecast.as_str().to_string(),
            f.observed_gap_s,
            f.source.as_str().to_string(),
        ),
        None => ("keep1h".to_string(), None, "default".to_string()),
    };

    RewriteEventRow {
        ts: iso8601_utc_now(),
        session: session.to_string(),
        rewrites_applied: report.rewrites_applied,
        bytes_in: report.bytes_in,
        bytes_out: report.bytes_out,
        hits: report.hits,
        misses: report.misses,
        skipped_stale: report.skipped_stale,
        skipped_below_threshold: report.skipped_below_threshold,
        errors: report.errors,
        tokens_saved_estimate,
        ttl_decision,
        observed_gap_s,
        cadence_source,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_event_row(path: &Path, row: &RewriteEventRow) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut line = serde_json::to_string(row)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    line.push('\n');
    file.write_all(line.as_bytes())
}

/// Env-var truthiness with a DEFAULT-ON semantics.
///
/// * Unset → true (the gate defaults to enabled — opt-out, not opt-in).
/// * "0", "false", "no" (case-insensitive) → false.
/// * Anything else (including "1", "true", "yes", garbage values) →
///   true. The asymmetry favors the on-state because misconfiguration
///   should not silently disable the optimization.
fn env_truthy_default_on(name: &str) -> bool {
    match std::env::var(name) {
        Err(_) => true,
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Resolve the `.ostk/` directory from environment + cwd. See
/// [`RewriteConfig::from_env`] for the order.
fn resolve_ostk_dir() -> PathBuf {
    if let Ok(v) = std::env::var("OSTK_DIR")
        && !v.is_empty()
    {
        return PathBuf::from(v);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    cwd.join(".ostk")
}

/// Format the current time as a compact ISO-8601 UTC string with
/// second precision, e.g. `"2026-05-06T12:34:56Z"`.
///
/// We avoid pulling in `chrono` for a single timestamp; the format is
/// straightforward and tests verify its shape.
///
/// →1781: re-used by `lib.rs` for the canonical Page::stored_at and
/// Page::last_accessed fields after the federation reconciliation
/// (was previously `SystemTime::now()` on the local Page type).
pub(crate) fn iso8601_utc_now() -> String {
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    iso8601_utc_from_secs(now)
}

/// Format a UNIX timestamp (seconds since epoch) as ISO-8601 UTC.
///
/// Public so tests can assert deterministic formatting without
/// stubbing out the system clock.
pub fn iso8601_utc_from_secs(secs: u64) -> String {
    // Days from epoch
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hour, minute, second
    )
}

/// Howard Hinnant's `civil_from_days` algorithm, returning a (year,
/// month, day) tuple from days-since-epoch (1970-01-01 = 0).
///
/// Reference: https://howardhinnant.github.io/date_algorithms.html
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn iso8601_format_known_vector() {
        // 2026-05-06T12:34:56Z = 1778070896 unix seconds
        let s = iso8601_utc_from_secs(1_778_070_896);
        assert_eq!(s, "2026-05-06T12:34:56Z");
        // Epoch
        assert_eq!(iso8601_utc_from_secs(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn build_event_row_computes_tokens_saved() {
        let report = RewriteReport {
            rewrites_applied: 1,
            bytes_in: 1000,
            bytes_out: 100,
            hits: 1,
            misses: 0,
            skipped_stale: 0,
            skipped_below_threshold: 0,
            errors: 0,
        };
        let row = build_event_row(&report, "sess-x");
        assert_eq!(row.session, "sess-x");
        assert_eq!(row.rewrites_applied, 1);
        assert_eq!(row.tokens_saved_estimate, 225); // 900 / 4
    }

    #[test]
    fn build_event_row_saturates_when_bytes_out_exceeds_in() {
        let report = RewriteReport {
            rewrites_applied: 0,
            bytes_in: 50,
            bytes_out: 200,
            ..Default::default()
        };
        let row = build_event_row(&report, "");
        assert_eq!(row.tokens_saved_estimate, 0);
    }

    #[test]
    fn config_disabled_makes_apply_a_noop() {
        let tmp = TempDir::new().unwrap();
        let config = RewriteConfig {
            enabled: false,
            ostk_dir: tmp.path().to_path_buf(),
            options: RewriteOptions::default(),
            session_id: "s".to_string(),
        };
        let mut req = json!({"messages": [{"role": "user", "content": "x"}]});
        let before = req.clone();
        let outcome = apply_rewrite(&mut req, &config);
        assert!(matches!(outcome, RewriteOutcome::Disabled));
        assert_eq!(req, before, "disabled rewrite must not mutate body");
        // No telemetry written.
        let log = tmp.path().join(REWRITE_EVENTS_FILENAME);
        assert!(!log.exists(), "disabled rewrite must not emit telemetry");
    }

    #[test]
    fn config_with_missing_dir_returns_cache_load_failed() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does-not-exist");
        let config = RewriteConfig {
            enabled: true,
            ostk_dir: nonexistent,
            options: RewriteOptions::default(),
            session_id: "s".to_string(),
        };
        let mut req = json!({"messages": []});
        let before = req.clone();
        let outcome = apply_rewrite(&mut req, &config);
        match outcome {
            RewriteOutcome::CacheLoadFailed(_) => {}
            other => panic!("expected CacheLoadFailed, got {:?}", other),
        }
        assert_eq!(req, before, "failed-cache rewrite must not mutate body");
    }

    #[test]
    fn emit_telemetry_writes_one_jsonl_row() {
        let tmp = TempDir::new().unwrap();
        let config = RewriteConfig {
            enabled: true,
            ostk_dir: tmp.path().to_path_buf(),
            options: RewriteOptions::default(),
            session_id: "sess-1".to_string(),
        };
        let report = RewriteReport {
            rewrites_applied: 2,
            bytes_in: 4096,
            bytes_out: 192,
            hits: 2,
            misses: 1,
            skipped_stale: 0,
            skipped_below_threshold: 0,
            errors: 0,
        };
        emit_telemetry(&report, &config).unwrap();

        let log = tmp.path().join(REWRITE_EVENTS_FILENAME);
        let content = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: RewriteEventRow = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.session, "sess-1");
        assert_eq!(parsed.rewrites_applied, 2);
        assert_eq!(parsed.bytes_in, 4096);
        assert_eq!(parsed.tokens_saved_estimate, 976); // (4096 - 192) / 4
        // ts shape
        assert!(parsed.ts.ends_with('Z'));
        assert!(parsed.ts.contains('T'));
    }

    #[test]
    fn env_truthy_default_on_semantics() {
        // We cannot easily mutate process env in a multi-threaded test
        // suite without races. Test the in-process helper indirectly by
        // verifying the truthiness rules through inputs we can control.
        // (The actual env path is exercised in the integration tests
        // that spawn the proxy as a subprocess.)
        //
        // This test asserts the function exists and behaves on the
        // common "unset" path. We use a name that's almost certainly
        // not set in the test env.
        let unset = env_truthy_default_on("OSTK_PROBABLY_NOT_SET_XYZ_42");
        assert!(unset, "unset env var must default ON");
    }
}
