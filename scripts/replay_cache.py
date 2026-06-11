#!/usr/bin/env python3
"""replay_cache.py — provider-keyed offline cache replay / verification harness.

PORT of haystack/scripts/replay_2032.py, which is FROZEN as the →2032
verification artifact (decision replay_2032_ac1_results) — do not modify
that file; this harness generalizes it (leader ruling, 2026-06-11).

Providers:

  anthropic   Replay Claude Code transcripts through the →2032 WARM/DEAD
              lane machine (policy SIMULATION). Explicit-write market:
              breakpoints chosen, write premium 5m=1.25x / 1h=2.00x,
              reads 0.10x. ACs: AC-1 write/call <= 4800, AC-3 hit >= 90%,
              AC-4 cold writes under cap.

  gpt         Scan an ostk-cache http-capture corpus (codex /responses
              wire). OBSERVATIONAL — there is no write policy to
              simulate: caching is automatic prefix-match, no
              breakpoints, no write premium (codex-2034-review §48-55;
              live-wire confirmed 2026-06-11). AC: AC-G1 warm-turn hit
              ratio >= 90% (observed baseline 96–98.5%), SEGMENTED per
              spec §7.4: lanes (prompt_cache_key) split at compaction
              boundaries (input collapse >50% under the same key);
              segment-opening turns are excluded from warm-hit, server
              transients are INCLUDED (they are the real-world rate),
              and each cold/dip turn carries a loss_class annotation
              {cold_start, structural_reset, server_transient,
              persistent_dip, client_churn, unresolved}. Dips are
              classed by EVIDENCE first (byte-diff of request bodies vs
              the previous usage-bearing turn settles server vs client
              definitionally); shape heuristics apply only when bodies
              are absent — there a dip persisting >=2 turns is the
              alert class (would-be client churn, §7.4-ii), and a dip
              with no recovery evidence (corpus end / reset boundary)
              is unresolved, never persistent_dip.
              Pricing is wire-keyed (spec §7.1 verdict):
                --gpt-wire oauth (default): NO dollar figures — AC-G2b
                  forbids $-ACs on oauth seats (no public evidence cached
                  input reduces plan credits); single retention tier;
                  token ratios only.
                --gpt-wire platform: cached/uncached dollar model from
                  PROVISIONAL rates (CLI-overridable) — gates nothing
                  until an API-key seat exists.

Cross-provider normalization (provider_policy::UsageSnapshot, A2):
  Anthropic `input_tokens` is cache-EXCLUSIVE (input + cache_read +
  cache_write = full prompt). GPT `input_tokens` is cache-INCLUSIVE
  (cached_tokens ⊆ input_tokens). All GPT uncached-input figures here
  are `input_tokens - cached_tokens`.
"""

import argparse
import glob
import json
import os
import re
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

FIVE_MIN = timedelta(minutes=5)
ONE_HOUR = timedelta(hours=1)

# ---------------------------------------------------------------------------
# Pricing models (provider-keyed)
# ---------------------------------------------------------------------------

# Anthropic: (input_per_mtok, output_per_mtok); longest prefix wins.
# Ported verbatim from haystack/scripts/cache_truth.py (frozen reference).
ANTHROPIC_RATES = {
    "claude-opus": (15.00, 75.00),
    "claude-fable": (10.00, 50.00),  # also fallback
    "claude-sonnet": (3.00, 15.00),
    "claude-haiku": (1.00, 5.00),
}
ANTHROPIC_FALLBACK = "claude-fable"
ANTHROPIC_WRITE_5M = 1.25  # x input rate
ANTHROPIC_WRITE_1H = 2.00
ANTHROPIC_READ = 0.10

# GPT platform (api.openai.com, API-key seats): PROVISIONAL defaults,
# CLI-overridable. Never used on the oauth wire (AC-G2b).
GPT_PLATFORM_DEFAULTS = {
    "input_per_mtok": 1.25,
    "cached_per_mtok": 0.125,
    "output_per_mtok": 10.00,
}

# Spec §7.4 loss-taxonomy thresholds (CLI-overridable; gpt mode only).
GPT_RESET_COLLAPSE = 0.50  # input < prev_input * this (same key) => structural_reset
GPT_DIP_RATIO = 0.90  # non-opening turn hit ratio below this => dip


def anthropic_rates(model: str) -> tuple[float, float]:
    best, best_len = None, -1
    for key in ANTHROPIC_RATES:
        if model.startswith(key) and len(key) > best_len:
            best, best_len = key, len(key)
    if best is None:
        print(
            f"WARNING: unknown model '{model}', using {ANTHROPIC_FALLBACK} rates",
            file=sys.stderr,
        )
        best = ANTHROPIC_FALLBACK
    return ANTHROPIC_RATES[best]


def _spend(tokens: int, per_mtok: float) -> float:
    return tokens * per_mtok / 1_000_000


def anthropic_spend(inp, cache_r, cw_5m, cw_1h, out, in_rate, out_rate) -> dict:
    s_inp = _spend(inp, in_rate)
    s_w5 = _spend(cw_5m, in_rate * ANTHROPIC_WRITE_5M)
    s_w1 = _spend(cw_1h, in_rate * ANTHROPIC_WRITE_1H)
    s_read = _spend(cache_r, in_rate * ANTHROPIC_READ)
    s_out = _spend(out, out_rate)
    return {
        "input": s_inp,
        "write": s_w5 + s_w1,
        "write_5m": s_w5,
        "write_1h": s_w1,
        "read": s_read,
        "output": s_out,
        "total": s_inp + s_w5 + s_w1 + s_read + s_out,
    }


def parse_ts(s: str) -> datetime:
    dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


# ---------------------------------------------------------------------------
# Anthropic lane: transcript extraction + →2032 policy replay (port)
# ---------------------------------------------------------------------------


def extract_anthropic_calls(path: str, since=None, until=None) -> list[dict]:
    """Per-call records from one Claude transcript, deduped by message.id."""
    seen, calls = set(), []
    with open(path, "r", encoding="utf-8") as fh:
        for raw in fh:
            raw = raw.strip()
            if not raw:
                continue
            try:
                obj = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if obj.get("type") != "assistant" or not obj.get("message"):
                continue
            msg = obj["message"]
            try:
                ts = parse_ts(obj.get("timestamp", ""))
            except ValueError:
                continue
            if (since and ts < since) or (until and ts >= until):
                continue
            mid = msg.get("id", "")
            if mid:
                if mid in seen:
                    continue
                seen.add(mid)
            usage = msg.get("usage", {})
            cc = usage.get("cache_creation")
            if isinstance(cc, dict) and (
                "ephemeral_5m_input_tokens" in cc or "ephemeral_1h_input_tokens" in cc
            ):
                cw_5m = cc.get("ephemeral_5m_input_tokens", 0) or 0
                cw_1h = cc.get("ephemeral_1h_input_tokens", 0) or 0
            else:
                cw_5m = usage.get("cache_creation_input_tokens", 0)
                cw_1h = 0
            calls.append(
                {
                    "session": Path(path).stem,
                    "model": msg.get("model", "unknown"),
                    "ts": ts,
                    "inp": usage.get("input_tokens", 0),
                    "cache_r": usage.get("cache_read_input_tokens", 0),
                    "cw_5m": cw_5m,
                    "cw_1h": cw_1h,
                    "out": usage.get("output_tokens", 0),
                }
            )
    return calls


def forecast_ttl(gaps: list[timedelta]) -> str:
    """Median of trailing 5 gaps; empty → 5m; >60m → going cold, 5m."""
    if not gaps:
        return "5m"
    tail = sorted(gaps[-5:])
    med = tail[len(tail) // 2]
    if med < FIVE_MIN:
        return "5m"
    if med <= ONE_HOUR:
        return "1h"
    return "5m"


def replay_anthropic(calls: list[dict], compact: float, min_prefix: int) -> dict:
    """→2032 WARM/DEAD state machine over the observed call stream.

    Mirrors write_policy::decide (Rust) and the frozen replay exactly.
    """
    lanes: dict = {}
    sim = {"calls": 0, "inp": 0, "cache_r": 0, "cache_w_5m": 0, "cache_w_1h": 0, "out": 0}
    per_model: dict = {}
    cold_writes = []
    warm = dead = 0

    for c in sorted(calls, key=lambda c: c["ts"]):
        key = (c["session"], c["model"])
        lane = lanes.setdefault(
            key, {"S": 0, "C": 0, "P_obs": 0, "ttl": FIVE_MIN, "last_ts": None, "gaps": []}
        )
        P = c["inp"] + c["cache_r"] + c["cw_5m"] + c["cw_1h"]
        growth = P - lane["P_obs"] if lane["last_ts"] is not None else P
        if growth < 0:
            lane["S"] = min(lane["S"], int(lane["S"] * P / max(lane["P_obs"], 1)))
            lane["C"] = min(lane["C"], lane["S"])
            growth = 0
        lane["P_obs"] = P
        lane["S"] += growth

        gap = (c["ts"] - lane["last_ts"]) if lane["last_ts"] else None
        if gap is not None:
            lane["gaps"].append(gap)

        if lane["C"] > 0 and gap is not None and gap < lane["ttl"]:
            warm += 1
            read, write = lane["C"], lane["S"] - lane["C"]
        else:
            dead += 1
            lane["S"] = max(min_prefix, int(lane["S"] * compact)) if lane["S"] else 0
            read, write = 0, lane["S"]
            cold_writes.append((write, c["session"], c["ts"].isoformat()))
        tier = forecast_ttl(lane["gaps"])
        lane["ttl"] = FIVE_MIN if tier == "5m" else ONE_HOUR
        lane["C"] = lane["S"]
        lane["last_ts"] = c["ts"]

        for stats in (sim, per_model.setdefault(c["model"], dict.fromkeys(sim, 0))):
            stats["calls"] += 1
            stats["inp"] += c["inp"]
            stats["cache_r"] += read
            stats["cache_w_5m" if tier == "5m" else "cache_w_1h"] += write
            stats["out"] += c["out"]

    sim["warm"], sim["dead"] = warm, dead
    sim["per_model"] = per_model
    sim["cold_writes"] = sorted(cold_writes, reverse=True)
    return sim


def run_anthropic(args) -> dict:
    calls = []
    for f in sorted(glob.glob(os.path.join(args.dir, "*.jsonl"))):
        calls.extend(extract_anthropic_calls(f, args.since, args.until))
    if not calls:
        sys.exit("no anthropic calls in window")

    def aggregate(rows):
        agg = {"calls": 0, "inp": 0, "cache_r": 0, "cache_w_5m": 0, "cache_w_1h": 0, "out": 0}
        pm: dict = {}
        for c in rows:
            for stats in (agg, pm.setdefault(c["model"], dict.fromkeys(agg, 0))):
                stats["calls"] += 1
                stats["inp"] += c["inp"]
                stats["cache_r"] += c["cache_r"]
                stats["cache_w_5m"] += c["cw_5m"]
                stats["cache_w_1h"] += c["cw_1h"]
                stats["out"] += c["out"]
        return agg, pm

    base, base_pm = aggregate(calls)
    sim = replay_anthropic(calls, args.compact, args.min_prefix)

    def spend_total(per_model):
        total = dict.fromkeys(("input", "write", "write_5m", "write_1h", "read", "output", "total"), 0.0)
        for model, ms in per_model.items():
            ir, orate = anthropic_rates(model)
            sp = anthropic_spend(
                ms["inp"], ms["cache_r"], ms["cache_w_5m"], ms["cache_w_1h"], ms["out"], ir, orate
            )
            for k in total:
                total[k] += sp[k]
        return total

    def summarize(stats, per_model):
        w = stats["cache_w_5m"] + stats["cache_w_1h"]
        r = stats["cache_r"]
        sp = spend_total(per_model)
        return {
            "calls": stats["calls"],
            "cache_read": r,
            "cache_write": w,
            "cache_write_5m": stats["cache_w_5m"],
            "cache_write_1h": stats["cache_w_1h"],
            "hit_ratio": r / (r + w) if r + w else 0.0,
            "write_per_call": w // stats["calls"],
            "spend": sp,
            "write_share": sp["write"] / sp["total"] if sp["total"] else 0.0,
        }

    base_sum, sim_sum = summarize(base, base_pm), summarize(sim, sim["per_model"])
    worst = sim["cold_writes"][0] if sim["cold_writes"] else (0, None, None)
    # Key set and insertion order mirror the frozen artifact EXACTLY —
    # no provider key here — so anthropic-mode JSON is byte-identical
    # (pinned by scripts/check_replay_equivalence.py).
    return {
        "window": {
            "since": args.since.isoformat() if args.since else None,
            "until": args.until.isoformat() if args.until else None,
            "dir": args.dir,
        },
        "params": {"compact": args.compact, "min_prefix": args.min_prefix, "cold_cap": args.cold_cap},
        "baseline": base_sum,
        "replay": sim_sum,
        "replay_warm_calls": sim["warm"],
        "replay_dead_calls": sim["dead"],
        "worst_cold_write": {"tokens": worst[0], "session": worst[1], "ts": worst[2]},
        "ac": {
            "ac1_write_per_call_le_4800": sim_sum["write_per_call"] <= 4800,
            "ac3_hit_ratio_ge_90": sim_sum["hit_ratio"] >= 0.90,
            "ac4_cold_writes_under_cap": worst[0] <= args.cold_cap,
        },
    }


# ---------------------------------------------------------------------------
# GPT lane: http-capture corpus scan (observational)
# ---------------------------------------------------------------------------

USAGE_RE = re.compile(r'"usage":\s*({(?:[^{}]|{[^{}]*})*})')


def extract_gpt_calls(capture_dir: str) -> tuple[list[dict], int]:
    """Per-call records from an ostk-cache http-capture tree.

    Layout: <dir>/<bucket>/<entry>/{metadata.json,request-out.body,
    response.body}. Usage comes from the FINAL "usage" block in the SSE
    stream (the `response.completed` event); entries without one (failed
    or truncated streams) are counted and skipped.
    """
    calls, skipped = [], 0
    for meta_path in sorted(glob.glob(os.path.join(capture_dir, "*", "*", "metadata.json"))):
        entry = os.path.dirname(meta_path)
        try:
            meta = json.load(open(meta_path))
        except (OSError, json.JSONDecodeError):
            skipped += 1
            continue
        try:
            body = open(os.path.join(entry, "response.body"), errors="replace").read()
        except OSError:
            skipped += 1
            continue
        hits = USAGE_RE.findall(body)
        if not hits:
            skipped += 1
            continue
        try:
            usage = json.loads(hits[-1])
            req = json.load(open(os.path.join(entry, "request-out.body")))
        except (OSError, json.JSONDecodeError):
            skipped += 1
            continue
        inp = usage.get("input_tokens", 0)
        calls.append(
            {
                "entry": os.path.basename(entry),
                "_dir": entry,  # retained for §7.4 evidence classification
                "session": meta.get("session", "?"),
                "ts": meta.get("ts"),
                "model": req.get("model", "unknown"),
                "cache_key": req.get("prompt_cache_key"),
                "input": inp,  # cache-INCLUSIVE (A2 normalization)
                "cached": usage.get("input_tokens_details", {}).get("cached_tokens", 0),
                "output": usage.get("output_tokens", 0),
            }
        )
    return calls, skipped


def _gpt_evidence_class(prev_dir, cur_dir):
    """Byte-evidence dip classification (§7.4 preferred path).

    When request bodies exist for the dip turn and its predecessor, the
    append-only invariant — stable top-level fields (model,
    instructions, prompt_cache_key) AND the prior input array an exact
    prefix of the current one — proves the client resent a byte-stable
    shared prefix, so the dip is server-side: server_transient
    DEFINITIONALLY. (This is the same byte-diff that classed #14, #43
    and #54 — including #43, which is shape-unresolvable because its
    next usage-bearing turn is post-compaction.) A violated invariant
    is mid-prefix mutation: client_churn (§7.4a — never yet observed;
    a zero count is itself a receipt). Returns None when evidence is
    unavailable, falling back to shape heuristics.
    """
    try:
        prev_req = json.load(open(os.path.join(prev_dir, "request-out.body")))
        cur_req = json.load(open(os.path.join(cur_dir, "request-out.body")))
    except (OSError, json.JSONDecodeError):
        return None
    prev_in, cur_in = prev_req.get("input"), cur_req.get("input")
    if not isinstance(prev_in, list) or not isinstance(cur_in, list):
        return None
    stable = all(
        prev_req.get(k) == cur_req.get(k)
        for k in ("model", "instructions", "prompt_cache_key")
    )
    if stable and cur_in[: len(prev_in)] == prev_in:
        return "server_transient"
    return "client_churn"


def classify_gpt_lanes(calls, reset_collapse, dip_ratio):
    """Spec §7.4 loss taxonomy over the capture stream.

    Groups calls into lanes (prompt_cache_key, falling back to capture
    session), orders each lane by timestamp, splits segments at
    compaction boundaries (input collapse: cur < prev * reset_collapse
    under the same key — §7.4c, codex auto-compact 215,818→26,175), and
    annotates every turn in place:

      lane          int   lane index
      segment       int   lane-local segment index
      segment_open  bool  first turn of a segment (excluded from warm-hit)
      loss_class    str|None
          cold_start        lane's first observed turn, zero cached
          structural_reset  compaction boundary (excluded from warm-hit)
          server_transient  byte-evidence append-only dip (§7.4b), or
                            shape: single dip with observed recovery
                            (INCLUDED in warm-hit)
          client_churn      byte-evidence mid-prefix mutation (§7.4a)
          persistent_dip    shape: dip run >=2 usage-bearing turns —
                            alert class, §7.4-ii (INCLUDED in warm-hit)
          unresolved        shape: single dip with NO recovery evidence
                            (reset boundary or corpus end terminates the
                            window) — never feeds the alert class

    Dips are classed by EVIDENCE first (_gpt_evidence_class); shape
    runs only cover evidence-less dips. Recovery windows are counted in
    usage-bearing turns (no-usage capture entries never reach here).

    Returns (lane_count, segment_count, loss_class_counts).
    """
    lanes: dict = {}
    for c in calls:
        lanes.setdefault(c["cache_key"] or c["session"], []).append(c)
    seg_total = 0
    loss = dict.fromkeys(
        (
            "cold_start",
            "structural_reset",
            "server_transient",
            "persistent_dip",
            "client_churn",
            "unresolved",
        ),
        0,
    )
    for li, key in enumerate(sorted(lanes)):
        turns = lanes[key]
        turns.sort(key=lambda c: (c["ts"] or "", c["entry"]))
        seg, prev = 0, None
        for c in turns:
            c["lane"], c["loss_class"] = li, None
            if prev is None:
                seg_total += 1
                c["segment_open"] = True
                if c["cached"] == 0:
                    c["loss_class"] = "cold_start"
            elif c["input"] < prev["input"] * reset_collapse:
                seg += 1
                seg_total += 1
                c["segment_open"] = True
                c["loss_class"] = "structural_reset"
            else:
                c["segment_open"] = False
            c["segment"] = seg
            c["_dip"] = (
                not c["segment_open"]
                and (c["cached"] / c["input"] if c["input"] else 0.0) < dip_ratio
            )
            if c["_dip"] and prev is not None:
                # Evidence first: byte-diff vs the previous usage-bearing
                # turn settles server vs client definitionally.
                c["loss_class"] = _gpt_evidence_class(prev["_dir"], c["_dir"])
            prev = c
        # Shape fallback over evidence-less dips. A run flushed by a
        # HEALTHY non-opening turn has observed recovery: 1 turn =>
        # server_transient, >=2 => persistent_dip. A run flushed without
        # recovery evidence (structural_reset boundary, corpus end, or
        # an adjacent evidence-classed dip): single => unresolved (don't
        # let corpus truncation feed the alert class), >=2 =>
        # persistent_dip (persistence observed within the corpus).
        run = []
        for c in turns + [None]:  # None flushes the trailing run
            if c is not None and c["_dip"] and c["loss_class"] is None:
                run.append(c)
                continue
            recovered = c is not None and not c["segment_open"] and not c["_dip"]
            cls = (
                "persistent_dip"
                if len(run) >= 2
                else ("server_transient" if recovered else "unresolved")
            )
            for r in run:
                r["loss_class"] = cls
            run = []
        for c in turns:
            c.pop("_dip", None)
            if c["loss_class"]:
                loss[c["loss_class"]] += 1
    return len(lanes), seg_total, loss


def run_gpt(args) -> dict:
    calls, skipped = extract_gpt_calls(args.capture_dir)
    if not calls:
        sys.exit(f"no usable gpt captures in {args.capture_dir} ({skipped} skipped)")

    n_lanes, n_segments, loss = classify_gpt_lanes(
        calls, args.gpt_reset_collapse, args.gpt_dip_ratio
    )
    # §7.4 segmentation: warm set = every non-segment-opening turn.
    # Lane starts + structural resets are excluded; server transients
    # and persistent dips stay IN — they are the real-world hit rate.
    warm = [c for c in calls if not c["segment_open"]]
    opens = [c for c in calls if c["segment_open"]]
    t_in = sum(c["input"] for c in calls)
    t_cached = sum(c["cached"] for c in calls)
    t_out = sum(c["output"] for c in calls)
    w_in = sum(c["input"] for c in warm)
    w_cached = sum(c["cached"] for c in warm)
    warm_hit = w_cached / w_in if w_in else 0.0

    report = {
        "provider": "gpt",
        "wire": args.gpt_wire,
        "window": {"capture_dir": args.capture_dir},
        "calls": len(calls),
        "skipped_no_usage": skipped,
        "lanes": n_lanes,
        "segments": n_segments,
        "warm_turns": len(warm),
        "segment_open_turns": len(opens),
        "loss_classes": loss,
        "input_tokens_inclusive": t_in,
        "cached_tokens": t_cached,
        "uncached_input_tokens": t_in - t_cached,  # A2: input is cache-inclusive
        "output_tokens": t_out,
        "overall_cached_share": t_cached / t_in if t_in else 0.0,
        "warm_turn_hit_ratio": warm_hit,
        "per_turn": [
            {
                "entry": c["entry"],
                "model": c["model"],
                "lane": c["lane"],
                "segment": c["segment"],
                "input": c["input"],
                "cached": c["cached"],
                "hit": c["cached"] / c["input"] if c["input"] else 0.0,
                "loss_class": c["loss_class"],
            }
            for c in calls
        ],
        "ac": {"ac_g1_warm_hit_ge_90": warm_hit >= 0.90},
    }

    if args.gpt_wire == "platform":
        # PROVISIONAL platform dollars — gates nothing until an API-key
        # seat exists; CLI-overridable.
        uncached = t_in - t_cached
        sp_unc = _spend(uncached, args.gpt_input_rate)
        sp_c = _spend(t_cached, args.gpt_cached_rate)
        sp_o = _spend(t_out, args.gpt_output_rate)
        report["spend_provisional"] = {
            "uncached_input": sp_unc,
            "cached_input": sp_c,
            "output": sp_o,
            "total": sp_unc + sp_c + sp_o,
            "rates": {
                "input_per_mtok": args.gpt_input_rate,
                "cached_per_mtok": args.gpt_cached_rate,
                "output_per_mtok": args.gpt_output_rate,
            },
        }
    else:
        # AC-G2b: oauth seats get NO dollar figures — single retention
        # tier, keep-warm cadence is the liveness lever (§7.1 verdict).
        report["spend"] = None
        report["spend_note"] = "oauth wire: $-ACs forbidden (AC-G2b); token ratios only"
    return report


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main() -> None:
    ap = argparse.ArgumentParser(description="Provider-keyed offline cache replay harness")
    ap.add_argument("--provider", choices=("anthropic", "gpt"), required=True)
    ap.add_argument("--json", action="store_true")
    # anthropic
    ap.add_argument("--dir", default=os.path.expanduser("~/.claude/projects/-Users-scottmeyer-projects-haystack"))
    ap.add_argument("--since", type=parse_ts, default=None)
    ap.add_argument("--until", type=parse_ts, default=None)
    ap.add_argument("--compact", type=float, default=150 / 540)
    ap.add_argument("--min-prefix", type=int, default=25_000)
    ap.add_argument("--cold-cap", type=int, default=200_000)
    # gpt
    ap.add_argument("--capture-dir", default=os.path.expanduser("~/.cache/ostk-cache/http-capture-gpt"))
    ap.add_argument("--gpt-wire", choices=("oauth", "platform"), default="oauth")
    ap.add_argument("--gpt-reset-collapse", type=float, default=GPT_RESET_COLLAPSE)
    ap.add_argument("--gpt-dip-ratio", type=float, default=GPT_DIP_RATIO)
    ap.add_argument("--gpt-input-rate", type=float, default=GPT_PLATFORM_DEFAULTS["input_per_mtok"])
    ap.add_argument("--gpt-cached-rate", type=float, default=GPT_PLATFORM_DEFAULTS["cached_per_mtok"])
    ap.add_argument("--gpt-output-rate", type=float, default=GPT_PLATFORM_DEFAULTS["output_per_mtok"])
    args = ap.parse_args()

    report = run_anthropic(args) if args.provider == "anthropic" else run_gpt(args)

    if args.json:
        json.dump(report, sys.stdout, indent=1, default=str)
        print()
        return

    if args.provider == "anthropic":
        # Text surface replicated from the frozen artifact verbatim —
        # tier decomposition (the 1h→5m write flip IS →2032's headline
        # mechanism), write-share, and the AC-4 worst-cold-write receipt.
        def row(name, s):
            print(
                f"{name:9} calls={s['calls']:5} read={s['cache_read']/1e6:8.1f}M "
                f"write={s['cache_write']/1e6:7.2f}M (5m {s['cache_write_5m']/1e6:.2f}M / "
                f"1h {s['cache_write_1h']/1e6:.2f}M) hit={s['hit_ratio']:6.1%} "
                f"w/call={s['write_per_call']:6} spend=${s['spend']['total']:7.2f} "
                f"write-share={s['write_share']:5.1%}"
            )

        row("baseline", report["baseline"])
        row("replay", report["replay"])
        wc = report["worst_cold_write"]
        print(
            f"replay tiers: {report['replay_warm_calls']} WARM / "
            f"{report['replay_dead_calls']} DEAD; "
            f"worst cold write {wc['tokens']/1e3:.0f}k tok "
            f"({wc['session']} @ {wc['ts']})"
        )
    else:
        print(
            f"gpt[{report['wire']}] calls={report['calls']} "
            f"(warm {report['warm_turns']} / segment-open {report['segment_open_turns']}, "
            f"{report['skipped_no_usage']} skipped no-usage)"
        )
        print(
            f"lanes={report['lanes']} segments={report['segments']} loss: "
            + " ".join(f"{k}={v}" for k, v in report["loss_classes"].items())
        )
        print(
            f"input(incl)={report['input_tokens_inclusive']/1e6:.2f}M "
            f"cached={report['cached_tokens']/1e6:.2f}M "
            f"uncached={report['uncached_input_tokens']/1e6:.2f}M "
            f"out={report['output_tokens']/1e3:.1f}k"
        )
        print(
            f"warm-turn hit={report['warm_turn_hit_ratio']:.1%} (§7.4 segmented) "
            f"overall cached share={report['overall_cached_share']:.1%}"
        )
        if report.get("spend_provisional"):
            print(f"PROVISIONAL platform spend: ${report['spend_provisional']['total']:.4f}")
        else:
            print(report["spend_note"])
    ac = report["ac"]
    print("AC: " + "  ".join(f"{k}={'PASS' if v else 'FAIL'}" for k, v in ac.items()))
    if not all(ac.values()):
        sys.exit(2)


if __name__ == "__main__":
    main()
