//! →2030(a) proxy-side TTL cadence classifier — **telemetry only**.
//!
//! One choke point: [`forecast_ttl`] is called once per request (after
//! session-id extraction, before breakpoint emission). It reads
//! [`SeatCadence`] state accumulated in the proxy's per-session store
//! (same place AMP accumulation lives) and returns a [`TtlForecast`]
//! recorded in `RewriteEventRow` telemetry. **No request mutation is
//! driven by this decision.**
//!
//! # Why observe-only (condition-1 sizing, 2026-06-10)
//!
//! The original spec promoted 5m→1h markers; verification showed the
//! harness already emits `ephemeral` `1h` natively on direct-connect
//! seats (64M 1h vs 0 5m write tokens), so promotion's delta was ~0.
//! The pivoted policy (demote doomed 1h writes to 5m) was then sized on
//! the Jun 9/10 corpus via the →2032 replay: the demotion trigger fires
//! **zero** times (>60m gaps exist but are always isolated — the tail
//! veto correctly keeps 1h), and even a perfect oracle saves only ~2.9k
//! of 13.1M rel units (0.022%), because writes preceding a long idle gap
//! are small delta writes, not the big cold re-projections that follow
//! it. Per the review decision rule (p50553): savings ≈ 0 → ship the
//! classifier + telemetry as the attribution column for the post-→2023
//! re-measurement, and add no rewriting policy surface.
//!
//! # Policy (pivoted frame, §3.1 post-verification)
//!
//! The harness-native 1h marker is the status quo; the classifier
//! reports what a demotion policy *would* do:
//!
//! - every gap in the observed window **> 60m** (median > 60m AND no
//!   recent ≤ 60m gap — the tail veto, asymmetry flipped toward KEEP)
//!   → [`TtlForecast::Demote5m`]
//! - anything else → [`TtlForecast::Keep1h`]
//! - needs ≥ 2 gap observations; fewer → fall through to identity hint;
//!   no hint → [`TtlForecast::Keep1h`] (fail-open)
//!
//! [`TtlForecast::LetDieCold`] is a reserved stub for →2032 DEAD-stub
//! integration; the classifier never emits it.
//!
//! # Re-base (review condition, asymmetry flipped)
//!
//! There is no sticky state in the demotion direction: a single observed
//! gap ≤ 60m re-bases the decision to `Keep1h` structurally (the short
//! gap enters the ring and defeats the every-gap-long trigger). A
//! surviving 1h cache is worth far more than the write premium it costs.
//!
//! # Identity hint
//!
//! When there are < 2 observed gaps, the proxy scans
//! `<ostk_dir>/proc/*/identity.json` for a file whose `session` field
//! matches the request's session id, then reads its `wake` field. The
//! scan result is cached for 60 s to avoid hammering the proc directory
//! on every cold-start request. Stale and unparseable hints always
//! fail-open to `Keep1h`.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// TTL cadence classification for a single request (telemetry only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlForecast {
    /// Status quo: leave harness-native 1h markers alone. Default and
    /// fail-open decision.
    Keep1h,
    /// Every observed gap exceeds 60m — a 1h cache written now is
    /// expected to die unread; a demotion policy would write 5m instead.
    /// Observe-only: recorded in telemetry, never applied.
    Demote5m,
    /// Reserved for →2032 DEAD-stub integration (skip the cache write
    /// entirely). Never emitted by the classifier.
    LetDieCold,
}

impl TtlForecast {
    /// Telemetry string label for [`RewriteEventRow`].
    pub fn as_str(self) -> &'static str {
        match self {
            TtlForecast::Keep1h => "keep1h",
            TtlForecast::Demote5m => "demote5m",
            TtlForecast::LetDieCold => "die_cold",
        }
    }
}

/// Source of the forecast decision — used in telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadenceSource {
    /// ≥ 2 empirical inter-request gaps were available.
    Observed,
    /// Cold start: decision came from the identity.json `wake` hint.
    IdentityHint,
    /// Cold start with no usable hint — default `Keep1h`.
    Default,
}

impl CadenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CadenceSource::Observed => "observed",
            CadenceSource::IdentityHint => "identity_hint",
            CadenceSource::Default => "default",
        }
    }
}

/// Per-session cadence state kept in the proxy's per-session store.
///
/// Holds the last request timestamp and a small ring of recent
/// inter-request gaps (in seconds). The ring is bounded at
/// [`CADENCE_RING_CAP`] entries; older entries are overwritten.
#[derive(Debug, Clone, Default)]
pub struct SeatCadence {
    /// Unix epoch seconds of the most recent request from this session.
    pub last_request_ts: Option<u64>,
    /// Ring of recent gap observations (seconds between requests).
    /// Stored oldest-first; new entries appended, evicted from the front
    /// once the ring is full.
    pub gap_ring: Vec<u64>,
    /// Most recent forecast for this session. `None` = first request.
    /// Telemetry continuity only — carries no sticky semantics (re-base
    /// toward Keep1h is structural, see module doc).
    pub last_forecast: Option<TtlForecast>,
}

/// Maximum number of gap observations retained per session.
const CADENCE_RING_CAP: usize = 8;

/// Minimum gap observations needed before the empirical classifier fires.
const MIN_OBSERVATIONS: usize = 2;

const SIXTY_MIN_S: u64 = 60 * 60;

// ---------------------------------------------------------------------------
// Identity-hint cache
// ---------------------------------------------------------------------------

/// Scan result cached per-ostk-dir for 60 s.
struct HintCacheEntry {
    fetched_at: Instant,
    /// Map: session_id → TtlForecast (only Keep1h or Demote5m from hints).
    entries: std::collections::HashMap<String, TtlForecast>,
}

/// A coarse process-wide hint cache. One entry per ostk_dir path.
/// We use a Mutex<Vec<...>> rather than DashMap to avoid adding a dep;
/// the proc dir is small and scans are infrequent (60 s TTL).
pub struct IdentityHintCache {
    inner: Mutex<Vec<(PathBuf, HintCacheEntry)>>,
}

impl IdentityHintCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
        }
    }

    /// Look up the identity hint for `session_id`, scanning `ostk_dir/proc/*/identity.json`
    /// if the cache is stale (> 60 s old) or absent.
    pub fn lookup(&self, ostk_dir: &Path, session_id: &str) -> Option<TtlForecast> {
        let now = Instant::now();
        let stale_after = Duration::from_secs(60);

        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        // Find existing entry for this ostk_dir.
        let pos = guard.iter().position(|(p, _)| p == ostk_dir);

        let needs_refresh = match pos {
            None => true,
            Some(i) => now.duration_since(guard[i].1.fetched_at) > stale_after,
        };

        if needs_refresh {
            let entries = scan_identity_hints(ostk_dir);
            let entry = HintCacheEntry {
                fetched_at: now,
                entries,
            };
            match pos {
                None => guard.push((ostk_dir.to_path_buf(), entry)),
                Some(i) => guard[i].1 = entry,
            }
        }

        let i = guard.iter().position(|(p, _)| p == ostk_dir).unwrap_or(0);
        guard[i].1.entries.get(session_id).copied()
    }
}

impl Default for IdentityHintCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Scan `<ostk_dir>/proc/*/identity.json` and build a session→hint map.
/// Never errors: bad files are silently skipped.
fn scan_identity_hints(ostk_dir: &Path) -> std::collections::HashMap<String, TtlForecast> {
    let proc_dir = ostk_dir.join("proc");
    let mut map = std::collections::HashMap::new();

    let entries = match std::fs::read_dir(&proc_dir) {
        Ok(e) => e,
        Err(_) => return map,
    };

    for entry in entries.flatten() {
        let identity_path = entry.path().join("identity.json");
        if !identity_path.exists() {
            continue;
        }
        let text = match std::fs::read_to_string(&identity_path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let session = match v.get("session").and_then(|s| s.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let wake = v.get("wake").and_then(|w| w.as_str()).unwrap_or("");
        let hint = parse_wake_hint(wake);
        map.insert(session, hint);
    }
    map
}

/// Map a `wake` field string to a forecast hint (pivoted frame).
///
/// Everything fails open to `Keep1h`; the only hint that suggests a
/// demotion candidate is a *declared machine cadence longer than the
/// 1h TTL* — a cache written for such a seat dies unread by schedule.
/// - `cron(<t>)` where `<t>` > 60m → Demote5m (would-demote)
/// - `poll(*)`, `push(*)`, `cron(≤60m)`, absent, unparseable → Keep1h
///
/// Turn-driven (`poll`) and traffic-driven (`push`) wakes are
/// human/event paced — never a demotion signal on a hint alone; the
/// empirical classifier takes over after two observed gaps anyway.
fn parse_wake_hint(wake: &str) -> TtlForecast {
    let wake = wake.trim();

    let inner = if let Some(rest) = wake.strip_prefix("cron(") {
        rest.trim_end_matches(')')
    } else {
        return TtlForecast::Keep1h;
    };

    match parse_duration_str(inner) {
        Some(secs) if secs > SIXTY_MIN_S => TtlForecast::Demote5m,
        _ => TtlForecast::Keep1h,
    }
}

/// Parse simple duration strings: "30s", "4m", "1h", "90". Returns
/// `None` if the format is unrecognised.
fn parse_duration_str(s: &str) -> Option<u64> {
    if let Some(n) = s.strip_suffix('s') {
        n.trim().parse::<u64>().ok()
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim().parse::<u64>().ok().map(|v| v * 60)
    } else if let Some(n) = s.strip_suffix('h') {
        n.trim().parse::<u64>().ok().map(|v| v * 3600)
    } else {
        // bare number: interpret as seconds
        s.trim().parse::<u64>().ok()
    }
}

// ---------------------------------------------------------------------------
// Core decision function
// ---------------------------------------------------------------------------

/// Result of a [`forecast_ttl`] call, bundling the decision with
/// telemetry-relevant metadata.
#[derive(Debug, Clone)]
pub struct ForecastResult {
    pub forecast: TtlForecast,
    /// Gap statistic (seconds) that drove an empirical decision: the
    /// window minimum for `Demote5m` (the gap that survived the veto),
    /// the median otherwise. None for hint or default paths.
    pub observed_gap_s: Option<u64>,
    pub source: CadenceSource,
}

/// Classify the cadence for one request and advance cadence state.
/// Telemetry only — callers must not mutate the request based on this.
///
/// `now_secs` is the current Unix epoch timestamp in seconds (injected
/// for testability; callers use
/// `SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()`).
///
/// Mutates `cadence` in-place: records the new gap, appends it to the
/// ring, and updates `last_forecast`.
pub fn forecast_ttl(
    session_id: &str,
    now_secs: u64,
    cadence: &mut SeatCadence,
    hint_cache: Option<&IdentityHintCache>,
    ostk_dir: Option<&Path>,
) -> ForecastResult {
    // --- 1. Record the new gap (if this isn't the first request) --------
    let new_gap = cadence
        .last_request_ts
        .map(|last| now_secs.saturating_sub(last));
    cadence.last_request_ts = Some(now_secs);

    if let Some(gap) = new_gap {
        if cadence.gap_ring.len() >= CADENCE_RING_CAP {
            cadence.gap_ring.remove(0);
        }
        cadence.gap_ring.push(gap);
    }

    // --- 2. Try empirical classification ---------------------------------
    if cadence.gap_ring.len() >= MIN_OBSERVATIONS {
        let mut sorted = cadence.gap_ring.clone();
        sorted.sort_unstable();
        let median = sorted[sorted.len() / 2];
        let min_gap = sorted[0];

        // Pivoted §3.1: Demote5m only when the median exceeds 60m AND no
        // gap in the window is ≤ 60m (tail veto toward KEEP). A single
        // short gap is evidence the seat comes back inside the TTL —
        // keeping the 1h cache alive is worth far more than the write
        // premium, so the veto is absolute.
        let decision = if median > SIXTY_MIN_S && min_gap > SIXTY_MIN_S {
            TtlForecast::Demote5m
        } else {
            TtlForecast::Keep1h
        };
        cadence.last_forecast = Some(decision);

        // Report the gap that drove the decision for telemetry.
        let deciding_gap = if decision == TtlForecast::Demote5m {
            min_gap
        } else {
            median
        };

        return ForecastResult {
            forecast: decision,
            observed_gap_s: Some(deciding_gap),
            source: CadenceSource::Observed,
        };
    }

    // --- 3. Fall back to identity hint (cold start) ----------------------
    if let (Some(cache), Some(dir)) = (hint_cache, ostk_dir) {
        if let Some(hint) = cache.lookup(dir, session_id) {
            cadence.last_forecast = Some(hint);
            return ForecastResult {
                forecast: hint,
                observed_gap_s: None,
                source: CadenceSource::IdentityHint,
            };
        }
    }

    // --- 4. Default: Keep1h (fail-open) ----------------------------------
    cadence.last_forecast = Some(TtlForecast::Keep1h);
    ForecastResult {
        forecast: TtlForecast::Keep1h,
        observed_gap_s: None,
        source: CadenceSource::Default,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Helper: build a SeatCadence with a pre-populated gap ring.
    fn cadence_with_gaps(gaps: &[u64]) -> SeatCadence {
        SeatCadence {
            last_request_ts: Some(1_000_000),
            gap_ring: gaps.to_vec(),
            last_forecast: None,
        }
    }

    // -----------------------------------------------------------------------
    // AC1: forecast_ttl classification table (pivoted frame)
    // -----------------------------------------------------------------------

    #[test]
    fn ac1_gap_regimes_no_hint() {
        // dense cadence (every gap < 5m) → Keep1h (status quo)
        let mut c = cadence_with_gaps(&[60, 120, 180]);
        let r = forecast_ttl("s1", 1_000_100, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Keep1h);
        assert_eq!(r.source, CadenceSource::Observed);

        // sparse 10–20m cadence → still Keep1h (1h cache survives these)
        let mut c = cadence_with_gaps(&[600, 900, 1200]);
        let r = forecast_ttl("s1", 1_001_000, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Keep1h);
        assert_eq!(r.source, CadenceSource::Observed);

        // every gap > 60m → Demote5m (cache dies unread either way)
        let mut c = cadence_with_gaps(&[4000, 5000, 6000]);
        let r = forecast_ttl("s1", 1_004_500, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Demote5m);
        assert_eq!(r.source, CadenceSource::Observed);
    }

    #[test]
    fn ac1_tail_veto_single_short_gap_blocks_demotion() {
        // Mostly-absent seat with ONE sub-60m gap in the window: the
        // median is > 60m but the veto (no recent ≤60m gap) must hold —
        // a seat that came back once inside the TTL may again, and a
        // surviving 1h cache dwarfs the write premium.
        let mut c = cadence_with_gaps(&[4000, 5000, 1800, 6000]);
        let r = forecast_ttl("s1", 1_004_500, &mut c, None, None);
        assert_eq!(
            r.forecast,
            TtlForecast::Keep1h,
            "a single ≤60m gap must veto demotion"
        );
    }

    #[test]
    fn ac1_classifier_never_emits_letdiecold() {
        // LetDieCold is a reserved →2032 stub. Sweep representative rings.
        for ring in [
            &[30u64, 60][..],
            &[600, 1200, 2400][..],
            &[4000, 5000, 6000][..],
            &[10_000, 20_000, 30_000][..],
        ] {
            let mut c = cadence_with_gaps(ring);
            let r = forecast_ttl("s1", 1_050_000, &mut c, None, None);
            assert_ne!(r.forecast, TtlForecast::LetDieCold);
        }
    }

    #[test]
    fn ac1_cold_start_0_observations_no_hint_defaults_keep1h() {
        let mut c = SeatCadence::default();
        let r = forecast_ttl("no-session", 1_000_000, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Keep1h);
        assert_eq!(r.source, CadenceSource::Default);
        assert_eq!(r.observed_gap_s, None);
    }

    #[test]
    fn ac1_cold_start_1_observation_then_second_gap_classifies() {
        // One stored gap + the gap recorded by this call = 2 observations,
        // enough for the empirical classifier. Ring becomes [300, 1000]:
        // min 300s ≤ 60m → Keep1h via the veto.
        let mut c = SeatCadence {
            last_request_ts: Some(999_000),
            gap_ring: vec![300],
            last_forecast: None,
        };
        let r = forecast_ttl("s1", 1_000_000, &mut c, None, None);
        assert_eq!(r.source, CadenceSource::Observed);
        assert_eq!(r.forecast, TtlForecast::Keep1h);
    }

    // -----------------------------------------------------------------------
    // AC1: identity hints fail open to Keep1h
    // -----------------------------------------------------------------------

    #[test]
    fn ac1_hint_cron_under_60m_maps_to_keep1h() {
        assert_eq!(parse_wake_hint("cron(1m)"), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("cron(4m)"), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("cron(10m)"), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("cron(60m)"), TtlForecast::Keep1h);
    }

    #[test]
    fn ac1_hint_cron_over_60m_maps_to_demote5m() {
        assert_eq!(parse_wake_hint("cron(90m)"), TtlForecast::Demote5m);
        assert_eq!(parse_wake_hint("cron(2h)"), TtlForecast::Demote5m);
    }

    #[test]
    fn ac1_hint_push_and_poll_map_to_keep1h() {
        assert_eq!(parse_wake_hint("push(monitor)"), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("poll(30s)"), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("poll(10m)"), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("poll(turn)"), TtlForecast::Keep1h);
    }

    #[test]
    fn ac1_hint_absent_maps_to_keep1h() {
        assert_eq!(parse_wake_hint(""), TtlForecast::Keep1h);
        assert_eq!(parse_wake_hint("unknown_format"), TtlForecast::Keep1h);
    }

    // -----------------------------------------------------------------------
    // AC3: usage passthrough (→1985 contract)
    // -----------------------------------------------------------------------

    #[test]
    fn ac3_flat_field_equals_breakdown_sum() {
        // Fixture: a usage payload with both flat and 1h-breakdown fields,
        // as Anthropic returns after a 1h-TTL write.
        let usage_json = json!({
            "input_tokens": 100,
            "cache_read_input_tokens": 50,
            "cache_creation_input_tokens": 300,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 200,
                "ephemeral_1h_input_tokens": 100
            }
        });

        let flat = usage_json
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let breakdown_5m = usage_json
            .pointer("/cache_creation/ephemeral_5m_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let breakdown_1h = usage_json
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // Contract: flat == 5m + 1h breakdown sum.
        assert_eq!(
            flat,
            breakdown_5m + breakdown_1h,
            "flat cache_creation_input_tokens must equal 5m+1h breakdown sum"
        );

        // parse_usage reads ONLY the flat field — verify it does not error
        // or return different values when breakdown is present.
        use ostk_cache_core::usage::{UsageDialect, parse_usage};
        let full_response = json!({
            "usage": usage_json
        });
        let body = serde_json::to_vec(&full_response).unwrap();
        let parsed = parse_usage(UsageDialect::Anthropic, false, &body);
        assert!(parsed.is_some(), "parse_usage returned None");
        let pu = parsed.unwrap();
        assert_eq!(
            pu.cache_create_tokens, 300,
            "parse_usage must read the flat field"
        );

        // Breakdown is passed through unmodified in the parsed Value.
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let breakdown = v.pointer("/usage/cache_creation");
        assert!(
            breakdown.is_some(),
            "breakdown object must survive JSON round-trip"
        );
        let ephem_1h = breakdown
            .unwrap()
            .get("ephemeral_1h_input_tokens")
            .and_then(|x| x.as_u64());
        assert_eq!(ephem_1h, Some(100), "1h breakdown must be intact");
    }

    // -----------------------------------------------------------------------
    // AC4: re-base toward Keep1h (asymmetry-flipped; replaces sticky)
    // -----------------------------------------------------------------------

    #[test]
    fn ac4_demote_rebases_to_keep1h_on_first_short_gap() {
        // Classify Demote5m from an all-long ring.
        let mut c = cadence_with_gaps(&[4000, 5000, 6000]);
        let r = forecast_ttl("s1", 1_004_500, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Demote5m);

        // Next request lands 10 minutes later: the 600s gap enters the
        // ring and must immediately re-base the decision to Keep1h.
        let r = forecast_ttl("s1", 1_005_100, &mut c, None, None);
        assert_eq!(
            r.forecast,
            TtlForecast::Keep1h,
            "one short gap must re-base Demote5m → Keep1h"
        );
    }

    #[test]
    fn ac4_no_sticky_state_in_demotion_direction() {
        // A prior Keep1h decision must not prevent Demote5m once the
        // ring is genuinely all-long (state carries no veto memory).
        let mut c = cadence_with_gaps(&[60, 120]);
        let r = forecast_ttl("s1", 1_000_100, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Keep1h);

        // Seat goes mostly-absent: ring rolls over to all-long gaps.
        c.gap_ring = vec![4000, 5000, 6000, 7000, 8000, 9000, 10_000, 11_000];
        c.last_request_ts = Some(1_100_000);
        let r = forecast_ttl("s1", 1_107_000, &mut c, None, None);
        assert_eq!(r.forecast, TtlForecast::Demote5m);
    }
}
