# PASSTHROUGH — A/B comparison of proxy mutations vs. native caching

The capture proxy normally rewrites every `/v1/messages` request before
forwarding it upstream to Anthropic. The premise is that those mutations
lift the **amplification ratio** (cache_read + input) / input — i.e. how
much of each turn's input came from cache rather than re-billing.

That premise needs to be tested empirically. Claude Code's own client
already places `cache_control` markers, so it is not obvious that the
proxy's rewrites help; they could be neutral, or even hurt by displacing
markers the client placed deliberately. `OSTK_CACHE_PASSTHROUGH=1`
toggles the proxy into a forwarding-only mode so we can measure the
delta against a window where the proxy stayed out of the way.

## What the three mutations do

When `OSTK_CACHE_PASSTHROUGH` is unset (the default, "mutate" mode) the
proxy applies three transforms before forwarding the body upstream:

1. **System collapse to a single 1h cache block.** Whatever the client
   sent — string, single block, multiple blocks with their own
   `cache_control` — is reduced to one `text` block with
   `cache_control: {type: ephemeral, ttl: "1h"}`. Goal: keep the
   firmware (system prompt) anchored in the long-lived cache layer
   regardless of how many parts the client used.

2. **HUD prepend on the most recent user turn.** A short status line
   (`cache: 5m=- 1h=- amp=Nx stored=K hot=H`) is inserted as the first
   block of the last user message, with its own 5m
   `cache_control` marker. Goal: provide a deterministic prefix on
   non-tool-result turns so the cache anchor for the moving edge is
   stable; secondary goal is observability inside the model's view.

3. **Strip `cache_control` from existing user blocks.** The proxy
   removes any client-placed `cache_control` keys from the remaining
   blocks of the rewritten user message. Goal: avoid having two
   competing cache markers in the same message.

Each mutation has a plausible upside (better anchor placement, fewer
competing markers) and a plausible downside (the client may have known
something we don't). The A/B protocol below collects evidence on the
net effect.

## What carries across modes

Passthrough mode forwards the inbound body byte-identically (modulo the
hop-by-hop `content-type` / `accept-encoding` housekeeping the upstream
HTTP loop already applies). Crucially:

- **Accounting still runs.** Usage parsing, ledger persistence, and the
  in-process `amp_store` update happen in both modes. Every turn
  produces an `AmpRow` in `.ostk/memory/ledger.jsonl`.
- **Rows are tagged.** Each row carries `mode: "mutate"` or
  `mode: "passthrough"` so they can be partitioned at analysis time.
- **Hooks still run.** `/hook/event` and the `DaemonAdapter` are
  unaffected by the flag.

## A/B protocol — empirical comparison

### Caveat up front: this is *workload* A/B, not *request* A/B

Cache state isn't shared between the two windows. A 1h cache entry
created during the mutate window is gone (or stale) by the time the
passthrough window starts, and vice versa. So this protocol answers
the question:

> Across two comparable workloads, does the proxy-with-mutations
> produce higher cache hit rate / amp_ratio than Claude Code talking
> to Anthropic directly?

It does **not** answer "for this exact request, was the mutated body
better than the original" — that requires shadow-A/B sending both
versions in parallel, which is option C from the design discussion
and is not what this flag implements.

### Step 1 — collect the mutate window

```sh
# Default mode: OSTK_CACHE_PASSTHROUGH unset
cargo run --release --bin ostk-cache-proxy
# Confirm banner reads: "Capture Proxy running on 127.0.0.1:8080 (mode: mutate)"
```

In a separate shell, run a normal Claude Code session pointed at the
proxy. Aim for ~30 minutes / ~50 turns of representative work — code
edits, tool calls, occasional new files. Stop the proxy with Ctrl-C
when done.

### Step 2 — collect the passthrough window

```sh
OSTK_CACHE_PASSTHROUGH=1 cargo run --release --bin ostk-cache-proxy
# Banner now reads: "Capture Proxy running on 127.0.0.1:8080 (mode: passthrough)"
```

Run another ~30 min / ~50 turns of comparable work in another Claude
Code session. The closer the workload shape to step 1, the cleaner the
comparison: similar mix of edit/read/test/explore, similar prompt
length, similar files touched.

Both windows append to the same `.ostk/memory/ledger.jsonl`. The mode
field is what partitions them.

### Step 3 — compare with the stats CLI

```sh
cargo run --release --bin stats -- --mode mutate
cargo run --release --bin stats -- --mode passthrough
```

Each call emits one JSON object per session. The fields we care about:

- `amp_mean` — average (cache_read + input) / input across the session.
  1.0 means no cache lift; 5.0 means 80% of input came from cache.
- `amp_p50` — median amp_ratio. Less skewed by a single fat turn.
- `cache_hit_rate` — cache_read / (cache_read + input). Bounded 0..1.
- `turns` — sample count. Comparison is only meaningful with similar
  turn counts in both windows.

### Step 4 — one-liner comparison report

```sh
# Aggregate across all sessions per mode and print side-by-side means.
{
  cargo run --quiet --release --bin stats -- --mode mutate \
    | jq -s '{mode: "mutate", sessions: length,
              amp_mean: (map(.amp_mean) | add / length),
              amp_p50: (map(.amp_p50) | add / length),
              cache_hit_rate: (map(.cache_hit_rate) | add / length),
              turns: (map(.turns) | add)}'
  cargo run --quiet --release --bin stats -- --mode passthrough \
    | jq -s '{mode: "passthrough", sessions: length,
              amp_mean: (map(.amp_mean) | add / length),
              amp_p50: (map(.amp_p50) | add / length),
              cache_hit_rate: (map(.cache_hit_rate) | add / length),
              turns: (map(.turns) | add)}'
} | jq -s .
```

This produces a two-element array — one per mode — with means
aggregated across whatever sessions ran in that window.

## Interpreting the result

| signal                                       | reading                                              |
|----------------------------------------------|------------------------------------------------------|
| mutate `amp_mean` materially > passthrough   | proxy's mutations lift cache reuse on this workload  |
| mutate `amp_mean` ≈ passthrough              | mutations are neutral; Claude Code already caches well |
| mutate `amp_mean` materially < passthrough   | mutations are *displacing* useful client markers     |

"Materially" is judgment — at this sample size, treat differences <5%
as noise. If `cache_hit_rate` and `amp_mean` move in opposite
directions, look at `turns` and `state_bytes_mean` for an explanation
(e.g. one window had more tool-result turns where the HUD path
short-circuits anyway).

A negative result is just as useful as a positive one. If the
mutations don't lift, that's evidence to drop them and keep the proxy
strictly observational. If they do lift, the magnitude tells us how
much engineering attention to pour into refining them further.
