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
              ratio >= 90% (observed baseline 96–98.5%).
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
            "hit_ratio": r / (r + w) if r + w else 0.0,
            "write_per_call": w // stats["calls"],
            "spend": sp,
            "write_share": sp["write"] / sp["total"] if sp["total"] else 0.0,
        }

    base_sum, sim_sum = summarize(base, base_pm), summarize(sim, sim["per_model"])
    worst = sim["cold_writes"][0] if sim["cold_writes"] else (0, None, None)
    return {
        "provider": "anthropic",
        "window": {"dir": args.dir},
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


def run_gpt(args) -> dict:
    calls, skipped = extract_gpt_calls(args.capture_dir)
    if not calls:
        sys.exit(f"no usable gpt captures in {args.capture_dir} ({skipped} skipped)")

    warm = [c for c in calls if c["cached"] > 0]
    cold = [c for c in calls if c["cached"] == 0]
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
        "warm_turns": len(warm),
        "cold_turns": len(cold),
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
                "input": c["input"],
                "cached": c["cached"],
                "hit": c["cached"] / c["input"] if c["input"] else 0.0,
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
    ap.add_argument("--gpt-input-rate", type=float, default=GPT_PLATFORM_DEFAULTS["input_per_mtok"])
    ap.add_argument("--gpt-cached-rate", type=float, default=GPT_PLATFORM_DEFAULTS["cached_per_mtok"])
    ap.add_argument("--gpt-output-rate", type=float, default=GPT_PLATFORM_DEFAULTS["output_per_mtok"])
    args = ap.parse_args()

    report = run_anthropic(args) if args.provider == "anthropic" else run_gpt(args)

    if args.json:
        json.dump(report, sys.stdout, indent=1, default=str)
        print()
        return

    if report["provider"] == "anthropic":
        b, s = report["baseline"], report["replay"]
        for name, x in (("baseline", b), ("replay", s)):
            print(
                f"{name:9} calls={x['calls']:5} read={x['cache_read']/1e6:8.1f}M "
                f"write={x['cache_write']/1e6:7.2f}M hit={x['hit_ratio']:6.1%} "
                f"w/call={x['write_per_call']:6} spend=${x['spend']['total']:7.2f}"
            )
        print(f"tiers: {report['replay_warm_calls']} WARM / {report['replay_dead_calls']} DEAD")
    else:
        print(
            f"gpt[{report['wire']}] calls={report['calls']} "
            f"(warm {report['warm_turns']} / cold {report['cold_turns']}, "
            f"{report['skipped_no_usage']} skipped no-usage)"
        )
        print(
            f"input(incl)={report['input_tokens_inclusive']/1e6:.2f}M "
            f"cached={report['cached_tokens']/1e6:.2f}M "
            f"uncached={report['uncached_input_tokens']/1e6:.2f}M "
            f"out={report['output_tokens']/1e3:.1f}k"
        )
        print(
            f"warm-turn hit={report['warm_turn_hit_ratio']:.1%} "
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
