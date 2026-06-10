//! →2032 cache-write policy — WARM/DEAD lane state machine.
//!
//! Proxy-side port of the policy validated offline by
//! `scripts/replay_2032.py` (haystack, commit 5e302d32) against the
//! 2026-06-10 boot-wave corpus, under the restated acceptance criteria
//! of `docs/draft/spec-2032-restated-ac-20260610.md` (approved p29952
//! seq 28, amendments folded @ c19ea43a):
//!
//! - **AC-1** mean cache-write tokens per logical call ≤ 4,800
//! - **AC-2** cache-write spend ≤ $60.00 per comparable wave-day window
//! - **AC-3** hit ratio ≥ 90%
//! - **AC-4** no single cold write > 200,000 tokens (structural here:
//!   the DEAD-path compaction target is clamped to `cold_cap`)
//!
//! # Policy
//!
//! Cache lanes are keyed `(session, model)` — model-scoped caches mean
//! a `/model` switch starts a cold lane (→2030 amplifier 2). Each
//! request is classified against the lane's *TTL in effect* (the tier
//! chosen at the previous write):
//!
//! - **WARM** (`gap < ttl`, committed prefix exists): the prefix is
//!   byte-stable append-only — read the cached prefix, write only the
//!   appended delta.
//! - **DEAD** (`gap >= ttl`, deterministic at request time): the full
//!   re-write is sunk cost, so re-projection is free — compact the
//!   prefix to `clamp(S·compact, min_prefix, cold_cap)` before the
//!   unavoidable write.
//!
//! **Write-time TTL forecast** (the only predictive corner): pick the
//! tier from observed wake cadence — median of the trailing 5 gaps
//! < 5m → `ephemeral_5m` (1.25× premium), 5m–60m → `ephemeral_1h`
//! (2×), > 60m → the seat is going cold, write 5m and let it die (the
//! DEAD path re-projects before the next write anyway). No lookahead;
//! an empty history defaults to 5m (cheapest tier until cadence is
//! established).
//!
//! # Composition with →2030(a)
//!
//! [`crate::ttl_forecast`] (Keep1h/Demote5m, observe-only) stays as
//! the telemetry attribution column; this module is the *active*
//! policy with its own per-lane cadence state. The two are computed
//! independently and never share state — the →1985 constraint
//! (usage-truth passthrough unmodified, harness auto-compact never
//! disabled, DEAD-path re-projection faithful) binds the *caller*: a
//! `Dead` decision licenses a faithful re-projection sized to
//! `compact_target`, never a lossy or truth-masking one.
//!
//! # Units
//!
//! Everything here is in **tokens**. The caller owns the bytes→tokens
//! estimate (`truth_bytes_per_token`) and the projection itself; this
//! module only carries lane state and answers "warm or dead, what
//! tier, what size".

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Tunable policy parameters. Reference configuration per the restated
/// spec; any change re-runs `replay_2032.py` against the Jun-10 corpus
/// and must keep all four ACs green before deploy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WritePolicyParams {
    /// DEAD-path compaction factor applied to the simulated prefix.
    pub compact: f64,
    /// Floor (tokens) for a compacted projection — below this the
    /// projection loses its working set and WARM deltas balloon.
    pub min_prefix: u64,
    /// Ceiling (tokens) for any single cold write (AC-4, structural).
    pub cold_cap: u64,
}

impl Default for WritePolicyParams {
    fn default() -> Self {
        Self {
            compact: 0.278,
            min_prefix: 25_000,
            cold_cap: 200_000,
        }
    }
}

/// TTL tier selected at write time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlTier {
    /// 5-minute ephemeral cache (1.25× input-rate write premium).
    Ephemeral5m,
    /// 1-hour ephemeral cache (2.00× input-rate write premium).
    Ephemeral1h,
}

impl TtlTier {
    /// Anthropic `cache_control.ttl` wire value.
    pub fn wire_str(self) -> &'static str {
        match self {
            TtlTier::Ephemeral5m => "5m",
            TtlTier::Ephemeral1h => "1h",
        }
    }

    /// TTL duration in seconds (the lifetime the next gap is judged
    /// against).
    pub fn as_secs(self) -> u64 {
        match self {
            TtlTier::Ephemeral5m => FIVE_MIN_S,
            TtlTier::Ephemeral1h => ONE_HOUR_S,
        }
    }
}

/// WARM/DEAD classification for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneClass {
    /// Cached prefix is alive: read it, write only the appended delta.
    Warm,
    /// Cached prefix is expired (or never existed): re-projection is
    /// free, compact before the unavoidable cold write.
    Dead,
}

impl LaneClass {
    /// Telemetry string label.
    pub fn as_str(self) -> &'static str {
        match self {
            LaneClass::Warm => "warm",
            LaneClass::Dead => "dead",
        }
    }
}

/// Per-lane policy state, keyed `(session, model)` by the caller.
#[derive(Debug, Clone, Default)]
pub struct LaneState {
    /// Simulated prefix size (tokens) under this policy.
    pub s: u64,
    /// Committed (cached) prefix size as of the last write.
    pub c: u64,
    /// Last *observed* total prompt size — growth is derived from its
    /// delta so the policy inherits real fleet behavior.
    pub p_obs: u64,
    /// TTL in effect (seconds) — the tier chosen at the previous
    /// write. First request of a lane starts at 5m (cheapest).
    pub ttl_s: u64,
    /// Unix epoch seconds of the lane's previous request.
    pub last_ts: Option<u64>,
    /// Ring of recent inter-request gaps (seconds), oldest first.
    pub gaps: Vec<u64>,
}

/// Maximum gap observations retained per lane. The forecast only reads
/// the trailing [`FORECAST_WINDOW`]; the extra headroom keeps the ring
/// stable across eviction.
const GAP_RING_CAP: usize = 8;
/// Trailing window the TTL forecast takes its median over.
const FORECAST_WINDOW: usize = 5;

const FIVE_MIN_S: u64 = 5 * 60;
const ONE_HOUR_S: u64 = 60 * 60;

/// One policy decision: everything the caller needs to act and to
/// emit telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyDecision {
    pub class: LaneClass,
    /// Tier for the cache_control marker on this request's write.
    pub tier: TtlTier,
    /// DEAD only: the faithful-projection size target (tokens),
    /// `clamp(S·compact, min_prefix, cold_cap)`. `None` when WARM —
    /// the projection must not change shape under a live prefix.
    pub compact_target: Option<u64>,
    /// Expected cache-read tokens for this request (the committed
    /// prefix when WARM, 0 when DEAD).
    pub expected_read: u64,
    /// Expected cache-write tokens (appended delta when WARM, the
    /// compacted projection when DEAD).
    pub expected_write: u64,
    /// The gap (seconds) this request was judged on. `None` on the
    /// first request of a lane.
    pub gap_s: Option<u64>,
}

// ---------------------------------------------------------------------------
// Core decision function
// ---------------------------------------------------------------------------

/// Classify one request and advance lane state.
///
/// `observed_prompt_tokens` is the caller's estimate of the total
/// prompt size (input + cache read + cache write) for this request —
/// the policy derives growth from its delta. `now_secs` is injected
/// for testability.
///
/// Mirrors `replay_2032.py::replay` exactly, including the ordering
/// subtlety: the request is classified against the TTL chosen at the
/// *previous* write; the tier forecast then runs on the updated gap
/// ring and governs the *next* gap.
pub fn decide(
    lane: &mut LaneState,
    now_secs: u64,
    observed_prompt_tokens: u64,
    params: &WritePolicyParams,
) -> PolicyDecision {
    if lane.ttl_s == 0 {
        lane.ttl_s = FIVE_MIN_S;
    }

    // --- 1. Derive growth from the observed prompt delta ----------------
    let p = observed_prompt_tokens;
    let first_request = lane.last_ts.is_none();
    let growth: u64 = if first_request {
        p
    } else if p >= lane.p_obs {
        p - lane.p_obs
    } else {
        // Observed prefix shrank (harness compaction) — that IS a
        // dead-boundary re-projection moment; mirror the shrink
        // proportionally rather than fight it (→2032 constraint).
        let scaled = (lane.s as f64 * p as f64 / lane.p_obs.max(1) as f64) as u64;
        lane.s = lane.s.min(scaled);
        lane.c = lane.c.min(lane.s);
        0
    };
    lane.p_obs = p;
    lane.s = lane.s.saturating_add(growth);

    // --- 2. Record the gap ----------------------------------------------
    let gap_s = lane.last_ts.map(|last| now_secs.saturating_sub(last));
    if let Some(g) = gap_s {
        if lane.gaps.len() >= GAP_RING_CAP {
            lane.gaps.remove(0);
        }
        lane.gaps.push(g);
    }

    // --- 3. WARM/DEAD classification against the TTL in effect ----------
    let warm = lane.c > 0 && matches!(gap_s, Some(g) if g < lane.ttl_s);
    let (class, expected_read, expected_write, compact_target) = if warm {
        (LaneClass::Warm, lane.c, lane.s - lane.c, None)
    } else {
        let target = if lane.s > 0 {
            ((lane.s as f64 * params.compact) as u64)
                .max(params.min_prefix)
                .min(params.cold_cap)
        } else {
            0
        };
        lane.s = target;
        (LaneClass::Dead, 0, target, Some(target))
    };

    // --- 4. Write-time TTL forecast governs the NEXT gap -----------------
    let tier = forecast_tier(&lane.gaps);
    lane.ttl_s = tier.as_secs();
    lane.c = lane.s;
    lane.last_ts = Some(now_secs);

    PolicyDecision {
        class,
        tier,
        compact_target,
        expected_read,
        expected_write,
        gap_s,
    }
}

/// Write-time TTL forecast from trailing cadence (median of the last
/// [`FORECAST_WINDOW`] gaps). No lookahead. Empty history → 5m.
fn forecast_tier(gaps: &[u64]) -> TtlTier {
    if gaps.is_empty() {
        return TtlTier::Ephemeral5m;
    }
    let start = gaps.len().saturating_sub(FORECAST_WINDOW);
    let mut tail: Vec<u64> = gaps[start..].to_vec();
    tail.sort_unstable();
    let med = tail[tail.len() / 2];
    if med < FIVE_MIN_S {
        TtlTier::Ephemeral5m
    } else if med <= ONE_HOUR_S {
        TtlTier::Ephemeral1h
    } else {
        // Going cold: let it die, don't pay the 2x premium — the DEAD
        // path re-projects before the next write anyway.
        TtlTier::Ephemeral5m
    }
}

// ---------------------------------------------------------------------------
// Tests — port of the replay_2032.py decision table
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> WritePolicyParams {
        WritePolicyParams::default()
    }

    #[test]
    fn reference_params_match_spec() {
        let p = WritePolicyParams::default();
        assert_eq!(p.compact, 0.278);
        assert_eq!(p.min_prefix, 25_000);
        assert_eq!(p.cold_cap, 200_000);
    }

    #[test]
    fn first_request_is_dead_and_compacted() {
        let mut lane = LaneState::default();
        let d = decide(&mut lane, 1_000_000, 100_000, &params());
        assert_eq!(d.class, LaneClass::Dead);
        assert_eq!(d.expected_read, 0);
        // 100_000 * 0.278 = 27_800 > min_prefix
        assert_eq!(d.compact_target, Some(27_800));
        assert_eq!(d.expected_write, 27_800);
        // No gaps yet → cheapest tier.
        assert_eq!(d.tier, TtlTier::Ephemeral5m);
        assert_eq!(d.gap_s, None);
    }

    #[test]
    fn compaction_floors_at_min_prefix() {
        let mut lane = LaneState::default();
        let d = decide(&mut lane, 1_000_000, 30_000, &params());
        // 30_000 * 0.278 = 8_340 < 25_000 floor
        assert_eq!(d.compact_target, Some(25_000));
    }

    #[test]
    fn compaction_clamps_at_cold_cap_ac4() {
        let mut lane = LaneState::default();
        // 1M-token prefix: 0.278x = 278_000 > 200_000 cap.
        let d = decide(&mut lane, 1_000_000, 1_000_000, &params());
        assert_eq!(d.compact_target, Some(200_000));
        assert!(d.expected_write <= 200_000, "AC-4 must be structural");
    }

    #[test]
    fn empty_lane_zero_prompt_stays_zero() {
        let mut lane = LaneState::default();
        let d = decide(&mut lane, 1_000_000, 0, &params());
        assert_eq!(d.compact_target, Some(0));
        assert_eq!(d.expected_write, 0);
    }

    #[test]
    fn warm_path_reads_prefix_writes_delta() {
        let mut lane = LaneState::default();
        decide(&mut lane, 1_000_000, 100_000, &params());
        // Second request 60s later (< 5m TTL in effect), grown by 5k.
        let d = decide(&mut lane, 1_000_060, 105_000, &params());
        assert_eq!(d.class, LaneClass::Warm);
        assert_eq!(d.expected_read, 27_800);
        assert_eq!(d.expected_write, 5_000);
        assert_eq!(d.compact_target, None);
        assert_eq!(d.gap_s, Some(60));
    }

    #[test]
    fn gap_past_ttl_is_dead_and_recompacts() {
        let mut lane = LaneState::default();
        decide(&mut lane, 1_000_000, 100_000, &params());
        // 10 minutes later, but TTL in effect is 5m → DEAD.
        let d = decide(&mut lane, 1_000_600, 105_000, &params());
        assert_eq!(d.class, LaneClass::Dead);
        assert_eq!(d.expected_read, 0);
        // S was 27_800 + 5_000 growth = 32_800; compacted: 0.278x =
        // 9_118 → floored to min_prefix.
        assert_eq!(d.compact_target, Some(25_000));
    }

    #[test]
    fn ttl_in_effect_upgrades_to_1h_after_sparse_cadence() {
        let mut lane = LaneState::default();
        let mut now = 1_000_000;
        decide(&mut lane, now, 100_000, &params());
        // Three requests at 10m gaps: each is DEAD under a 5m TTL until
        // the forecast (median 600s ∈ [5m, 60m]) flips the tier to 1h.
        now += 600;
        let d = decide(&mut lane, now, 101_000, &params());
        assert_eq!(d.class, LaneClass::Dead);
        assert_eq!(d.tier, TtlTier::Ephemeral1h, "median 10m → 1h tier");
        // Next 10m gap is now judged against a 1h TTL → WARM.
        now += 600;
        let d = decide(&mut lane, now, 102_000, &params());
        assert_eq!(d.class, LaneClass::Warm);
    }

    #[test]
    fn forecast_tier_table() {
        // <5m median → 5m
        assert_eq!(forecast_tier(&[60, 120, 180]), TtlTier::Ephemeral5m);
        // 5m–60m median → 1h
        assert_eq!(forecast_tier(&[600, 900, 1200]), TtlTier::Ephemeral1h);
        assert_eq!(forecast_tier(&[3600]), TtlTier::Ephemeral1h);
        // >60m median → going cold, 5m (let it die)
        assert_eq!(forecast_tier(&[4000, 5000, 6000]), TtlTier::Ephemeral5m);
        // empty → 5m
        assert_eq!(forecast_tier(&[]), TtlTier::Ephemeral5m);
        // median over the trailing window only: 5 recent fast gaps
        // outvote older slow ones.
        assert_eq!(
            forecast_tier(&[9000, 9000, 60, 60, 60, 60, 60]),
            TtlTier::Ephemeral5m
        );
    }

    #[test]
    fn harness_compaction_mirrors_shrink_proportionally() {
        let mut lane = LaneState::default();
        decide(&mut lane, 1_000_000, 100_000, &params());
        let s_before = lane.s;
        // Observed prompt halves (harness auto-compact fired upstream).
        let d = decide(&mut lane, 1_000_060, 50_000, &params());
        assert!(lane.s <= s_before / 2 + 1, "S must mirror the shrink");
        // Shrink contributes zero growth; the request itself still
        // classifies normally (WARM here: gap 60s < 5m TTL).
        assert_eq!(d.class, LaneClass::Warm);
        assert_eq!(d.expected_write, 0);
    }

    #[test]
    fn gap_ring_is_bounded() {
        let mut lane = LaneState::default();
        let mut now = 1_000_000;
        for _ in 0..20 {
            now += 60;
            decide(&mut lane, now, 10_000, &params());
        }
        assert!(lane.gaps.len() <= GAP_RING_CAP);
    }

    #[test]
    fn model_switch_is_callers_lane_key_not_state() {
        // Documents the contract: lanes are keyed (session, model) by
        // the caller; a fresh LaneState behaves like a cold lane.
        let mut opus_lane = LaneState::default();
        decide(&mut opus_lane, 1_000_000, 100_000, &params());
        let mut fable_lane = LaneState::default();
        let d = decide(&mut fable_lane, 1_000_030, 100_000, &params());
        assert_eq!(d.class, LaneClass::Dead, "new lane must start cold");
    }

    #[test]
    fn warm_dead_warm_cycle_keeps_reads_dominant() {
        // Sanity port of the replay's aggregate property: under a dense
        // cadence the steady state is WARM deltas, and hit ratio
        // (read / (read+write)) clears AC-3's 90% quickly.
        let mut lane = LaneState::default();
        let mut now = 1_000_000;
        let mut p = 100_000u64;
        let (mut read, mut write) = (0u64, 0u64);
        decide(&mut lane, now, p, &params());
        for _ in 0..50 {
            now += 60;
            p += 2_000;
            let d = decide(&mut lane, now, p, &params());
            read += d.expected_read;
            write += d.expected_write;
        }
        let hit = read as f64 / (read + write) as f64;
        assert!(
            hit > 0.90,
            "dense-cadence hit ratio {hit:.3} must clear 90%"
        );
    }
}
