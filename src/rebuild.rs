//! Request rebuild — Layer 1 of the kernel-projection rewrite (per
//! `docs/draft/ostk-cache-adapter.md` §4 and the plan at
//! `~/.claude/plans/yes-exactly-now-that-lazy-thimble.md`).
//!
//! Discards `messages[0..last_user_idx]` and replaces it with one
//! synthetic user message containing kernel-projection-shaped context
//! (envelope quadruplet + native tool activity summary + user intent
//! thread). The in-flight chain `messages[last_user_idx..]` is
//! preserved verbatim so `tool_use ↔ tool_result` pairs round-trip.
//!
//! # Standalone vs federated
//!
//! This module operates in **standalone mode** — context is synthesized
//! from data already in the request body. Federated mode (Layer 2) will
//! call into the kernel's `kernel.projection.full` IPC verb and use its
//! result instead of the local synthesis. This module's output shape is
//! the same in both modes; only the data source differs.
//!
//! # Failure mode
//!
//! Any failure leaves the request body UNCHANGED and returns a non-
//! `Applied` outcome. Callers MUST forward the (possibly unchanged)
//! body upstream regardless of outcome. Breaking the proxy is far
//! worse than missing a rebuild opportunity.

use serde_json::{Value, json};
// →1831: cache_control accounting lives in the ostk-abi membrane so the
// kernel and ostk-cache count and budget against the same numbers.
use ostk_abi::cache_control::{ANTHROPIC_CACHE_CONTROL_LIMIT, count_cache_control_fields};

/// Kernel orientation text — firmware-class operating discipline that
/// belongs in the system prompt tier (per →1830). Static across turns;
/// once Anthropic caches the system block it's effectively free.
///
/// Migrated out of `compose_synthetic_context` (where it was paid-for
/// per-turn inside the dynamic synthetic block) into the system tier
/// where it cache-hits at the natural firmware boundary.
pub const KERNEL_ORIENTATION: &str = "# ostk-cache kernel orientation\n\nYou are operating from a kernel-projected working set, not the full conversation history. Your messages array contains a synthesized projection of state at the cycle boundary plus the in-flight chain of the current cycle. Discipline below applies on every turn while this orientation is in your system prompt.\n\n## Trust the projection\n\nThe projection is your authoritative working state. Do not attempt to reconstitute the prior transcript wholesale. When you need a historical artifact back, reach for the **right primitive**:\n\n- **re-run** → live answers (file contents, process info, build status). State has moved on anyway; re-querying is cheaper than recovering a stale answer.\n- **`recall:<addr>`** → historical answers (decisions, prior reasoning). Use ONLY for specific addresses surfaced in the projection. Recall is not a transcript-walker; aggregating it across prior turns defeats the paging.\n- **handle** (`{file_id, gen, size}`) → content dedup for large outputs you might want back. (Future: tool_result `out:Nb` stubs will become typed handles per →1812/→1813.)\n\n## Asymmetry: shapes vs bodies\n\nTool results in the projection are **shapes-only for `[ok]`, body-inline for `[error]`** under a budget. Errors are signal-dense and small; you almost always want them without a re-fetch.\n\n## Two more disciplines\n\n- **Ask the user** for specifics the projection doesn't expose (cheaper and more honest than reconstructing from substrate).\n- **Commit important conclusions to substrate** (`ostk decide`, needle creation) when you want them to survive the next projection.\n\n## End every turn with a digest fence\n\nSo your own intent survives the next projection, emit (as the LAST thing in your response, after any markdown, before any trailing whitespace) exactly this shape:\n\n```\n<turn-digest>{\"intent\":\"what you were trying to accomplish\",\"outcome\":\"agreed|disagreed|committed|flagged|blocked|open\",\"artifacts\":[\"path/file.rs\",\"decision:foo_bar\",\"needle:1828\"],\"narrative\":\"1-2 sentences capturing what you did this turn\"}</turn-digest>\n```\n\nKeep it terse — ~150 tokens. The proxy harvests this fence and renders the last K into the next projection's `## Recent assistant turns` section. Without it, your turn becomes shape-only on the next cycle.\n\n---\n";

/// Marker the proxy uses to detect that orientation has already been
/// appended to a request's system block (idempotency / no-double-append).
const ORIENTATION_HEADER_MARKER: &str = "# ostk-cache kernel orientation";

// →1831: `ANTHROPIC_CACHE_CONTROL_LIMIT` and the per-request marker
// count now live in `ostk_abi::cache_control` (imported above) so the
// kernel and ostk-cache budget against a single source of truth.

/// Append the kernel orientation block to the request's system prompt.
///
/// Adds `cache_control` with the caller-chosen `ttl` ("5m" or "1h" —
/// status-quo callers pass "1h"; →2032 the write policy's tier when
/// active) IFF we have budget remaining under Anthropic's 4-block
/// limit. If we're already at the limit, the orientation text is still
/// appended (model still sees the discipline); it just doesn't get a
/// dedicated cache breakpoint and falls back to whatever prefix-cache
/// happens to land on its stable bytes.
///
/// Preserves claude-code's existing system structure (string or array
/// of blocks). Idempotent — if the orientation marker is already
/// present, no-op.
///
/// Returns true if a block was appended; false otherwise.
pub fn append_kernel_orientation_to_system(value: &mut Value, ttl: &str) -> bool {
    let with_cache_control =
        count_cache_control_fields(value) < ANTHROPIC_CACHE_CONTROL_LIMIT;
    let new_block = if with_cache_control {
        json!({
            "type": "text",
            "text": KERNEL_ORIENTATION,
            "cache_control": {"type": "ephemeral", "ttl": ttl}
        })
    } else {
        json!({
            "type": "text",
            "text": KERNEL_ORIENTATION
        })
    };

    match value.get_mut("system") {
        Some(Value::Array(arr)) => {
            // Idempotency: skip if any existing block already starts
            // with the orientation marker.
            let already = arr.iter().any(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| s.contains(ORIENTATION_HEADER_MARKER))
                    .unwrap_or(false)
            });
            if already {
                return false;
            }
            arr.push(new_block);
            true
        }
        Some(Value::String(s)) => {
            if s.contains(ORIENTATION_HEADER_MARKER) {
                return false;
            }
            let original = s.clone();
            value["system"] = json!([
                {"type": "text", "text": original},
                new_block
            ]);
            true
        }
        Some(_) => {
            // Unknown shape; conservatively skip rather than risk
            // shape-corruption of a non-string-non-array system field.
            false
        }
        None => {
            value["system"] = json!([new_block]);
            true
        }
    }
}

/// Configuration for the request-rebuild pass.
#[derive(Debug, Clone, Default)]
pub struct RebuildConfig {
    /// True if rebuild is enabled.
    pub enabled: bool,
    /// Mode tag written to `AmpRow.mode`. "rebuild_local" for Layer 1
    /// standalone synthesis; "rebuild_kernel" for Layer 2 federated.
    pub mode_tag: String,
    /// Cap on the number of native tool calls summarized into context
    /// (most recent N retained). Bounded to prevent runaway summaries.
    /// →1979 K4: rebudgeted 50→18 — resolved-[ok] shapes beyond the
    /// current epoch are recall/re-run territory; the deep window was
    /// unanimously low-value (→1974). Unresolved frontier items are
    /// EXEMPT from this cap (see [`summarize_unresolved`]).
    pub max_native_tool_summary: usize,
    /// Cap on the number of prior user messages retained in the user
    /// intent thread (most recent N).
    pub max_user_intent_thread: usize,
    /// Federated-mode override: if set, this string is used as the live
    /// envelope instead of extracting one from the request body. Set
    /// when ostk-cache successfully fetches a projection from the
    /// kernel daemon over IPC. None → standalone synthesis (extract
    /// most recent envelope from request body, fall through to
    /// zeroed placeholder).
    pub live_envelope_override: Option<String>,
    /// Transcript-tail summary (Layer 3 Pattern A): pre-rendered
    /// cross-session activity section to append to the synthetic
    /// context. Populated by main.rs when
    /// `OSTK_CACHE_TAIL_TRANSCRIPT=1`. None → tail disabled or no
    /// cross-session events found.
    pub transcript_tail_summary: Option<String>,
    /// Recent assistant turn digests (Layer 1, per `cycle_digest`):
    /// pre-rendered `## Recent assistant turns` section pulled from
    /// the standalone state's cycle_digests.jsonl. Populated by
    /// main.rs when prior digests exist. None → no digests yet (e.g.
    /// fresh state dir or assistant hasn't started emitting fences).
    pub recent_assistant_digests: Option<String>,
    /// Pre-rendered `## Recent activity templates` section (→1856
    /// P1.E v0.3.4 splice): summary of `kernel/templates` clusters
    /// over the request's history text. Populated by main.rs when
    /// federated mode successfully fetches templates from the kernel
    /// daemon. None → templates unavailable (standalone mode, kernel
    /// down, daemon predates the verb, or no clusters formed).
    pub templates_summary: Option<String>,
    /// Less-lossy prior-user-intent thread, sourced from the harness
    /// transcript JSONL (Layer 3 Pattern A). When present, replaces
    /// the in-process `extract_user_intent_thread` output — the
    /// transcript has the full user text, so we kill the 240-char
    /// chop. Capped at `max_user_intent_thread` (last N turns) by
    /// the caller. None → fall back to in-process slice (e.g.
    /// non-claude-code harness, missing projects dir).
    pub prior_user_turns_override: Option<Vec<String>>,
    /// →1985 X3: true inbound request body size in bytes. When set, the
    /// projected `[meminfo]` line gains a ` body:<N.N>MB/32MB` gauge so
    /// seats and the lead can watch the march toward the transport wall.
    pub body_bytes_in: Option<u64>,
    /// →2032: DEAD-path faithful-projection byte budget, converted by
    /// the caller from the write policy's `compact_target` tokens
    /// (`tokens × truth_bytes_per_token`). When the composed synthetic
    /// context exceeds it, optional substrate-recoverable sections are
    /// elided in fixed priority order (templates → transcript tail →
    /// assistant digests); the core sections (envelope, tool activity,
    /// frontier, user thread) are never elided. None → WARM turn or
    /// policy dark: no elision, status-quo composition.
    pub compact_target_bytes: Option<usize>,
}

impl RebuildConfig {
    /// Construct a config from environment variables.
    ///
    /// `OSTK_CACHE_REBUILD` accepts: "1" / "true" / "yes" → standalone
    /// (rebuild_local); "kernel" → federated (rebuild_kernel, reserved
    /// for Layer 2). Empty / unset / "0" → disabled.
    pub fn from_env() -> Self {
        let raw = std::env::var("OSTK_CACHE_REBUILD").unwrap_or_default();
        let trimmed = raw.trim().to_ascii_lowercase();
        let (enabled, mode_tag) = match trimmed.as_str() {
            "1" | "true" | "yes" => (true, "rebuild_local"),
            "kernel" => (true, "rebuild_kernel"),
            _ => (false, "rebuild_local"),
        };
        Self {
            enabled,
            mode_tag: mode_tag.to_string(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        }
    }

    /// Build a rebuild config from the proxy's resolved [`crate::config::Config`].
    ///
    /// Mirrors `from_env`'s shape but pulls enabled+mode_tag from the
    /// already-resolved `mode` field (which has CLI > env > toml > default
    /// precedence applied). Mode `passthrough` and `mutate` disable rebuild;
    /// `rebuild` / `rebuild-kernel` enable with the corresponding ledger tag.
    pub fn from_resolved(cfg: &crate::config::Config) -> Self {
        use crate::config::Mode;
        let (enabled, mode_tag) = match cfg.mode.value {
            Mode::Rebuild => (true, "rebuild_local"),
            Mode::RebuildKernel => (true, "rebuild_kernel"),
            Mode::Passthrough | Mode::Mutate => (false, "rebuild_local"),
        };
        Self {
            enabled,
            mode_tag: mode_tag.to_string(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        }
    }
}

/// Outcome of [`apply_rebuild`].
#[derive(Debug, Clone)]
pub enum RebuildOutcome {
    /// Rebuild was disabled; body unchanged.
    Disabled,
    /// Rebuild was enabled but skipped — e.g. malformed messages, no
    /// user message found, no prior history. Body unchanged.
    Skipped(String),
    /// Rebuild ran successfully. The request body was mutated in place.
    Applied(RebuildReport),
}

/// Statistics from a successful rebuild.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    /// Number of `messages[]` entries discarded (everything before the
    /// last user message).
    pub turns_dropped: usize,
    /// Approximate byte size of the discarded `messages[]` slice.
    pub bytes_in: usize,
    /// Byte size of the synthetic context message that replaced it.
    pub bytes_out: usize,
    /// True if a `[procs]/[loadavg]/[meminfo]/[ctx]` envelope quadruplet
    /// was found in the dropped slice and reused. False means we used
    /// the standalone synthetic placeholder (zeroed counters).
    pub envelope_found: bool,
    /// Number of native tool calls summarized into context.
    pub native_tool_calls_summarized: usize,
    /// Number of prior user messages retained in the user intent thread.
    pub user_messages_summarized: usize,
    /// →2032: optional sections elided to meet `compact_target_bytes`
    /// (0 = no budget set or synthetic already fit).
    pub sections_elided: usize,
}

/// Run the rebuild rewriter against `req` in place.
///
/// On any failure mode the original body is left UNCHANGED and the
/// call returns a non-`Applied` variant.
pub fn apply_rebuild(req: &mut Value, config: &RebuildConfig) -> RebuildOutcome {
    if !config.enabled {
        return RebuildOutcome::Disabled;
    }

    let messages = match req.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => return RebuildOutcome::Skipped("messages not array".into()),
    };

    // Find the cycle boundary: the most recent user message that ISN'T
    // entirely tool_result blocks. Anthropic's user role doubles as
    // (a) the real user message and (b) the tool_result wrapper sent
    // back by the harness mid-cycle. We want to cut at (a), preserving
    // the in-flight chain (a + assistant tool_use + b + assistant
    // tool_use + b + ... → end_turn) on the right side of the cut.
    let user_idx = match find_cycle_boundary_idx(messages) {
        Some(i) => i,
        None => return RebuildOutcome::Skipped("no real user message".into()),
    };

    if user_idx == 0 {
        return RebuildOutcome::Skipped("no prior history".into());
    }

    let dropped_slice = &messages[..user_idx];
    let bytes_in = serde_json::to_string(dropped_slice)
        .map(|s| s.len())
        .unwrap_or(0);

    // Federated mode: use the kernel-fetched envelope if provided.
    // Standalone mode: extract the most recent envelope from the
    // request body's tool_result text. Either may be None if neither
    // source produced one — render a zeroed placeholder downstream.
    let envelope = config
        .live_envelope_override
        .clone()
        .or_else(|| extract_latest_envelope(dropped_slice))
        // →1985 X3: stamp the true inbound body size onto the [meminfo]
        // line so the projection carries the transport-wall gauge.
        .map(|env| match config.body_bytes_in {
            Some(bytes) => crate::usage_truth::augment_meminfo_body(&env, bytes),
            None => env,
        });
    let native_summary = summarize_native_tools(dropped_slice, config.max_native_tool_summary);
    // →1979 K3: frontier scan runs over the FULL dropped slice,
    // deliberately uncapped — unresolved items are exempt from the
    // paging budget above.
    let unresolved = summarize_unresolved(dropped_slice);
    let user_thread = match &config.prior_user_turns_override {
        Some(turns) if !turns.is_empty() => {
            // Transcript-sourced thread: full text, no chop. Cap at the
            // last N turns to match in-process behavior.
            let cap = config.max_user_intent_thread;
            let start = turns.len().saturating_sub(cap);
            turns[start..].to_vec()
        }
        _ => extract_user_intent_thread(dropped_slice, config.max_user_intent_thread),
    };

    let synthetic = compose_synthetic_context(
        &envelope,
        &native_summary,
        &unresolved,
        &user_thread,
        config.transcript_tail_summary.as_deref(),
        config.recent_assistant_digests.as_deref(),
        config.templates_summary.as_deref(),
    );

    // →2032: DEAD-path compaction. A `compact_target_bytes` budget
    // licenses a faithful re-projection — elide optional sections in
    // fixed priority order (templates first: derived data; then
    // transcript tail; then assistant digests) until the synthetic
    // fits. All three are substrate-recoverable (recall / transcript
    // JSONL / cycle_digests.jsonl), satisfying the →1985 faithfulness
    // constraint. If the core projection alone still exceeds the
    // budget, it stands as-is — never elide envelope, tool activity,
    // frontier, or the user thread.
    let mut synthetic = synthetic;
    let mut sections_elided = 0usize;
    if let Some(budget) = config.compact_target_bytes {
        let mut templates = config.templates_summary.as_deref();
        let mut tail = config.transcript_tail_summary.as_deref();
        let mut digests = config.recent_assistant_digests.as_deref();
        while synthetic.len() > budget {
            if templates.is_some() {
                templates = None;
            } else if tail.is_some() {
                tail = None;
            } else if digests.is_some() {
                digests = None;
            } else {
                break;
            }
            sections_elided += 1;
            synthetic = compose_synthetic_context(
                &envelope,
                &native_summary,
                &unresolved,
                &user_thread,
                tail,
                digests,
                templates,
            );
        }
    }
    let bytes_out = synthetic.len();

    let in_flight: Vec<Value> = messages[user_idx..].to_vec();
    let mut new_messages = Vec::with_capacity(in_flight.len() + 1);
    // Synthetic context has DYNAMIC content (envelope, tool activity,
    // digests change every turn) so a cache_control marker here almost
    // never hits — its bytes shift turn-to-turn. Anthropic caps total
    // cache_control markers at 4 per request and claude-code already
    // uses several (system, tools, user msg). The synthetic's marker
    // was low-value AND consumed our budget. Drop it; let the
    // orientation block in system tier carry our cache breakpoint
    // (firmware-class, byte-stable, actually hits cache).
    new_messages.push(json!({
        "role": "user",
        "content": [{
            "type": "text",
            "text": synthetic
        }]
    }));
    new_messages.extend(in_flight);

    let turns_dropped = user_idx;
    *messages = new_messages;

    // Defensive repair: strip orphaned tool_result blocks from the
    // in-flight chain. claude-code's interrupt artifacts can leave
    // tool_results whose paired tool_use is in the dropped region (or
    // was never sent at all). Anthropic 400s on these.
    let orphans = repair_orphaned_tool_results(messages);
    if orphans > 0 {
        eprintln!("[proxy] rebuild: stripped {} orphaned tool_result blocks", orphans);
    }

    RebuildOutcome::Applied(RebuildReport {
        turns_dropped,
        bytes_in,
        bytes_out,
        envelope_found: envelope.is_some(),
        native_tool_calls_summarized: native_summary.len(),
        user_messages_summarized: user_thread.len(),
        sections_elided,
    })
}

/// Repair pass: strip `tool_result` blocks whose paired `tool_use`
/// is not in the immediately preceding message.
///
/// Anthropic enforces: "Each tool_result block must have a corresponding
/// tool_use block in the previous message." When claude-code is
/// interrupted mid-tool-call (user cancels), its reconstructed history
/// can leave orphaned tool_result blocks — most visibly at messages[0]
/// where there is NO previous message and therefore NO valid pairing.
///
/// This function walks every user message, drops orphaned tool_result
/// blocks, and removes any user message whose content array becomes
/// empty after stripping. Returns the number of orphaned blocks
/// removed (for telemetry).
///
/// Idempotent. Pure data transform. Safe to run on any messages array,
/// including byte-passthrough modes.
/// Outcome of [`enforce_soft_cap`]. Captures which reduction tiers fired
/// and how many bytes each recovered.
///
/// Tier A — ejected tool_result content above the per-result threshold.
/// Tier B — pruned oldest in-flight tool_use/tool_result pairs as a unit.
/// Tier C — dropped tool definitions not referenced by any in-flight `tool_use`.
/// Tier D — exhausted: caller should return 413 with the reduction log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReductionReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Number of `tool_result` content bodies ejected in Tier A.
    pub tier_a_ejected: usize,
    /// Total bytes recovered by Tier A ejections.
    pub tier_a_bytes_recovered: u64,
    /// Number of in-flight pair tuples (tool_use + tool_result) pruned in Tier B.
    pub tier_b_pairs_pruned: usize,
    /// Number of unreferenced tool definitions dropped in Tier C.
    pub tier_c_tools_dropped: usize,
    /// True when the cap could not be reached. Caller should 413.
    pub irreducible: bool,
}

impl ReductionReport {
    pub fn applied_any(&self) -> bool {
        self.tier_a_ejected > 0
            || self.tier_b_pairs_pruned > 0
            || self.tier_c_tools_dropped > 0
            || self.irreducible
    }
}

/// Ejection threshold: only `tool_result` content bodies above this size
/// are candidates for Tier A. Smaller results stay inline so the model
/// doesn't lose useful context on small payloads.
pub const TIER_A_EJECTION_THRESHOLD: usize = 100 * 1024;

/// Progressive reduction pipeline: trim a request body until it fits
/// under `soft_cap_bytes`. Runs four tiers (see [`ReductionReport`]) in
/// order, stopping as soon as the body is under the cap.
///
/// The function MUST preserve Anthropic's `tool_use` → `tool_result`
/// pairing invariant on the in-flight chain. Tier A replaces content
/// bytes but keeps the `tool_use_id` link. Tier B removes pair tuples
/// together so neither side is orphaned. Tier C only drops `tools[]`
/// entries with names not referenced by any in-flight `tool_use` block.
///
/// When `soft_cap_bytes == 0` the cap is disabled and this function
/// returns a no-op report immediately.
pub fn enforce_soft_cap(value: &mut Value, soft_cap_bytes: u64) -> ReductionReport {
    let mut report = ReductionReport::default();
    report.bytes_before = current_body_size(value);
    report.bytes_after = report.bytes_before;
    if soft_cap_bytes == 0 || report.bytes_before <= soft_cap_bytes {
        return report;
    }

    // ── Tier A — eject large tool_result content bodies ────────────────
    eject_large_tool_results(value, soft_cap_bytes, &mut report);
    report.bytes_after = current_body_size(value);
    if report.bytes_after <= soft_cap_bytes {
        return report;
    }

    // ── Tier B — prune oldest in-flight tool_use/tool_result pairs ─────
    prune_oldest_inflight_pairs(value, soft_cap_bytes, &mut report);
    report.bytes_after = current_body_size(value);
    if report.bytes_after <= soft_cap_bytes {
        return report;
    }

    // ── Tier C — drop unreferenced tool definitions ────────────────────
    drop_unreferenced_tools(value, &mut report);
    report.bytes_after = current_body_size(value);
    if report.bytes_after <= soft_cap_bytes {
        return report;
    }

    // ── Tier D — surrender. Caller MUST 413. ───────────────────────────
    report.irreducible = true;
    report
}

fn current_body_size(value: &Value) -> u64 {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0) as u64
}

fn eject_large_tool_results(value: &mut Value, soft_cap_bytes: u64, report: &mut ReductionReport) {
    // Collect (msg_idx, block_idx, size) for every tool_result content
    // body above the ejection threshold, sorted descending by size.
    let mut candidates: Vec<(usize, usize, usize)> = Vec::new();
    if let Some(msgs) = value.get("messages").and_then(|m| m.as_array()) {
        for (mi, msg) in msgs.iter().enumerate() {
            let Some(arr) = msg.get("content").and_then(|c| c.as_array()) else {
                continue;
            };
            for (bi, block) in arr.iter().enumerate() {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                let content_size = block
                    .get("content")
                    .map(|c| serde_json::to_string(c).map(|s| s.len()).unwrap_or(0))
                    .unwrap_or(0);
                if content_size >= TIER_A_EJECTION_THRESHOLD {
                    candidates.push((mi, bi, content_size));
                }
            }
        }
    }
    candidates.sort_by_key(|c| std::cmp::Reverse(c.2));

    for (mi, bi, original_size) in candidates {
        if current_body_size(value) <= soft_cap_bytes {
            break;
        }
        let stub_text = format!(
            "[ejected by ostk-cache soft-cap: {} → 60b. Re-run the call if needed.]",
            crate::fmt_bytes(original_size as u64)
        );
        let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            break;
        };
        let Some(msg) = messages.get_mut(mi) else { break };
        let Some(arr) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let Some(block) = arr.get_mut(bi) else { continue };
        let Some(obj) = block.as_object_mut() else { continue };
        obj.insert(
            "content".to_string(),
            serde_json::json!([{"type": "text", "text": stub_text}]),
        );
        report.tier_a_ejected += 1;
        report.tier_a_bytes_recovered += original_size as u64;
    }
}

fn prune_oldest_inflight_pairs(
    value: &mut Value,
    soft_cap_bytes: u64,
    report: &mut ReductionReport,
) {
    // Iterate from the second assistant message forward, dropping
    // (assistant tool_use msg, paired user tool_result msg) tuples
    // together. Keep the last pair so the model has at least one
    // round-trip of in-flight context.
    loop {
        if current_body_size(value) <= soft_cap_bytes {
            break;
        }
        let Some(messages) = value.get_mut("messages").and_then(|m| m.as_array_mut()) else {
            break;
        };

        // Find the first assistant-tool-use → user-tool-result pair.
        // Skip the first message (synthetic / real user message) and
        // skip the LAST pair to retain context for the model.
        let mut prune_idx: Option<usize> = None;
        let len = messages.len();
        for i in 1..len.saturating_sub(2) {
            let is_assistant_tool_use = messages
                .get(i)
                .and_then(|m| m.get("role").and_then(|r| r.as_str()))
                == Some("assistant")
                && message_contains_tool_use(&messages[i]);
            let is_user_tool_result = messages
                .get(i + 1)
                .and_then(|m| m.get("role").and_then(|r| r.as_str()))
                == Some("user")
                && message_only_tool_results(&messages[i + 1]);
            if is_assistant_tool_use && is_user_tool_result {
                prune_idx = Some(i);
                break;
            }
        }
        let Some(i) = prune_idx else { break };
        messages.drain(i..=i + 1);
        report.tier_b_pairs_pruned += 1;
    }
}

fn message_contains_tool_use(msg: &Value) -> bool {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
        })
        .unwrap_or(false)
}

fn message_only_tool_results(msg: &Value) -> bool {
    match msg.get("content") {
        Some(Value::Array(arr)) if !arr.is_empty() => arr.iter().all(|b| {
            b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
        }),
        _ => false,
    }
}

fn drop_unreferenced_tools(value: &mut Value, report: &mut ReductionReport) {
    // Collect tool names actually invoked by any in-flight assistant
    // `tool_use` block. Conservative: never drop tools that appear in
    // active calls. If `tools` is absent or not an array, no-op.
    let referenced: std::collections::HashSet<String> = value
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            let mut set = std::collections::HashSet::new();
            for msg in msgs {
                let Some(arr) = msg.get("content").and_then(|c| c.as_array()) else {
                    continue;
                };
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        && let Some(name) = block.get("name").and_then(|n| n.as_str())
                    {
                        set.insert(name.to_string());
                    }
                }
            }
            set
        })
        .unwrap_or_default();

    let Some(tools) = value.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return;
    };
    let before = tools.len();
    tools.retain(|t| {
        t.get("name")
            .and_then(|n| n.as_str())
            .map(|n| referenced.contains(n))
            .unwrap_or(true) // unnamed tools (shouldn't happen) → keep
    });
    let after = tools.len();
    report.tier_c_tools_dropped = before.saturating_sub(after);
}

/// Per-section byte sizes of a `/v1/messages` request body. Returned by
/// [`section_sizes`] for the per-turn telemetry line and for soft-cap
/// diagnostics.
///
/// `synthetic` is the size of the first user message in the messages
/// array (which, post-rebuild, holds the synthesized kernel-projection
/// context). When rebuild didn't apply this turn, `synthetic` is 0 and
/// its bytes are folded into `in_flight`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SectionSizes {
    pub system: u64,
    pub tools: u64,
    pub synthetic: u64,
    pub in_flight: u64,
}

impl SectionSizes {
    pub fn total(&self) -> u64 {
        self.system + self.tools + self.synthetic + self.in_flight
    }

    /// Largest section name + size — used by the soft-cap 413 hint
    /// and the indented breakdown when one section dominates.
    pub fn dominant(&self) -> (&'static str, u64) {
        let mut max = ("system", self.system);
        for (n, v) in [
            ("tools", self.tools),
            ("synthetic", self.synthetic),
            ("in_flight", self.in_flight),
        ] {
            if v > max.1 {
                max = (n, v);
            }
        }
        max
    }
}

/// Compute byte sizes of the four major sections of an Anthropic
/// `/v1/messages` request body.
///
/// `synthetic_present` should be true iff rebuild applied this turn
/// (the first message in `messages[]` is the synthesized context).
/// When false, the first message is a normal user turn and contributes
/// to `in_flight`.
///
/// Implementation: re-serializes each section to JSON to get its
/// wire-level size. This matches what gets sent upstream. The proxy
/// pays this serialization cost once per turn, on a value tree it
/// already parsed — net negligible.
pub fn section_sizes(value: &Value, synthetic_present: bool) -> SectionSizes {
    let system = value
        .get("system")
        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        .unwrap_or(0) as u64;
    let tools = value
        .get("tools")
        .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        .unwrap_or(0) as u64;
    let (synthetic, in_flight) = match value.get("messages").and_then(|m| m.as_array()) {
        Some(msgs) if !msgs.is_empty() && synthetic_present => {
            let syn = serde_json::to_string(&msgs[0])
                .map(|s| s.len())
                .unwrap_or(0) as u64;
            let rest: u64 = msgs[1..]
                .iter()
                .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0) as u64)
                .sum();
            (syn, rest)
        }
        Some(msgs) => {
            let rest: u64 = msgs
                .iter()
                .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0) as u64)
                .sum();
            (0u64, rest)
        }
        None => (0, 0),
    };
    SectionSizes {
        system,
        tools,
        synthetic,
        in_flight,
    }
}

pub fn repair_orphaned_tool_results(messages: &mut Vec<Value>) -> usize {
    let mut stripped: usize = 0;

    // Build a per-position view of valid `tool_use_id`s available to a
    // user message at index i: the tool_use IDs in messages[i-1] (if
    // it's an assistant message). messages[0] has no previous, so any
    // tool_result there is unconditionally orphaned.
    let valid_ids_for: Vec<std::collections::HashSet<String>> = (0..messages.len())
        .map(|i| {
            if i == 0 {
                std::collections::HashSet::new()
            } else {
                collect_tool_use_ids(&messages[i - 1])
            }
        })
        .collect();

    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let valid = &valid_ids_for[i];
        let arr = match msg.get_mut("content").and_then(|c| c.as_array_mut()) {
            Some(a) => a,
            None => continue,
        };
        let before = arr.len();
        arr.retain(|block| {
            let is_tool_result =
                block.get("type").and_then(|t| t.as_str()) == Some("tool_result");
            if !is_tool_result {
                return true;
            }
            match block.get("tool_use_id").and_then(|v| v.as_str()) {
                Some(id) => valid.contains(id),
                None => false,
            }
        });
        stripped += before - arr.len();
    }

    // Drop user messages that are now empty (all-orphan content).
    messages.retain(|msg| {
        if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
            match msg.get("content") {
                Some(Value::Array(a)) => !a.is_empty(),
                _ => true,
            }
        } else {
            true
        }
    });

    stripped
}

fn collect_tool_use_ids(msg: &Value) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && let Some(id) = block.get("id").and_then(|v| v.as_str())
            {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

/// Find the index of the most recent user message that is NOT entirely
/// composed of `tool_result` blocks. This is the cycle boundary: the
/// user message that started the current reasoning cycle (as opposed
/// to a mid-cycle tool_result wrapper).
fn find_cycle_boundary_idx(messages: &[Value]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| {
            m.get("role").and_then(|r| r.as_str()) == Some("user") && is_real_user_message(m)
        })
        .map(|(i, _)| i)
}

/// True if a user message is a "real" user message (has text/image/document
/// content) rather than purely a tool_result wrapper. Plain-string content
/// is always real; an empty content array is treated as real (degenerate).
fn is_real_user_message(msg: &Value) -> bool {
    match msg.get("content") {
        Some(Value::String(_)) => true,
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                return true;
            }
            arr.iter().any(|block| {
                let t = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                t != "tool_result"
            })
        }
        _ => true,
    }
}

/// Find the most recent `[procs]\n[loadavg]\n[meminfo]\n[ctx]` quadruplet
/// in any text content of any tool_result block (or top-level text content)
/// in the dropped slice. Returns the four-line envelope as a string.
///
/// Manual line-scanning avoids the regex dependency.
fn extract_latest_envelope(messages: &[Value]) -> Option<String> {
    let mut latest: Option<String> = None;

    for msg in messages {
        let texts = collect_text_content(msg);
        for text in &texts {
            if let Some(env) = find_envelope_quadruplet(text) {
                latest = Some(env);
            }
        }
    }
    latest
}

/// Pull every text payload out of a message: top-level string content,
/// text blocks in array content, and text inside tool_result content.
fn collect_text_content(msg: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    match msg.get("content") {
        Some(Value::String(s)) => texts.push(s.clone()),
        Some(Value::Array(arr)) => {
            for block in arr {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                    "tool_result" => match block.get("content") {
                        Some(Value::String(s)) => texts.push(s.clone()),
                        Some(Value::Array(inner)) => {
                            for inner_block in inner {
                                if let Some(t) = inner_block.get("text").and_then(|t| t.as_str()) {
                                    texts.push(t.to_string());
                                }
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        _ => {}
    }
    texts
}

/// Scan text for a four-consecutive-line envelope starting with [procs],
/// then [loadavg], [meminfo], [ctx]. Returns the joined four lines.
/// Returns the LAST occurrence in the text (most recent if text is a
/// concatenation of multiple turns).
fn find_envelope_quadruplet(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut last: Option<String> = None;
    let mut i = 0;
    while i + 3 < lines.len() {
        if lines[i].starts_with("[procs]")
            && lines[i + 1].starts_with("[loadavg]")
            && lines[i + 2].starts_with("[meminfo]")
            && lines[i + 3].starts_with("[ctx]")
        {
            last = Some(format!(
                "{}\n{}\n{}\n{}",
                lines[i],
                lines[i + 1],
                lines[i + 2],
                lines[i + 3]
            ));
            i += 4;
        } else {
            i += 1;
        }
    }
    last
}

#[derive(Debug, Clone, Default)]
struct NativeToolCall {
    name: String,
    /// Tool-use id from the inbound request (claude-code: `toolu_<…>`,
    /// other harnesses vary). Carried through so the projection can
    /// emit a short tag (`short_tag`-style suffix) — addressable by
    /// eye and resolvable to a body via the transcript index.
    id: String,
    /// Raw input args (preserved so we can render per-tool signatures
    /// at compose time — extracting offset/limit for Read, cmd for
    /// Bash, etc. — instead of dumping the JSON blob).
    input: Option<Value>,
    output_size: usize,
    status: String,
    /// Inline body — populated only when (a) status is "error" AND
    /// (b) the output is under `INLINE_ERROR_BUDGET` chars. Errors
    /// are usually small and almost always worth seeing without a
    /// re-fetch (the asymmetry the model running through the proxy
    /// flagged: "shapes-only for [ok], body-inline for [error] under
    /// some size threshold"). For [ok] this stays None — those go
    /// through the handle / recall / re-run paths.
    error_body: Option<String>,
}

/// Maximum characters of an error body we inline into the projection.
/// Sized to comfortably fit common compiler errors, test failures, and
/// command stderr without bloating the synthetic when an error is
/// pathologically large.
const INLINE_ERROR_BUDGET: usize = 1500;

/// Build a compact summary of tool_use/tool_result pairs in the dropped
/// slice, paired by tool_use_id. Only the most recent `cap` calls are
/// retained (in encounter order).
fn summarize_native_tools(messages: &[Value], cap: usize) -> Vec<NativeToolCall> {
    use std::collections::HashMap;

    // ordered list of tool_use_ids as we encounter them
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, NativeToolCall> = HashMap::new();

    for msg in messages {
        let content = match msg.get("content").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => continue,
        };
        for block in content {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match block_type {
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let input = block.get("input").cloned();
                    if !by_id.contains_key(&id) {
                        order.push(id.clone());
                    }
                    by_id.insert(
                        id.clone(),
                        NativeToolCall {
                            name,
                            id,
                            input,
                            output_size: 0,
                            status: "pending".into(),
                            error_body: None,
                        },
                    );
                }
                "tool_result" => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    let is_error = block.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
                    let (output_size, output_text) = match block.get("content") {
                        Some(Value::String(s)) => (s.len(), Some(s.clone())),
                        Some(Value::Array(arr)) => {
                            // Concatenate text blocks; size = total
                            // characters across them. Non-text blocks
                            // contribute their JSON length to size but
                            // not to the inline-able text.
                            let mut texts: Vec<String> = Vec::new();
                            let mut total: usize = 0;
                            for b in arr {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    total += t.len();
                                    texts.push(t.to_string());
                                } else {
                                    total += serde_json::to_string(b).map(|s| s.len()).unwrap_or(0);
                                }
                            }
                            let joined = if texts.is_empty() {
                                None
                            } else {
                                Some(texts.join("\n"))
                            };
                            (total, joined)
                        }
                        _ => (0, None),
                    };
                    if let Some(call) = by_id.get_mut(&id) {
                        call.output_size = output_size;
                        call.status = if is_error { "error".into() } else { "ok".into() };
                        // Inline error bodies under budget. [ok]
                        // results stay shape-only — handle / recall /
                        // re-run paths cover those.
                        if is_error
                            && let Some(t) = output_text
                            && t.chars().count() <= INLINE_ERROR_BUDGET
                        {
                            call.error_body = Some(t);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Take the LAST `cap` calls in encounter order (most recent).
    if order.len() > cap {
        let drop_n = order.len() - cap;
        order.drain(..drop_n);
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

/// →1979 K3: scan the FULL dropped slice (uncapped — this is the
/// survival mechanism) for frontier items that must not fall off the
/// projection just because resolved [ok] shapes pushed them past the
/// paging cap:
///
/// 1. **In-flight tool_use** — a `tool_use` with no paired
///    `tool_result` at the cycle boundary (cycle died mid-chain).
/// 2. **Armed monitors** — `Monitor` calls that returned [ok];
///    liveness is not derivable from the transcript, so entries carry
///    a verify hint rather than asserting the monitor still runs.
/// 3. **Held locks / lane claims** — kernel `lock` `create` without a
///    matching `release`/`break` for the same name in the window.
///
/// Because this is recomputed from the request's message history on
/// every rebuild, frontier items survive cycle death and compaction
/// regardless of how deep they sit — they re-emit into each new
/// synthetic until the transcript shows them resolved.
fn summarize_unresolved(messages: &[Value]) -> Vec<String> {
    let all = summarize_native_tools(messages, usize::MAX);
    let mut items: Vec<String> = Vec::new();

    // 1. in-flight tool_use (pending = no tool_result observed)
    for call in &all {
        if call.status == "pending" {
            items.push(format!(
                "in-flight tool_use: {} — no result observed at cycle boundary; re-issue or check substrate before assuming it ran",
                render_tool_signature(call)
            ));
        }
    }

    // 2. armed monitors (transcript can't prove liveness — hint, don't assert)
    for call in &all {
        if call.status == "ok" && call.name == "Monitor" {
            let desc = call
                .input
                .as_ref()
                .and_then(|i| i.get("description"))
                .and_then(|d| d.as_str())
                .unwrap_or("?");
            items.push(format!(
                "armed monitor: {:?} (liveness not derivable from transcript — verify via /tasks)",
                truncate_chars(desc, 100)
            ));
        }
    }

    // 3. locks created and not released in the window (lane claims).
    // Encounter order is preserved by `all`, so create→release pairs
    // cancel correctly even when re-acquired.
    let mut held: Vec<String> = Vec::new();
    for call in &all {
        if call.status != "ok" {
            continue;
        }
        let is_lock = call.name == "mcp__ostk__lock" || call.name == "lock";
        if !is_lock {
            continue;
        }
        let (action, name) = match call.input.as_ref() {
            Some(i) => (
                i.get("action").and_then(|a| a.as_str()).unwrap_or(""),
                i.get("name").and_then(|n| n.as_str()).unwrap_or(""),
            ),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        match action {
            "create" => {
                if !held.iter().any(|h| h == name) {
                    held.push(name.to_string());
                }
            }
            "release" | "break" => held.retain(|h| h != name),
            _ => {}
        }
    }
    for name in held {
        items.push(format!(
            "lock held: {name} (created this window, no release observed — confirm with lock status before re-claiming)"
        ));
    }

    items
}

fn extract_user_intent_thread(messages: &[Value], cap: usize) -> Vec<String> {
    let mut thread = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let text: Option<String> = match msg.get("content") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(arr)) => {
                let texts: Vec<String> = arr
                    .iter()
                    .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from))
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join("\n"))
                }
            }
            _ => None,
        };
        if let Some(t) = text {
            let trimmed = truncate_chars(&t, 240);
            if !trimmed.trim().is_empty() {
                thread.push(trimmed);
            }
        }
    }
    if thread.len() > cap {
        let drop_n = thread.len() - cap;
        thread.drain(..drop_n);
    }
    thread
}

/// Per-tool signature renderer — turns a `NativeToolCall` into a single
/// human/model-readable line that surfaces the key arguments first-
/// class instead of burying them in a JSON blob. Closes the gap the
/// model called out about Read offset/limit being unrecoverable from
/// `{"path":"...","offset":N,"limit":M}`-style summaries.
///
/// For tools we don't recognize, falls through to a truncated JSON
/// dump so we never lose information.
fn render_tool_signature(call: &NativeToolCall) -> String {
    let lower = call.name.to_lowercase();
    let args = call.input.as_ref();
    let str_arg = |k: &str| -> Option<String> {
        args.and_then(|a| a.get(k))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let u64_arg = |k: &str| -> Option<u64> { args.and_then(|a| a.get(k)).and_then(|v| v.as_u64()) };

    let signature = match lower.as_str() {
        "read" | "fs_read" | "file:read" => {
            // claude-code native Read uses `file_path`; MCP-routed
            // `read` uses `path`. Check both — different namespaces,
            // same intent.
            let path = str_arg("path")
                .or_else(|| str_arg("file_path"))
                .unwrap_or_else(|| "?".into());
            match (u64_arg("offset"), u64_arg("limit")) {
                (Some(o), Some(l)) => format!("**read** {} [offset:{} limit:{}]", path, o, l),
                (Some(o), None) => format!("**read** {} [offset:{}]", path, o),
                (None, Some(l)) => format!("**read** {} [limit:{}]", path, l),
                (None, None) => format!("**read** {}", path),
            }
        }
        "bash" | "shell" | "sh_run" => {
            let cmd = str_arg("cmd")
                .or_else(|| str_arg("command"))
                .unwrap_or_else(|| "?".into());
            format!("**bash** {}", truncate_chars(&cmd, 100))
        }
        "fs_ops" | "edit" | "file:edit" => {
            let path = str_arg("path")
                .or_else(|| str_arg("file_path"))
                .unwrap_or_else(|| "?".into());
            let op = str_arg("op")
                .or_else(|| {
                    // CAS edit (old_str + new_str) → infer "str_replace"
                    args.and_then(|a| a.get("old_str")).and(Some("str_replace".to_string()))
                })
                .unwrap_or_else(|| "edit".into());
            format!("**fs_ops** {} op={}", path, op)
        }
        "write" | "fs_write" => {
            let path = str_arg("path")
                .or_else(|| str_arg("file_path"))
                .unwrap_or_else(|| "?".into());
            format!("**write** {}", path)
        }
        "search" | "grep" | "glob" | "find" => {
            let q = str_arg("query")
                .or_else(|| str_arg("pattern"))
                .unwrap_or_else(|| "?".into());
            let scope = str_arg("scope");
            let mode = str_arg("mode");
            let mut s = format!("**search** {:?}", truncate_chars(&q, 80));
            if let Some(sc) = scope {
                s.push_str(&format!(" scope={}", sc));
            }
            if let Some(m) = mode {
                s.push_str(&format!(" mode={}", m));
            }
            s
        }
        "recall" | "recall_search" | "recall_outline" => {
            let addr = str_arg("addr")
                .or_else(|| str_arg("query"))
                .unwrap_or_else(|| "?".into());
            format!("**{}** {}", call.name, truncate_chars(&addr, 100))
        }
        "task" | "agent" => {
            let desc = str_arg("description").unwrap_or_else(|| "?".into());
            format!("**{}** {}", call.name, truncate_chars(&desc, 100))
        }
        "webfetch" | "websearch" | "web_read" | "web_fetch" => {
            let url = str_arg("url")
                .or_else(|| str_arg("query"))
                .unwrap_or_else(|| "?".into());
            format!("**{}** {}", call.name, truncate_chars(&url, 100))
        }
        _ => {
            // Unknown tool — fall back to truncated JSON so we never
            // silently drop information. 200 chars matches the prior
            // input_summary truncation budget.
            let blob = call
                .input
                .as_ref()
                .map(|v| {
                    let s = serde_json::to_string(v).unwrap_or_default();
                    truncate_chars(&s, 200)
                })
                .unwrap_or_default();
            format!("**{}** {}", call.name, blob)
        }
    };

    let mut head = format!("{} [{}] (out:{}b)", signature, call.status, call.output_size);
    if let Some(tag) = short_tool_tag(&call.id) {
        head.push_str(&format!(" [{}]", tag));
    }
    // Inline body for [error] under budget — small, signal-dense,
    // almost always worth seeing without a re-fetch. [ok] results
    // stay shape-only.
    match &call.error_body {
        Some(body) => format!("{}\n  ```\n  {}\n  ```", head, body.replace('\n', "\n  ")),
        None => head,
    }
}

/// Last 8 chars of a tool-use id, lowercased — addressable by eye, and
/// resolvable to a full body via the transcript index. Returns None for
/// empty ids or ids shorter than 8 chars (those are too generic to be
/// useful as a tag).
fn short_tool_tag(id: &str) -> Option<String> {
    if id.chars().count() < 8 {
        return None;
    }
    let total = id.chars().count();
    let tail: String = id.chars().skip(total - 8).collect();
    Some(tail.to_lowercase())
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn compose_synthetic_context(
    envelope: &Option<String>,
    native_summary: &[NativeToolCall],
    unresolved: &[String],
    user_thread: &[String],
    transcript_tail_summary: Option<&str>,
    recent_assistant_digests: Option<&str>,
    templates_summary: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("# Kernel projection — current cycle state\n\n");
    out.push_str(
        "See your system prompt's `# ostk-cache kernel orientation` block for the trichotomy, the ok/error asymmetry, and the turn-digest fence requirement. The sections below are dynamic state for *this cycle only*: live envelope, paged tool activity, unresolved frontier, prior user thread, recent assistant digests.\n\n",
    );

    out.push_str("## Live state envelope\n\n");
    if let Some(env) = envelope {
        out.push_str("```\n");
        out.push_str(env);
        out.push_str("\n```\n\n");
    } else {
        out.push_str(
            "```\n[procs] count:0 active:0 stale:0 dead:0 ctx_p95:0 concern:none\n[loadavg] needles: 0 open (0 P0) | fleet: 0/0 alive | nudges: 0\n[meminfo] ctx: 0% 0k/0k Buffers:0k tok_calls:0\n[ctx] \u{0394}0t:0s | audit:+0 | needles:0 | fleet:0/0 | nudge:0 | conflict:none\n```\n\n",
        );
        out.push_str(
            "(No envelope was present in the discarded history; the placeholder above is standalone-mode synthesis. Federated mode will populate from live kernel state.)\n\n",
        );
    }

    if !native_summary.is_empty() {
        out.push_str("## Recent tool activity (paged from prior turns)\n\n");
        for call in native_summary {
            out.push_str(&format!("- {}\n", render_tool_signature(call)));
        }
        out.push('\n');
    }

    // →1979 K3: pending frontier. Rendered even past the paging cap —
    // these are the items a fresh cycle must not silently forget.
    if !unresolved.is_empty() {
        out.push_str("## Unresolved (frontier at cycle boundary — exempt from paging, recomputed every cycle)\n\n");
        for item in unresolved {
            out.push_str(&format!("- {item}\n"));
        }
        out.push('\n');
    }

    if let Some(tpl) = templates_summary
        && !tpl.trim().is_empty()
    {
        out.push_str(tpl);
        if !tpl.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    if !user_thread.is_empty() {
        out.push_str("## Prior user intent thread\n\n");
        for (i, msg) in user_thread.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, msg));
        }
        out.push('\n');
    }

    if let Some(digests) = recent_assistant_digests
        && !digests.trim().is_empty()
    {
        out.push_str(digests);
        if !digests.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    if let Some(tail) = transcript_tail_summary
        && !tail.trim().is_empty()
    {
        out.push_str(tail);
        if !tail.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    out.push_str("## Current cycle\n\nYour user's latest message follows immediately, along with any in-flight tool_use/tool_result chain from your response so far. Treat the projection above as authoritative state; do not assume content beyond it without recalling it.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn section_sizes_breaks_down_request() {
        let req = json!({
            "system": [{"type": "text", "text": "SYS"}],
            "tools": [{"name": "Bash"}, {"name": "Read"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "synthetic projection"}]},
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            ],
        });
        let s = section_sizes(&req, true);
        assert!(s.system > 0);
        assert!(s.tools > 0);
        assert!(s.synthetic > 0);
        assert!(s.in_flight > 0);
        assert_eq!(s.total(), s.system + s.tools + s.synthetic + s.in_flight);
    }

    #[test]
    fn section_sizes_no_synthetic_folds_into_in_flight() {
        let req = json!({
            "system": "s",
            "messages": [
                {"role": "user", "content": "user msg"},
                {"role": "assistant", "content": "reply"},
            ],
        });
        let s = section_sizes(&req, false);
        assert_eq!(s.synthetic, 0);
        assert!(s.in_flight > 0);
    }

    #[test]
    fn enforce_soft_cap_noop_when_under_cap() {
        let mut req = json!({"messages": [{"role": "user", "content": "hi"}]});
        let r = enforce_soft_cap(&mut req, 10 * 1024 * 1024);
        assert!(!r.applied_any());
        assert_eq!(r.tier_a_ejected, 0);
        assert_eq!(r.tier_b_pairs_pruned, 0);
        assert!(!r.irreducible);
    }

    #[test]
    fn enforce_soft_cap_noop_when_cap_zero() {
        // Cap 0 means disabled — never trigger reductions even on huge bodies.
        let blob = "x".repeat(2 * 1024 * 1024);
        let mut req = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tu_big",
                    "content": [{"type": "text", "text": blob}]
                }]
            }],
        });
        let r = enforce_soft_cap(&mut req, 0);
        assert!(!r.applied_any());
    }

    #[test]
    fn tier_a_ejects_largest_tool_result_first() {
        let big = "x".repeat(2 * 1024 * 1024); // 2MB
        let medium = "y".repeat(512 * 1024); // 512KB — also above 100KB threshold
        let small = "z".repeat(50 * 1024); // 50KB — below threshold

        let mut req = json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "kick"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_big", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "tu_med", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "tu_small", "name": "Read", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_big",
                        "content": [{"type": "text", "text": big}]},
                    {"type": "tool_result", "tool_use_id": "tu_med",
                        "content": [{"type": "text", "text": medium}]},
                    {"type": "tool_result", "tool_use_id": "tu_small",
                        "content": [{"type": "text", "text": small}]},
                ]},
            ],
        });

        // Soft cap 1MB → must eject "big" (and likely "medium" too) but
        // preserve "small". Even after ejecting, pairings stay intact.
        let r = enforce_soft_cap(&mut req, 1024 * 1024);
        assert!(r.tier_a_ejected >= 1, "expected ≥1 ejection, got {}", r.tier_a_ejected);
        assert!(!r.irreducible, "should not be irreducible after Tier A");
        assert!(r.tier_a_bytes_recovered >= 2 * 1024 * 1024);

        // Find the big result and verify it's a stub now.
        let msgs = req["messages"].as_array().unwrap();
        let big_result = msgs[2]["content"][0]["content"][0]["text"].as_str().unwrap();
        assert!(
            big_result.starts_with("[ejected"),
            "tu_big should be ejected, got: {}",
            &big_result[..big_result.len().min(80)]
        );
        // Small result stayed inline.
        let small_result = msgs[2]["content"][2]["content"][0]["text"].as_str().unwrap();
        assert_eq!(small_result.len(), 50 * 1024);
    }

    #[test]
    fn tier_a_preserves_tool_use_id_pairing() {
        let blob = "x".repeat(500 * 1024);
        let mut req = json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "kick"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_42", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_42",
                        "content": [{"type": "text", "text": blob}]}
                ]},
            ],
        });
        let _r = enforce_soft_cap(&mut req, 50 * 1024);
        let ejected_id = req["messages"][2]["content"][0]["tool_use_id"].as_str().unwrap();
        assert_eq!(ejected_id, "tu_42", "tool_use_id must survive Tier A");
    }

    #[test]
    fn tier_d_irreducible_on_oversized_system() {
        // System prompt is not reducible. Even after Tier A/B/C fail to
        // help, enforce_soft_cap should return irreducible=true.
        let sys = "S".repeat(2 * 1024 * 1024);
        let mut req = json!({
            "system": sys,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let r = enforce_soft_cap(&mut req, 100 * 1024);
        assert!(r.irreducible, "huge system prompt should be irreducible");
    }

    #[test]
    fn finds_cycle_boundary_at_last_real_user_message() {
        let messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "reply"}),
            json!({"role": "user", "content": "second"}),
            json!({"role": "assistant", "content": "reply2"}),
        ];
        assert_eq!(find_cycle_boundary_idx(&messages), Some(2));
    }

    #[test]
    fn cycle_boundary_skips_tool_result_wrappers() {
        let messages = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "reply"}),
            json!({"role": "user", "content": [{"type": "text", "text": "do it"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {"cmd": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "files"}
            ]}),
        ];
        // Most recent user message at index 4 is a tool_result wrapper.
        // The boundary should be at index 2 ("do it").
        assert_eq!(find_cycle_boundary_idx(&messages), Some(2));
    }

    #[test]
    fn cycle_boundary_handles_mixed_content_user_msg() {
        let messages = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "X", "content": "old"},
                {"type": "text", "text": "and also a follow-up question"}
            ]}),
        ];
        // Has text alongside tool_result — counts as real user message.
        assert_eq!(find_cycle_boundary_idx(&messages), Some(0));
    }

    #[test]
    fn finds_envelope_quadruplet_in_text() {
        let text = "some prior junk\n[procs] count:1\n[loadavg] needles: 0\n[meminfo] ctx: 0%\n[ctx] \u{0394}1t:5s\nmore junk";
        let env = find_envelope_quadruplet(text);
        assert!(env.is_some());
        let e = env.unwrap();
        assert!(e.starts_with("[procs]"));
        assert!(e.contains("[ctx]"));
    }

    #[test]
    fn extracts_envelope_from_tool_result() {
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "X",
                "content": "stuff\n[procs] count:1\n[loadavg] needles: 0\n[meminfo] ctx: 0%\n[ctx] \u{0394}1t:5s"
            }]
        })];
        let env = extract_latest_envelope(&messages);
        assert!(env.is_some());
        assert!(env.as_ref().unwrap().contains("[procs]"));
        assert!(env.as_ref().unwrap().contains("[ctx]"));
    }

    #[test]
    fn extracts_envelope_from_tool_result_array_content() {
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "X",
                "content": [
                    {"type": "text", "text": "[procs] count:2\n[loadavg] needles: 1\n[meminfo] ctx: 5%\n[ctx] \u{0394}2t:10s"}
                ]
            }]
        })];
        let env = extract_latest_envelope(&messages);
        assert!(env.is_some(), "expected envelope from array tool_result content");
    }

    #[test]
    fn rebuild_drops_prior_turns() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "old reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        match outcome {
            RebuildOutcome::Applied(report) => {
                assert_eq!(report.turns_dropped, 2);
                assert!(report.bytes_out > 0);
            }
            other => panic!("expected Applied, got {:?}", other),
        }
        let messages = req.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 2, "synthetic + in-flight user message");
        assert_eq!(messages[0].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("user"));
        assert_eq!(messages[1].get("content").unwrap().as_str(), Some("new"));
    }

    // →2032: a compact_target_bytes budget elides optional sections in
    // priority order (templates → tail → digests) and reports the
    // count; the core projection always survives.
    #[test]
    fn compact_target_elides_optional_sections() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "old reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: Some("tail-section-marker".into()),
            recent_assistant_digests: Some("digest-section-marker".into()),
            templates_summary: Some("templates-section-marker".into()),
            prior_user_turns_override: None,
            body_bytes_in: None,
            // Impossible budget: every optional section must go and the
            // core projection still stands.
            compact_target_bytes: Some(1),
        };
        match apply_rebuild(&mut req, &config) {
            RebuildOutcome::Applied(report) => {
                assert_eq!(report.sections_elided, 3);
                assert!(report.bytes_out > 0, "core projection must survive");
            }
            other => panic!("expected Applied, got {:?}", other),
        }
        let text = serde_json::to_string(&req).unwrap();
        assert!(!text.contains("templates-section-marker"));
        assert!(!text.contains("tail-section-marker"));
        assert!(!text.contains("digest-section-marker"));
        assert!(text.contains("Kernel projection"), "core sections never elided");
    }

    // →2032: a generous budget elides nothing — WARM-equivalent
    // composition is byte-identical to the no-budget path.
    #[test]
    fn compact_target_keeps_sections_when_under_budget() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "old reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: Some("tail-section-marker".into()),
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: Some(10_000_000),
        };
        match apply_rebuild(&mut req, &config) {
            RebuildOutcome::Applied(report) => {
                assert_eq!(report.sections_elided, 0);
            }
            other => panic!("expected Applied, got {:?}", other),
        }
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.contains("tail-section-marker"));
    }

    #[test]
    fn rebuild_preserves_in_flight_tool_chain() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": [{"type": "text", "text": "do it"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {"cmd": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "files"}
                ]}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let messages = req.get("messages").unwrap().as_array().unwrap();
        // synthetic + "do it" + assistant tool_use + tool_result
        assert_eq!(messages.len(), 4, "synthetic + 3-turn in-flight chain");
        // tool_use_id must round-trip
        let tool_use_id = messages[2]
            .get("content").unwrap()
            .as_array().unwrap()[0]
            .get("id").unwrap().as_str().unwrap();
        assert_eq!(tool_use_id, "tu_1");
        let tool_result_id = messages[3]
            .get("content").unwrap()
            .as_array().unwrap()[0]
            .get("tool_use_id").unwrap().as_str().unwrap();
        assert_eq!(tool_result_id, "tu_1");
    }

    #[test]
    fn rebuild_disabled_no_change() {
        let mut req = json!({"messages": [{"role": "user", "content": "x"}]});
        let original = req.clone();
        let config = RebuildConfig {
            enabled: false,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(matches!(outcome, RebuildOutcome::Disabled));
        assert_eq!(req, original);
    }

    #[test]
    fn rebuild_skipped_no_user_message() {
        let mut req = json!({"messages": [{"role": "assistant", "content": "x"}]});
        let original = req.clone();
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(matches!(outcome, RebuildOutcome::Skipped(_)));
        assert_eq!(req, original);
    }

    #[test]
    fn rebuild_skipped_first_turn() {
        let mut req = json!({"messages": [{"role": "user", "content": "first"}]});
        let original = req.clone();
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(matches!(outcome, RebuildOutcome::Skipped(_)));
        assert_eq!(req, original);
    }

    // ── Per-tool signature rendering ──────────────────────────────────────

    #[test]
    fn render_signature_read_with_offset_and_limit() {
        let call = NativeToolCall {
            name: "read".into(),
            input: Some(json!({"path": "/abs/path/file.rs", "offset": 120, "limit": 50})),
            output_size: 95,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("read"));
        assert!(line.contains("/abs/path/file.rs"));
        assert!(line.contains("offset:120"));
        assert!(line.contains("limit:50"));
        assert!(line.contains("(out:95b)"));
    }

    #[test]
    fn render_signature_read_with_file_path_alias() {
        // claude-code's native Read tool sends file_path, not path.
        let call = NativeToolCall {
            name: "Read".into(),
            input: Some(json!({"file_path": "/abs/native_read.rs", "limit": 80})),
            output_size: 800,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("/abs/native_read.rs"), "must extract file_path");
        assert!(!line.contains(" ?"), "no `?` placeholder when file_path is set");
        assert!(line.contains("limit:80"));
    }

    #[test]
    fn render_signature_write_with_file_path_alias() {
        let call = NativeToolCall {
            name: "Write".into(),
            input: Some(json!({"file_path": "/abs/out.rs", "content": "..."})),
            output_size: 0,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("/abs/out.rs"));
    }

    #[test]
    fn render_signature_edit_with_file_path_alias() {
        let call = NativeToolCall {
            name: "Edit".into(),
            input: Some(json!({"file_path": "/abs/edit.rs", "old_str": "x", "new_str": "y"})),
            output_size: 50,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("/abs/edit.rs"));
        // CAS-edit inference (old_str + new_str → str_replace)
        assert!(line.contains("op=str_replace"));
    }

    #[test]
    fn render_signature_read_without_offset() {
        let call = NativeToolCall {
            name: "Read".into(),
            input: Some(json!({"path": "/abs/std.rs"})),
            output_size: 6467,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("/abs/std.rs"));
        assert!(!line.contains("offset:"));
        assert!(line.contains("(out:6467b)"));
    }

    #[test]
    fn render_signature_bash_extracts_cmd() {
        let call = NativeToolCall {
            name: "bash".into(),
            input: Some(json!({"cmd": "cargo test --release"})),
            output_size: 225,
            status: "error".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("cargo test --release"));
        assert!(line.contains("[error]"));
    }

    #[test]
    fn render_signature_search_extracts_query_and_axes() {
        let call = NativeToolCall {
            name: "search".into(),
            input: Some(json!({"query": "fn main", "scope": "code", "mode": "regex"})),
            output_size: 800,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("fn main"));
        assert!(line.contains("scope=code"));
        assert!(line.contains("mode=regex"));
    }

    #[test]
    fn render_signature_unknown_tool_falls_back_to_json() {
        let call = NativeToolCall {
            name: "exotic_tool".into(),
            input: Some(json!({"weird": "args", "n": 42})),
            output_size: 10,
            status: "ok".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("exotic_tool"));
        assert!(line.contains("weird"));
        assert!(line.contains("42"));
    }

    #[test]
    fn render_signature_handles_missing_input() {
        let call = NativeToolCall {
            name: "bash".into(),
            input: None,
            output_size: 0,
            status: "pending".into(),
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("bash"));
        assert!(line.contains("(out:0b)"));
    }

    // ── Error-body inlining (asymmetry: shapes for [ok], body for [error]) ─

    #[test]
    fn render_signature_inlines_short_error_body() {
        let call = NativeToolCall {
            name: "bash".into(),
            input: Some(json!({"cmd": "cargo test"})),
            output_size: 225,
            status: "error".into(),
            error_body: Some("error[E0308]: mismatched types\n  --> src/main.rs:10:5\n   |\n10 |     return 42;\n   |     ^^^^^^^^^ expected `()`, found integer".into()),
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(line.contains("[error]"));
        assert!(line.contains("(out:225b)"));
        assert!(line.contains("E0308"));
        assert!(line.contains("mismatched types"));
        assert!(line.contains("```"));
    }

    #[test]
    fn render_signature_omits_body_when_status_is_ok() {
        let call = NativeToolCall {
            name: "bash".into(),
            input: Some(json!({"cmd": "ls"})),
            output_size: 200,
            status: "ok".into(),
            // Even if a body sneaks in, the renderer should ignore it
            // for ok status. (In practice the scanner only populates
            // error_body when status==error, but the renderer must
            // not double-rely on that.)
            error_body: None,
            id: String::new(),
        };
        let line = render_tool_signature(&call);
        assert!(!line.contains("```"));
    }

    #[test]
    fn summarize_captures_error_body_under_budget() {
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_E", "name": "Bash", "input": {"cmd": "false"}}
            ]}),
            json!({"role": "user", "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "tu_E",
                    "content": "thread main panicked at 'boom'",
                    "is_error": true
                }
            ]}),
        ];
        let calls = summarize_native_tools(&messages, 50);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].status, "error");
        assert!(calls[0].error_body.is_some());
        assert!(calls[0].error_body.as_ref().unwrap().contains("boom"));
    }

    #[test]
    fn summarize_does_not_capture_ok_body() {
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_K", "name": "Bash", "input": {"cmd": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "tu_K",
                    "content": "file1\nfile2",
                    "is_error": false
                }
            ]}),
        ];
        let calls = summarize_native_tools(&messages, 50);
        assert_eq!(calls[0].status, "ok");
        assert!(calls[0].error_body.is_none(), "ok results stay shape-only");
    }

    #[test]
    fn summarize_drops_error_body_over_budget() {
        let huge = "x".repeat(INLINE_ERROR_BUDGET + 100);
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_X", "name": "Bash", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_X", "content": huge, "is_error": true}
            ]}),
        ];
        let calls = summarize_native_tools(&messages, 50);
        assert_eq!(calls[0].status, "error");
        assert!(
            calls[0].error_body.is_none(),
            "errors over budget fall through to shape-only — re-fetch is the answer"
        );
    }

    // ── Discipline preamble ───────────────────────────────────────────────

    #[test]
    fn render_signature_appends_short_tool_tag() {
        let call = NativeToolCall {
            name: "Bash".into(),
            id: "toolu_01ABCdefA8B99E0D".into(),
            input: Some(json!({"cmd": "ls"})),
            output_size: 31,
            status: "ok".into(),
            error_body: None,
        };
        let line = render_tool_signature(&call);
        // Last 8 chars, lowercased.
        assert!(line.ends_with(" [a8b99e0d]"), "got: {}", line);
        assert!(line.contains("(out:31b) ["), "tag follows the (out:Nb) marker");
    }

    #[test]
    fn render_signature_omits_tag_when_id_missing() {
        let call = NativeToolCall {
            name: "Bash".into(),
            id: String::new(),
            input: Some(json!({"cmd": "ls"})),
            output_size: 12,
            status: "ok".into(),
            error_body: None,
        };
        let line = render_tool_signature(&call);
        assert!(line.ends_with("(out:12b)"), "no tag suffix when id empty: {}", line);
    }

    #[test]
    fn render_signature_omits_tag_when_id_too_short() {
        let call = NativeToolCall {
            name: "Bash".into(),
            id: "abc123".into(),
            input: Some(json!({"cmd": "ls"})),
            output_size: 5,
            status: "ok".into(),
            error_body: None,
        };
        let line = render_tool_signature(&call);
        assert!(line.ends_with("(out:5b)"), "no tag suffix when id <8 chars: {}", line);
    }

    #[test]
    fn synthetic_context_includes_discipline_preamble() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        apply_rebuild(&mut req, &config);
        let messages = req.get("messages").unwrap().as_array().unwrap();
        let synthetic = messages[0].get("content").unwrap().as_array().unwrap()[0]
            .get("text")
            .unwrap()
            .as_str()
            .unwrap();
        // After →1830, discipline preamble is in system tier — the
        // synthetic block stays purely dynamic state. The synthetic
        // should NOT carry the discipline preamble anymore.
        assert!(
            !synthetic.contains("Operating discipline"),
            "discipline preamble must live in system tier (→1830), not synthetic"
        );
        assert!(
            !synthetic.contains("End every turn with a digest fence"),
            "fence requirement is in system orientation, not synthetic"
        );
        // The synthetic still has a small framing reference pointing
        // the model to its system orientation:
        assert!(
            synthetic.contains("system orientation")
                || synthetic.contains("system prompt"),
            "synthetic must reference where discipline lives"
        );
    }

    // ── →1830: system-tier kernel orientation ───────────────────────────

    #[test]
    fn append_orientation_to_array_system() {
        let mut req = json!({
            "system": [
                {"type": "text", "text": "claude-code original system block"}
            ]
        });
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(appended);
        let arr = req.get("system").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // claude-code's original block stays at index 0:
        assert_eq!(
            arr[0].get("text").unwrap().as_str(),
            Some("claude-code original system block")
        );
        // Our orientation block is appended at the end with cache_control:
        let last = arr.last().unwrap();
        assert_eq!(
            last.get("cache_control").and_then(|c| c.get("ttl")).and_then(|t| t.as_str()),
            Some("1h")
        );
        assert!(last.get("text").unwrap().as_str().unwrap().contains("kernel orientation"));
    }

    #[test]
    fn append_orientation_wraps_string_system_into_array() {
        let mut req = json!({"system": "claude-code's flat string system"});
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(appended);
        let arr = req.get("system").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(
            arr[0].get("text").unwrap().as_str(),
            Some("claude-code's flat string system")
        );
        assert!(
            arr[1]
                .get("text")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("kernel orientation")
        );
    }

    #[test]
    fn append_orientation_creates_system_when_absent() {
        let mut req = json!({"messages": []});
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(appended);
        let arr = req.get("system").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert!(
            arr[0]
                .get("text")
                .unwrap()
                .as_str()
                .unwrap()
                .contains("kernel orientation")
        );
    }

    #[test]
    fn append_orientation_is_idempotent_on_array() {
        let mut req = json!({
            "system": [
                {"type": "text", "text": "claude-code original"},
                {"type": "text", "text": KERNEL_ORIENTATION, "cache_control": {"type":"ephemeral","ttl":"1h"}}
            ]
        });
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(!appended, "orientation already present — must not double-append");
        let arr = req.get("system").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn append_orientation_is_idempotent_on_string() {
        let mut req = json!({"system": KERNEL_ORIENTATION});
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(!appended);
        // String not converted to array since no append happened:
        assert!(req.get("system").unwrap().is_string());
    }

    #[test]
    fn count_cache_control_fields_across_request() {
        let req = json!({
            "tools": [
                {"name": "read", "cache_control": {"type":"ephemeral","ttl":"1h"}},
                {"name": "bash"}
            ],
            "system": [
                {"type": "text", "text": "claude-code", "cache_control": {"type":"ephemeral","ttl":"1h"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi", "cache_control": {"type":"ephemeral","ttl":"5m"}},
                    {"type": "text", "text": "no marker"}
                ]}
            ]
        });
        assert_eq!(count_cache_control_fields(&req), 3);
    }

    #[test]
    fn append_orientation_at_limit_drops_cache_control_marker() {
        // Set up a request already at the 4-marker limit.
        let mut req = json!({
            "tools": [
                {"name": "read", "cache_control": {"type":"ephemeral","ttl":"1h"}}
            ],
            "system": [
                {"type": "text", "text": "claude-code", "cache_control": {"type":"ephemeral","ttl":"1h"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "user msg", "cache_control": {"type":"ephemeral","ttl":"1h"}},
                    {"type": "text", "text": "another", "cache_control": {"type":"ephemeral","ttl":"1h"}}
                ]}
            ]
        });
        assert_eq!(count_cache_control_fields(&req), ANTHROPIC_CACHE_CONTROL_LIMIT);
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(appended, "orientation text must still be appended");
        let last = req.get("system").unwrap().as_array().unwrap().last().unwrap();
        assert!(
            last.get("cache_control").is_none(),
            "no cache_control marker when at limit (would 400)"
        );
        assert!(last.get("text").unwrap().as_str().unwrap().contains("kernel orientation"));
        // Total markers must remain at the limit (we did not add one):
        assert_eq!(
            count_cache_control_fields(&req),
            ANTHROPIC_CACHE_CONTROL_LIMIT
        );
    }

    #[test]
    fn append_orientation_under_limit_includes_cache_control_marker() {
        let mut req = json!({
            "system": [
                {"type": "text", "text": "claude-code", "cache_control": {"type":"ephemeral","ttl":"1h"}}
            ]
        });
        let before = count_cache_control_fields(&req);
        assert_eq!(before, 1);
        append_kernel_orientation_to_system(&mut req, "1h");
        let after = count_cache_control_fields(&req);
        assert_eq!(after, 2, "orientation marker added under limit");
    }

    #[test]
    fn synthetic_message_has_no_cache_control() {
        // The synthetic block carries dynamic content (envelope etc).
        // A cache_control marker there almost never hits and consumes
        // our budget against the 4-block limit. Drop it.
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        apply_rebuild(&mut req, &config);
        let synthetic_block = req.get("messages").unwrap().as_array().unwrap()[0]
            .get("content")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .clone();
        assert!(
            synthetic_block.get("cache_control").is_none(),
            "synthetic must not carry a cache_control marker"
        );
    }

    #[test]
    fn orientation_text_contains_full_discipline() {
        // The orientation MUST cover everything that used to be in the
        // synthetic preamble — moving content into system tier without
        // dropping any of the discipline.
        assert!(KERNEL_ORIENTATION.contains("Trust the projection"));
        assert!(KERNEL_ORIENTATION.contains("re-run"));
        assert!(KERNEL_ORIENTATION.contains("recall:"));
        assert!(KERNEL_ORIENTATION.contains("handle"));
        assert!(KERNEL_ORIENTATION.contains("shapes-only for `[ok]`"));
        assert!(KERNEL_ORIENTATION.contains("ostk decide"));
        assert!(KERNEL_ORIENTATION.contains("<turn-digest>"));
    }

    #[test]
    fn rebuild_injects_recent_assistant_digests_section() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: Some(
                "## Recent assistant turns (digest)\n\n- **[t-0]** [agreed] did the thing [a.rs]\n".into(),
            ),
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let messages = req.get("messages").unwrap().as_array().unwrap();
        let synthetic = messages[0].get("content").unwrap().as_array().unwrap()[0]
            .get("text")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(synthetic.contains("Recent assistant turns (digest)"));
        assert!(synthetic.contains("did the thing"));
        // Must come before the Current cycle marker (placement check):
        let dig_idx = synthetic.find("Recent assistant turns").unwrap();
        let cycle_idx = synthetic.find("Current cycle").unwrap();
        assert!(dig_idx < cycle_idx);
    }

    #[test]
    fn summarize_native_tools_pairs_use_and_result() {
        let messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_A", "name": "Bash", "input": {"cmd": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_A", "content": "file1\nfile2", "is_error": false}
            ]}),
        ];
        let calls = summarize_native_tools(&messages, 50);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Bash");
        assert_eq!(calls[0].status, "ok");
        assert!(calls[0].output_size > 0);
    }

    #[test]
    fn rebuild_uses_live_envelope_override_when_set() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let kernel_envelope = "[procs] count:42 active:1 stale:0 dead:0 ctx_p95:0 concern:none\n[loadavg] needles: 99 open (3 P0) | fleet: 5/5 alive | nudges: 0\n[meminfo] ctx: 7% 50k/700k Buffers:0k tok_calls:1234\n[ctx] \u{0394}99t:1h | audit:+10 | needles:99 | fleet:5/5 | nudge:0 | conflict:none";
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_kernel".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: Some(kernel_envelope.to_string()),
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        match outcome {
            RebuildOutcome::Applied(report) => {
                assert!(report.envelope_found);
            }
            other => panic!("expected Applied, got {:?}", other),
        }
        let messages = req.get("messages").unwrap().as_array().unwrap();
        let synthetic = messages[0].get("content").unwrap().as_array().unwrap()[0]
            .get("text")
            .unwrap()
            .as_str()
            .unwrap();
        // Kernel-fetched envelope must appear in the synthetic context.
        assert!(synthetic.contains("count:42"), "kernel envelope missing");
        assert!(synthetic.contains("99 open"), "kernel envelope missing");
    }

    #[test]
    fn rebuild_appends_transcript_tail_summary() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": "new"}
            ]
        });
        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: Some(
                "## Cross-session activity (from harness transcript)\n\n- foo\n".into(),
            ),
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let messages = req.get("messages").unwrap().as_array().unwrap();
        let synthetic = messages[0].get("content").unwrap().as_array().unwrap()[0]
            .get("text")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(synthetic.contains("Cross-session activity"));
        assert!(synthetic.contains("Current cycle"));
        // Tail summary must come BEFORE the Current cycle marker.
        let tail_idx = synthetic.find("Cross-session activity").unwrap();
        let cycle_idx = synthetic.find("Current cycle").unwrap();
        assert!(tail_idx < cycle_idx);
    }

    // ── Repair: orphaned tool_result stripping ────────────────────────────

    #[test]
    fn repair_strips_orphaned_tool_result_at_messages_zero() {
        // The exact failure mode from the cargo-test interrupt: a
        // tool_result at messages[0] has no possible "previous message"
        // to pair with, so it's unconditionally orphaned.
        let mut msgs = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_result", "tool_use_id": "toolu_orphan", "content": "..."}
            ]
        })];
        let stripped = repair_orphaned_tool_results(&mut msgs);
        assert_eq!(stripped, 1);
        let arr = msgs[0].get("content").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("type").unwrap().as_str(), Some("text"));
    }

    #[test]
    fn repair_keeps_paired_tool_result() {
        let mut msgs = vec![
            json!({"role": "user", "content": "do it"}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_A", "name": "Bash", "input": {"cmd": "ls"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_A", "content": "ok"}
            ]}),
        ];
        let stripped = repair_orphaned_tool_results(&mut msgs);
        assert_eq!(stripped, 0);
        assert_eq!(msgs.len(), 3, "no messages should be dropped");
    }

    #[test]
    fn repair_strips_tool_result_without_preceding_assistant() {
        // tool_result whose paired tool_use is missing entirely.
        let mut msgs = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "no tool calls"}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_missing", "content": "..."}
            ]}),
        ];
        let stripped = repair_orphaned_tool_results(&mut msgs);
        assert_eq!(stripped, 1);
        // The third message becomes empty after stripping → dropped.
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn repair_strips_only_the_orphan_when_block_is_mixed() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_X", "name": "Bash", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_X", "content": "ok"},
                {"type": "tool_result", "tool_use_id": "tu_orphan", "content": "..."},
                {"type": "text", "text": "by the way"}
            ]}),
        ];
        let stripped = repair_orphaned_tool_results(&mut msgs);
        assert_eq!(stripped, 1, "exactly one orphan stripped");
        let content = msgs[1].get("content").unwrap().as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0].get("tool_use_id").unwrap().as_str(), Some("tu_X"));
        assert_eq!(content[1].get("type").unwrap().as_str(), Some("text"));
    }

    #[test]
    fn repair_drops_message_when_only_orphans() {
        let mut msgs = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": "no tool"}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "ghost1", "content": "x"},
                {"type": "tool_result", "tool_use_id": "ghost2", "content": "y"}
            ]}),
            json!({"role": "user", "content": "follow up"}),
        ];
        let stripped = repair_orphaned_tool_results(&mut msgs);
        assert_eq!(stripped, 2);
        // The all-orphan user message is dropped entirely.
        assert_eq!(msgs.len(), 3);
    }

    #[test]
    fn repair_idempotent() {
        let mut msgs = vec![
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "ghost", "content": "x"},
                {"type": "text", "text": "hi"}
            ]}),
        ];
        let first = repair_orphaned_tool_results(&mut msgs);
        let second = repair_orphaned_tool_results(&mut msgs);
        assert_eq!(first, 1);
        assert_eq!(second, 0, "idempotent — second pass strips nothing");
    }

    #[test]
    fn user_intent_thread_extracts_text() {
        let messages = vec![
            json!({"role": "user", "content": "first message"}),
            json!({"role": "assistant", "content": "reply"}),
            json!({"role": "user", "content": [{"type": "text", "text": "second"}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "X", "content": "ignore me"}
            ]}),
        ];
        let thread = extract_user_intent_thread(&messages, 10);
        assert_eq!(thread.len(), 2, "tool_result-only user msgs are skipped");
        assert!(thread[0].contains("first"));
        assert!(thread[1].contains("second"));
    }

    // ── →1834: assembled-request TTL ordering invariant ─────────────────
    //
    // Anthropic's prompt-cache rule: when a request body carries cache_control
    // markers with mixed TTLs, every `ttl: "1h"` block must precede every
    // `ttl: "5m"` block in API document order (system → tools → messages,
    // and within each message its content array). On 2026-05-07 a real
    // 400 surfaced because the assembled body had a 5m marker on a user
    // turn ahead of a 1h marker further back in messages. Existing tests
    // checked orientation idempotency and the cache_control budget in
    // isolation; none walked the FINAL body produced by `apply_rebuild` +
    // `append_kernel_orientation_to_system`. This test does.

    /// Walk a request body in API document order and return the TTL string
    /// of every `cache_control` block encountered. Block without `ttl` is
    /// recorded as `"<unset>"` so reviewers can see structural slots even
    /// when TTL is missing. Order: system blocks (in array order) → tools
    /// (in array order) → messages (in order; each message's content
    /// array in order).
    fn collect_ttls_in_doc_order(req: &Value) -> Vec<String> {
        let mut out = Vec::new();
        let mut visit = |v: &Value| {
            if let Some(cc) = v.get("cache_control") {
                let ttl = cc
                    .get("ttl")
                    .and_then(|t| t.as_str())
                    .unwrap_or("<unset>")
                    .to_string();
                out.push(ttl);
            }
        };
        if let Some(arr) = req.get("system").and_then(|s| s.as_array()) {
            for block in arr {
                visit(block);
            }
        }
        if let Some(arr) = req.get("tools").and_then(|t| t.as_array()) {
            for tool in arr {
                visit(tool);
            }
        }
        if let Some(arr) = req.get("messages").and_then(|m| m.as_array()) {
            for msg in arr {
                visit(msg);
                if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                    for block in content {
                        visit(block);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn assembled_request_ttl_1h_precedes_5m_after_rebuild_and_orientation() {
        // Claude-code-style request: stable system block (1h), tool def
        // (1h), prior turn pair, current user with the volatile 5m
        // marker (mirrors how claude-code marks the live edge).
        let mut req = json!({
            "model": "claude-opus-4-7",
            "system": [
                {
                    "type": "text",
                    "text": "You are Claude Code...",
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ],
            "tools": [
                {
                    "name": "read",
                    "description": "Read a file",
                    "input_schema": {"type": "object"},
                    "cache_control": {"type": "ephemeral", "ttl": "1h"}
                }
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "earlier turn"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
                {"role": "user", "content": [
                    {
                        "type": "text",
                        "text": "current request",
                        "cache_control": {"type": "ephemeral", "ttl": "5m"}
                    }
                ]}
            ]
        });

        let config = RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: 18,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        };
        let outcome = apply_rebuild(&mut req, &config);
        assert!(
            matches!(outcome, RebuildOutcome::Applied(_)),
            "rebuild must apply against this fixture; got {:?}",
            outcome
        );
        let appended = append_kernel_orientation_to_system(&mut req, "1h");
        assert!(appended, "orientation block must append to fixture");

        let ttls = collect_ttls_in_doc_order(&req);

        // Sanity: we should still see at least one 1h marker (orientation
        // and/or the original system block) and the 5m marker on the
        // current user turn.
        assert!(
            ttls.iter().any(|t| t == "1h"),
            "expected at least one ttl=1h marker; got {:?}",
            ttls
        );
        assert!(
            ttls.iter().any(|t| t == "5m"),
            "expected the volatile ttl=5m marker; got {:?}",
            ttls
        );

        // Total markers must remain at or under Anthropic's hard limit.
        assert!(
            ttls.len() <= ANTHROPIC_CACHE_CONTROL_LIMIT,
            "assembled body must not exceed the {}-block cache_control limit; got {}: {:?}",
            ANTHROPIC_CACHE_CONTROL_LIMIT,
            ttls.len(),
            ttls
        );

        // The invariant: in API document order, the LAST 1h must come
        // BEFORE the FIRST 5m. A 5m marker ahead of any 1h marker is
        // exactly the shape that produced the 2026-05-07 400.
        verify_ttl_ordering(&ttls).expect("ttl ordering invariant must hold for assembled body");
    }

    /// Verify that no `ttl=5m` marker precedes any `ttl=1h` marker in
    /// document order. Returns Ok when both kinds are absent, when only
    /// one kind is present, or when 1h precedes 5m. Returns Err with a
    /// diagnostic when the invariant is violated.
    ///
    /// Extracted from the positive-case test so the negative-case fixture
    /// (→1842) can call it directly and assert `.is_err()`. Without the
    /// extraction the original `if let (Some, Some)` guard silently
    /// passed bodies that lacked one TTL kind, which a future regression
    /// in `collect_ttls_in_doc_order` could exploit unnoticed.
    fn verify_ttl_ordering(ttls: &[String]) -> Result<(), String> {
        let last_1h = ttls.iter().rposition(|t| t == "1h");
        let first_5m = ttls.iter().position(|t| t == "5m");
        match (last_1h, first_5m) {
            (Some(last_1h), Some(first_5m)) if last_1h >= first_5m => Err(format!(
                "ttl=1h must precede ttl=5m in document order \
                 (last 1h at {last_1h}, first 5m at {first_5m}); \
                 ttls in order: {ttls:?}"
            )),
            _ => Ok(()),
        }
    }

    // ── →1842: negative fixture for the TTL ordering invariant ─────────────
    //
    // The positive test above uses a guard that returns Ok when only one
    // TTL kind is present. These negative + guard-behavior tests lock the
    // helper's contract so a future regression that quietly drops one
    // kind from the assembled body cannot pass `verify_ttl_ordering`
    // unnoticed.

    #[test]
    fn ttl_ordering_rejects_5m_before_1h() {
        // Inverted document order: 5m on the system block (would land
        // first in collect_ttls_in_doc_order), 1h further back in
        // messages. This is exactly the shape the 2026-05-07 400
        // reported. The helper must flag it.
        let req = json!({
            "system": [
                {"type": "text", "text": "sys",
                 "cache_control": {"type": "ephemeral", "ttl": "5m"}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "later",
                     "cache_control": {"type": "ephemeral", "ttl": "1h"}}
                ]}
            ]
        });
        let ttls = collect_ttls_in_doc_order(&req);
        assert_eq!(ttls, vec!["5m".to_string(), "1h".to_string()]);
        let result = verify_ttl_ordering(&ttls);
        assert!(
            result.is_err(),
            "5m preceding 1h must be rejected; got {:?}",
            result
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains("ttl=1h must precede ttl=5m"),
            "diagnostic must name the invariant; got {msg:?}"
        );
    }

    #[test]
    fn ttl_ordering_passes_when_only_one_kind_present() {
        // Lock the guard: bodies with only 1h or only 5m (or none) are
        // valid — the invariant only applies when both kinds coexist.
        assert!(verify_ttl_ordering(&[]).is_ok(), "empty must pass");
        assert!(
            verify_ttl_ordering(&["1h".to_string(), "1h".to_string()]).is_ok(),
            "all-1h must pass"
        );
        assert!(
            verify_ttl_ordering(&["5m".to_string(), "5m".to_string()]).is_ok(),
            "all-5m must pass"
        );
        assert!(
            verify_ttl_ordering(&["<unset>".to_string(), "1h".to_string()]).is_ok(),
            "unset markers must not interfere"
        );
    }

    #[test]
    fn ttl_ordering_passes_when_1h_precedes_5m() {
        // The healthy shape — 1h on system, 5m on the live edge.
        let ttls = vec!["1h".to_string(), "1h".to_string(), "5m".to_string()];
        assert!(verify_ttl_ordering(&ttls).is_ok(), "1h..5m must pass");
    }

    #[test]
    fn ttl_ordering_rejects_5m_at_same_index_as_1h() {
        // Edge: same-index pair — last_1h == first_5m would only happen
        // if they're the same marker (impossible in practice) but the
        // helper must still treat `>=` as a violation, not a pass.
        // Construct a synthetic vec to lock the boundary.
        let ttls = vec!["5m".to_string(), "1h".to_string(), "5m".to_string()];
        // last_1h = 1, first_5m = 0 → 1 >= 0 → Err.
        assert!(verify_ttl_ordering(&ttls).is_err());
    }

    #[test]
    fn test_tier_c_drops_unreferenced_tools() {
        let mut value = serde_json::json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "use tool1"},
                        {"type": "tool_use", "id": "u1", "name": "tool1", "input": {}}
                    ]
                }
            ],
            "tools": [
                {"name": "tool1", "description": "desc1", "input_schema": {}},
                {"name": "tool2", "description": "desc2", "input_schema": {}}
            ]
        });

        let mut report = ReductionReport::default();
        drop_unreferenced_tools(&mut value, &mut report);

        let tools = value["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "tool1");
        assert_eq!(report.tier_c_tools_dropped, 1);
    }

    // ── →1979 K3+K4 — unresolved frontier + paging rebudget ──────────

    /// Helper: a rebuild config with the given paging cap.
    fn k_config(cap: usize) -> RebuildConfig {
        RebuildConfig {
            enabled: true,
            mode_tag: "rebuild_local".into(),
            max_native_tool_summary: cap,
            max_user_intent_thread: 10,
            live_envelope_override: None,
            transcript_tail_summary: None,
            recent_assistant_digests: None,
            templates_summary: None,
            prior_user_turns_override: None,
            body_bytes_in: None,
            compact_target_bytes: None,
        }
    }

    /// AC-3 (→1979 K4): defaults rebudgeted 50→18 on both constructors.
    #[test]
    fn k4_default_paging_cap_is_18() {
        assert_eq!(RebuildConfig::from_env().max_native_tool_summary, 18);
        use clap::Parser;
        let cli = crate::config::CliArgs::parse_from(["ostk-cache"]);
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config::resolve(cli, tmp.path());
        assert_eq!(
            RebuildConfig::from_resolved(&cfg).max_native_tool_summary,
            18
        );
    }

    /// AC-1 (→1979 K3): all three frontier kinds — pending tool_use,
    /// armed monitor, held lock — render under `## Unresolved`.
    #[test]
    fn k3_unresolved_section_lists_all_frontier_kinds() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "start"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_mon", "name": "Monitor",
                     "input": {"description": "watch deploy log", "command": "tail -f x", "timeout_ms": 1000, "persistent": true}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_mon", "content": "armed"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_lock", "name": "mcp__ostk__lock",
                     "input": {"action": "create", "name": "lane-1979-p89507"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_lock", "content": "created"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_dead", "name": "Bash", "input": {"command": "cargo test"}}
                ]},
                // no tool_result for tu_dead — cycle died mid-chain
                {"role": "user", "content": "new turn"}
            ]
        });
        let outcome = apply_rebuild(&mut req, &k_config(18));
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let synthetic = req["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(synthetic.contains("## Unresolved"), "missing section:\n{synthetic}");
        assert!(synthetic.contains("in-flight tool_use"), "pending call not surfaced");
        assert!(synthetic.contains("armed monitor"), "monitor not surfaced");
        assert!(synthetic.contains("watch deploy log"), "monitor description lost");
        assert!(
            synthetic.contains("lock held: lane-1979-p89507"),
            "lane claim not surfaced"
        );
    }

    /// AC-2 (→1979 K3): a pending tool_use buried deeper than the
    /// paging cap still surfaces — the frontier scan is uncapped. This
    /// is the survival property: recomputation each cycle means the
    /// item re-emits after cycle death and after compaction shrinks
    /// the paged window.
    #[test]
    fn k3_unresolved_survives_past_paging_cap() {
        let mut messages = vec![
            json!({"role": "user", "content": "start"}),
            // the frontier item: pending tool_use, oldest in the window
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_orphan", "name": "Bash",
                 "input": {"command": "deploy --prod"}}
            ]}),
        ];
        // bury it under 30 resolved calls (cap will be 5)
        for i in 0..30 {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": format!("tu_{i}"), "name": "Read",
                 "input": {"file_path": format!("/tmp/f{i}")}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("tu_{i}"), "content": "ok"}
            ]}));
        }
        messages.push(json!({"role": "user", "content": "new turn"}));
        let mut req = json!({ "messages": messages });
        let outcome = apply_rebuild(&mut req, &k_config(5));
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let synthetic = req["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(
            synthetic.contains("deploy --prod"),
            "pending call evicted by paging cap — frontier must be exempt:\n{synthetic}"
        );
        // and the paged section honored its cap: tu_0..tu_24 dropped
        assert!(!synthetic.contains("/tmp/f0\""), "paging cap not applied");
    }

    /// AC-1 negative: clean window (everything resolved, locks
    /// released) → no `## Unresolved` section at all.
    #[test]
    fn k3_no_unresolved_section_when_frontier_empty() {
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "start"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_l", "name": "mcp__ostk__lock",
                     "input": {"action": "create", "name": "lane-x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_l", "content": "created"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_r", "name": "mcp__ostk__lock",
                     "input": {"action": "release", "name": "lane-x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_r", "content": "released"}
                ]},
                {"role": "user", "content": "new turn"}
            ]
        });
        let outcome = apply_rebuild(&mut req, &k_config(18));
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let synthetic = req["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(
            !synthetic.contains("## Unresolved"),
            "create→release pair must cancel:\n{synthetic}"
        );
    }

    /// AC-4 (→1979 KEEP): error bodies stay inline verbatim under the
    /// rebudgeted cap — regression guard on the unanimously
    /// load-bearing policy.
    #[test]
    fn k4_error_bodies_remain_inline_verbatim() {
        let err_body = "error[E0308]: mismatched types\n --> src/main.rs:7:9";
        let mut req = json!({
            "messages": [
                {"role": "user", "content": "start"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_e", "name": "Bash", "input": {"command": "cargo build"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_e", "content": err_body, "is_error": true}
                ]},
                {"role": "user", "content": "new turn"}
            ]
        });
        let outcome = apply_rebuild(&mut req, &k_config(18));
        assert!(matches!(outcome, RebuildOutcome::Applied(_)));
        let synthetic = req["messages"][0]["content"][0]["text"].as_str().unwrap();
        // Rendering indents continuation lines by two spaces inside the
        // fence; assert per-line presence (the policy is "body inline",
        // not byte-identical whitespace).
        assert!(
            synthetic.contains("error[E0308]: mismatched types"),
            "error body first line must be inline:\n{synthetic}"
        );
        assert!(
            synthetic.contains("--> src/main.rs:7:9"),
            "error body continuation line must be inline:\n{synthetic}"
        );
    }

    /// AC-5 (→1979 K4): budget-delta receipt. Replays a real captured
    /// request body through apply_rebuild at cap 50 (old) vs 18 (new)
    /// and prints reclaimed bytes. Ignored by default — needs a capture
    /// path: `OSTK_CAPTURE_BODY=<path>/request-in.body cargo test
    /// k4_budget_delta -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn k4_budget_delta_on_capture() {
        let path = match std::env::var("OSTK_CAPTURE_BODY") {
            Ok(p) => p,
            Err(_) => {
                eprintln!("OSTK_CAPTURE_BODY not set; skipping");
                return;
            }
        };
        let raw = std::fs::read_to_string(&path).expect("read capture body");
        let body: Value = serde_json::from_str(&raw).expect("parse capture body");

        let mut sizes = Vec::new();
        for cap in [50usize, 18] {
            let mut req = body.clone();
            match apply_rebuild(&mut req, &k_config(cap)) {
                RebuildOutcome::Applied(report) => sizes.push((cap, report.bytes_out)),
                other => panic!("expected Applied at cap {cap}, got {other:?}"),
            }
        }
        let (_, old_bytes) = sizes[0];
        let (_, new_bytes) = sizes[1];
        eprintln!(
            "K4 budget delta on {path}: cap50={old_bytes}b cap18={new_bytes}b reclaimed={}b ({:.1}%)",
            old_bytes as i64 - new_bytes as i64,
            100.0 * (old_bytes as f64 - new_bytes as f64) / old_bytes as f64
        );
        assert!(new_bytes <= old_bytes, "rebudget must not grow the synthetic");
    }
}
