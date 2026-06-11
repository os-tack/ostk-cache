//! Provider-policy backend — the cross-provider seam for the →2032
//! write-policy state machine (lane A1 of the cross-provider epic).
//!
//! # Contract
//!
//! The →2032 WARM/DEAD lane machine in [`crate::write_policy`] is
//! provider-neutral: it answers "warm or dead, what tier, what size"
//! in tokens. What *differs* per provider is the wire surface those
//! answers drive:
//!
//! - **Anthropic**: explicit `cache_control` breakpoints with a
//!   per-block `ttl` ("5m" | "1h"), a write premium (1.25× / 2.00×),
//!   and read discounts. The tier choice is priced and must be placed
//!   on the request markers.
//! - **GPT/OpenAI**: automatic prefix-match caching — *no breakpoints,
//!   no write premium* (per `docs/draft/codex-2034-review-20260610.md`
//!   §48-55, haystack). The levers are `prompt_cache_key` (routing
//!   affinity), retention (in-memory default vs 24h extended), prefix
//!   layout discipline, and `cached_tokens` observability.
//!
//! # Equivalence discipline (flag-dark guarantee)
//!
//! When the resolved provider is Anthropic — the default — every code
//! path through [`AnthropicPolicy`] is a pure delegation to the same
//! functions the proxy called before this trait existed:
//! [`write_policy::decide`] and [`TtlTier::wire_str`]. The tests at
//! the bottom hold the backend to the →2032 decision table: identical
//! `PolicyDecision` *and* identical post-call `LaneState` across the
//! reference vectors. Anthropic wire bytes cannot change.
//!
//! # GPT skeleton status
//!
//! [`GptPolicy`] reuses the same lane machine: automatic prefix
//! caching still rewards an append-only prefix and punishes mid-prefix
//! churn, so WARM/DEAD classification and DEAD-path re-projection
//! sizing carry over. The tier maps to *retention* instead of a
//! breakpoint TTL. Wire-level field paths (Responses API vs chat
//! completions) are pinned in A2 once codex capture data arrives —
//! nothing here emits GPT wire bytes yet.
//!
//! →1985 binds both backends identically: a `Dead` decision licenses a
//! faithful re-projection, never a truth-masking one, and usage-truth
//! passthrough is not this module's to modify.

use crate::config::Provider;
use crate::write_policy::{self, LaneState, PolicyDecision, TtlTier, WritePolicyParams};
use serde_json::Value;

/// Provider-normalized view of one response's usage block —
/// observational only (A2). Field semantics:
///
/// - `cached_tokens`: read-side cache hits. Anthropic
///   `cache_read_input_tokens`; GPT `input_tokens_details.cached_tokens`
///   (Responses shape, confirmed on the live chatgpt wire 2026-06-11)
///   or `prompt_tokens_details.cached_tokens` (chat-completions shape,
///   dormant until observed).
/// - `cache_write_tokens`: priced cache writes. Anthropic
///   `cache_creation_input_tokens`; always 0 on GPT — no write premium
///   exists (codex-2034-review §48-55).
/// - `input_tokens`: the provider's reported input-side count. NOTE:
///   Anthropic reports input EXCLUSIVE of cache reads/writes; GPT
///   reports input INCLUSIVE of cached_tokens. Consumers comparing
///   across providers must normalize (e.g. uncached input = Anthropic
///   `input_tokens` vs GPT `input_tokens - cached_tokens`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
}

/// Provider-specific cache-policy backend.
///
/// Object-safe; obtain a static instance via [`backend_for`].
pub trait PolicyBackend: Send + Sync {
    /// Stable label for telemetry / logs ("anthropic" | "gpt").
    fn name(&self) -> &'static str;

    /// Classify one request and advance lane state — the →2032 core
    /// decision. Backends may differ in *how the decision is used*,
    /// not in the lane-state discipline.
    fn decide(
        &self,
        lane: &mut LaneState,
        now_secs: u64,
        observed_prompt_tokens: u64,
        params: &WritePolicyParams,
    ) -> PolicyDecision;

    /// Wire value for an explicit per-block cache marker, if this
    /// provider takes one at all.
    ///
    /// Anthropic: `Some("5m" | "1h")` for `cache_control.ttl`.
    /// GPT: `None` — caching is automatic; there is no breakpoint to
    /// emit.
    fn tier_wire(&self, tier: TtlTier) -> Option<&'static str>;

    /// Wire value for a request-level retention lever, if this
    /// provider takes one.
    ///
    /// GPT: `Some("in_memory" | "24h")` (exact field path pinned in
    /// A2 against codex capture data). Anthropic: `None` — retention
    /// is the breakpoint TTL, already covered by [`Self::tier_wire`].
    fn retention_wire(&self, tier: TtlTier) -> Option<&'static str>;

    /// Cache-routing affinity key for providers with automatic
    /// caching.
    ///
    /// `client_key` is the `prompt_cache_key` already present on the
    /// inbound request, if any. Clients may key natively (codex sends
    /// its session UUID — observed on the live wire 2026-06-11); the
    /// client's key ALWAYS wins. Only when absent does the GPT backend
    /// synthesize one from the lane key. Anthropic: `None` — affinity
    /// is implicit in the prefix bytes.
    ///
    /// Computation only — emission is the caller's, and is gated off
    /// entirely in passthrough mode (capture lanes stay byte-clean).
    fn cache_key(&self, client_key: Option<&str>, session_id: &str, model: &str) -> Option<String>;

    /// Parse a provider usage block (the `usage` object, already
    /// located by the caller) into the normalized snapshot.
    /// Observational only — never feeds a mutation path. Returns
    /// `None` when the value doesn't carry this provider's input side.
    fn parse_usage(&self, usage: &Value) -> Option<UsageSnapshot>;
}

/// Anthropic backend: pure delegation, byte-identical to the
/// pre-trait proxy behavior. See the module docs for the equivalence
/// discipline.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicPolicy;

impl PolicyBackend for AnthropicPolicy {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn decide(
        &self,
        lane: &mut LaneState,
        now_secs: u64,
        observed_prompt_tokens: u64,
        params: &WritePolicyParams,
    ) -> PolicyDecision {
        write_policy::decide(lane, now_secs, observed_prompt_tokens, params)
    }

    fn tier_wire(&self, tier: TtlTier) -> Option<&'static str> {
        Some(tier.wire_str())
    }

    fn retention_wire(&self, _tier: TtlTier) -> Option<&'static str> {
        None
    }

    fn cache_key(
        &self,
        _client_key: Option<&str>,
        _session_id: &str,
        _model: &str,
    ) -> Option<String> {
        None
    }

    fn parse_usage(&self, usage: &Value) -> Option<UsageSnapshot> {
        // Anthropic Messages shape: input_tokens is required; cache
        // fields default to 0 (absent on uncached requests).
        let input_tokens = usage.get("input_tokens")?.as_u64()?;
        let get = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        Some(UsageSnapshot {
            input_tokens,
            cached_tokens: get("cache_read_input_tokens"),
            cache_write_tokens: get("cache_creation_input_tokens"),
            output_tokens: get("output_tokens"),
        })
    }
}

/// GPT/OpenAI backend skeleton (A2 lands the wire emission and usage
/// parsing; nothing routes here while the resolved provider is
/// Anthropic).
///
/// Lane-machine reuse rationale: OpenAI's automatic prefix caching
/// matches on byte-stable prefixes exactly as Anthropic's explicit
/// breakpoints do, so WARM (append-only delta) vs DEAD (re-projection
/// moment) classification — and the →1985-faithful compaction sizing —
/// transfer unchanged. What does NOT transfer is pricing: there is no
/// write premium, so the tier forecast governs *retention* selection
/// rather than a priced TTL marker.
#[derive(Debug, Clone, Copy, Default)]
pub struct GptPolicy;

impl PolicyBackend for GptPolicy {
    fn name(&self) -> &'static str {
        "gpt"
    }

    fn decide(
        &self,
        lane: &mut LaneState,
        now_secs: u64,
        observed_prompt_tokens: u64,
        params: &WritePolicyParams,
    ) -> PolicyDecision {
        write_policy::decide(lane, now_secs, observed_prompt_tokens, params)
    }

    fn tier_wire(&self, _tier: TtlTier) -> Option<&'static str> {
        // Automatic prefix caching — no breakpoints exist to mark.
        None
    }

    fn retention_wire(&self, tier: TtlTier) -> Option<&'static str> {
        // Mapping rationale: a 5m forecast (dense cadence or going
        // cold) is served by default in-memory retention; a 1h
        // forecast (sparse-but-alive cadence) wants the 24h extended
        // tier. Exact request field path is pinned in A2 against
        // codex empirical captures.
        Some(match tier {
            TtlTier::Ephemeral5m => "in_memory",
            TtlTier::Ephemeral1h => "24h",
        })
    }

    fn cache_key(&self, client_key: Option<&str>, session_id: &str, model: &str) -> Option<String> {
        // Client-set prompt_cache_key always wins (codex keys natively
        // by session UUID). Synthesize from the lane key only when the
        // client sent none.
        match client_key {
            Some(k) if !k.is_empty() => Some(k.to_string()),
            _ => Some(format!("ostk:{session_id}:{model}")),
        }
    }

    fn parse_usage(&self, usage: &Value) -> Option<UsageSnapshot> {
        // Responses-API shape first — confirmed identical on the
        // chatgpt backend wire (live capture 2026-06-11):
        //   input_tokens, input_tokens_details.cached_tokens,
        //   output_tokens, output_tokens_details.reasoning_tokens.
        if let Some(input_tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
            return Some(UsageSnapshot {
                input_tokens,
                cached_tokens: usage
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_write_tokens: 0, // no write premium on GPT
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            });
        }
        // Chat-completions shape (dormant — not yet observed on the
        // codex wire; kept for api.openai.com chat callers):
        //   prompt_tokens, prompt_tokens_details.cached_tokens.
        let input_tokens = usage.get("prompt_tokens")?.as_u64()?;
        Some(UsageSnapshot {
            input_tokens,
            cached_tokens: usage
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_write_tokens: 0,
            output_tokens: usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }
}

/// Resolve the backend for a configured provider.
pub fn backend_for(provider: Provider) -> &'static dyn PolicyBackend {
    match provider {
        Provider::Anthropic => &AnthropicPolicy,
        Provider::Gpt => &GptPolicy,
    }
}

// ---------------------------------------------------------------------------
// Equivalence tests — the A1 gate
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_policy::LaneClass;

    fn params() -> WritePolicyParams {
        WritePolicyParams::default()
    }

    /// Drive a reference request sequence through both the raw →2032
    /// function and a backend, asserting identical decisions AND
    /// identical lane state after every step.
    fn assert_equivalent_over(
        backend: &dyn PolicyBackend,
        seq: &[(u64, u64)], // (now_secs, observed_prompt_tokens)
    ) {
        let p = params();
        let mut raw_lane = LaneState::default();
        let mut backend_lane = LaneState::default();
        for (i, &(now, prompt)) in seq.iter().enumerate() {
            let raw = write_policy::decide(&mut raw_lane, now, prompt, &p);
            let via = backend.decide(&mut backend_lane, now, prompt, &p);
            assert_eq!(raw, via, "decision diverged at step {i}");
            assert_eq!(
                format!("{raw_lane:?}"),
                format!("{backend_lane:?}"),
                "lane state diverged at step {i}"
            );
        }
    }

    /// The →2032 decision-table vectors: first-request cold, dense
    /// WARM cadence, TTL-expiry DEAD, sparse-cadence tier upgrade,
    /// harness-compaction shrink mirror.
    fn reference_sequences() -> Vec<Vec<(u64, u64)>> {
        vec![
            // first request, cold compaction
            vec![(1_000_000, 100_000)],
            // dense cadence: warm deltas
            vec![
                (1_000_000, 100_000),
                (1_000_060, 105_000),
                (1_000_120, 110_000),
            ],
            // gap past TTL: dead, recompact
            vec![(1_000_000, 100_000), (1_000_600, 105_000)],
            // sparse cadence: tier upgrade to 1h then warm
            vec![
                (1_000_000, 100_000),
                (1_000_600, 101_000),
                (1_001_200, 102_000),
                (1_001_800, 103_000),
            ],
            // harness compaction shrink mirrored proportionally
            vec![(1_000_000, 100_000), (1_000_060, 50_000)],
            // AC-4 structural clamp
            vec![(1_000_000, 1_000_000)],
            // zero-prompt empty lane
            vec![(1_000_000, 0)],
        ]
    }

    #[test]
    fn anthropic_backend_is_byte_identical_to_write_policy() {
        for seq in reference_sequences() {
            assert_equivalent_over(&AnthropicPolicy, &seq);
        }
    }

    #[test]
    fn anthropic_tier_wire_matches_legacy_literals() {
        // These two strings ARE the pre-trait wire bytes; they must
        // never change under the Anthropic backend.
        assert_eq!(AnthropicPolicy.tier_wire(TtlTier::Ephemeral5m), Some("5m"));
        assert_eq!(AnthropicPolicy.tier_wire(TtlTier::Ephemeral1h), Some("1h"));
        assert_eq!(
            AnthropicPolicy.tier_wire(TtlTier::Ephemeral5m),
            Some(TtlTier::Ephemeral5m.wire_str())
        );
        assert_eq!(
            AnthropicPolicy.tier_wire(TtlTier::Ephemeral1h),
            Some(TtlTier::Ephemeral1h.wire_str())
        );
    }

    #[test]
    fn anthropic_has_no_gpt_levers() {
        assert_eq!(AnthropicPolicy.retention_wire(TtlTier::Ephemeral5m), None);
        assert_eq!(AnthropicPolicy.retention_wire(TtlTier::Ephemeral1h), None);
        assert_eq!(AnthropicPolicy.cache_key(None, "s", "m"), None);
        assert_eq!(
            AnthropicPolicy.cache_key(Some("client-key"), "s", "m"),
            None
        );
    }

    #[test]
    fn gpt_skeleton_reuses_lane_machine() {
        // Documented A1 choice: the GPT backend shares the →2032 lane
        // machine until A2 pins provider-specific params.
        for seq in reference_sequences() {
            assert_equivalent_over(&GptPolicy, &seq);
        }
    }

    #[test]
    fn gpt_emits_no_breakpoints_maps_retention() {
        assert_eq!(GptPolicy.tier_wire(TtlTier::Ephemeral5m), None);
        assert_eq!(GptPolicy.tier_wire(TtlTier::Ephemeral1h), None);
        assert_eq!(
            GptPolicy.retention_wire(TtlTier::Ephemeral5m),
            Some("in_memory")
        );
        assert_eq!(GptPolicy.retention_wire(TtlTier::Ephemeral1h), Some("24h"));
    }

    #[test]
    fn gpt_cache_key_prefers_client_key() {
        // Codex sends prompt_cache_key natively (= its session UUID,
        // observed live 2026-06-11) — the client's key must win.
        assert_eq!(
            GptPolicy.cache_key(
                Some("019eb8a0-d837-7ca3-bc9a-0ed5bb6ff73c"),
                "sess-1",
                "gpt-5.5"
            ),
            Some("019eb8a0-d837-7ca3-bc9a-0ed5bb6ff73c".to_string())
        );
        // Absent (or empty) client key → synthesize from the lane key.
        assert_eq!(
            GptPolicy.cache_key(None, "sess-1", "gpt-5.5"),
            Some("ostk:sess-1:gpt-5.5".to_string())
        );
        assert_eq!(
            GptPolicy.cache_key(Some(""), "sess-1", "gpt-5.5"),
            Some("ostk:sess-1:gpt-5.5".to_string())
        );
    }

    #[test]
    fn gpt_parses_responses_shape_from_live_wire_vector() {
        // VERBATIM usage block from the chatgpt backend wire — capture
        // 1781215373111-000001-e792b0924213, POST
        // /backend-api/codex/responses, SSE `response.completed`,
        // 2026-06-11.
        let usage = serde_json::json!({
            "input_tokens": 106206,
            "input_tokens_details": {"cached_tokens": 21376},
            "output_tokens": 594,
            "output_tokens_details": {"reasoning_tokens": 516},
            "total_tokens": 106800
        });
        let snap = GptPolicy.parse_usage(&usage).unwrap();
        assert_eq!(snap.input_tokens, 106206);
        assert_eq!(snap.cached_tokens, 21376);
        assert_eq!(snap.cache_write_tokens, 0, "GPT has no write premium");
        assert_eq!(snap.output_tokens, 594);
    }

    #[test]
    fn gpt_parses_chat_completions_shape_dormant() {
        let usage = serde_json::json!({
            "prompt_tokens": 1000,
            "prompt_tokens_details": {"cached_tokens": 400},
            "completion_tokens": 25,
            "total_tokens": 1025
        });
        let snap = GptPolicy.parse_usage(&usage).unwrap();
        assert_eq!(snap.input_tokens, 1000);
        assert_eq!(snap.cached_tokens, 400);
        assert_eq!(snap.output_tokens, 25);
    }

    #[test]
    fn gpt_usage_missing_details_defaults_zero_cached() {
        let usage = serde_json::json!({"input_tokens": 50, "output_tokens": 5});
        let snap = GptPolicy.parse_usage(&usage).unwrap();
        assert_eq!(snap.cached_tokens, 0);
        // No input side at all → None.
        assert_eq!(GptPolicy.parse_usage(&serde_json::json!({"x": 1})), None);
    }

    #[test]
    fn anthropic_parses_messages_usage_shape() {
        let usage = serde_json::json!({
            "input_tokens": 100,
            "cache_read_input_tokens": 900,
            "cache_creation_input_tokens": 300,
            "output_tokens": 42
        });
        let snap = AnthropicPolicy.parse_usage(&usage).unwrap();
        assert_eq!(snap.input_tokens, 100);
        assert_eq!(snap.cached_tokens, 900);
        assert_eq!(snap.cache_write_tokens, 300);
        assert_eq!(snap.output_tokens, 42);
        // Uncached request: cache fields absent → 0.
        let bare = serde_json::json!({"input_tokens": 10, "output_tokens": 1});
        let snap = AnthropicPolicy.parse_usage(&bare).unwrap();
        assert_eq!(snap.cached_tokens, 0);
        assert_eq!(snap.cache_write_tokens, 0);
    }

    #[test]
    fn backend_for_resolves_by_provider() {
        assert_eq!(backend_for(Provider::Anthropic).name(), "anthropic");
        assert_eq!(backend_for(Provider::Gpt).name(), "gpt");
    }

    #[test]
    fn first_request_classifies_dead_via_backend() {
        // Spot-check the delegation actually runs the real machine.
        let mut lane = LaneState::default();
        let d = backend_for(Provider::Anthropic).decide(&mut lane, 1_000_000, 100_000, &params());
        assert_eq!(d.class, LaneClass::Dead);
        assert_eq!(d.compact_target, Some(27_800));
    }
}
