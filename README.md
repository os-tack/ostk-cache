# ostk-cache

Drop-in **L1.5 caching proxy** for Anthropic `/v1/messages`, with an OpenAI GPT adapter for `/v1/responses`. The Anthropic adapter anchors long-lived context (system prompts, tool definitions, kernel orientation) into stable byte-boundaries that hit Anthropic's prompt cache; the GPT adapter applies OpenAI prompt-cache routing knobs while preserving the provider-specific wire shape.

Works with **any surface that lets you set a provider base URL** — Claude Code, Codex, Cursor, custom MCP servers, internal harnesses. The proxy is transparent at the protocol layer (chunked HTTP, SSE streaming, multipart file uploads all forward verbatim where appropriate); request-body rewrites are adapter-specific.

## Install

**Pre-built binaries** for every release — three bins per platform, no build step required:

| Platform        | Proxy                          | Hooks installer                       | Stats reporter                        |
|-----------------|--------------------------------|---------------------------------------|---------------------------------------|
| Linux x86_64    | `ostk-cache-linux-amd64`       | `ostk-cache-hooks-linux-amd64`        | `ostk-cache-stats-linux-amd64`        |
| macOS x86_64    | `ostk-cache-macos-amd64`       | `ostk-cache-hooks-macos-amd64`        | `ostk-cache-stats-macos-amd64`        |
| macOS arm64    | `ostk-cache-macos-arm64`       | `ostk-cache-hooks-macos-arm64`        | `ostk-cache-stats-macos-arm64`        |
| Windows x86_64  | `ostk-cache-windows-amd64.exe` | `ostk-cache-hooks-windows-amd64.exe`  | `ostk-cache-stats-windows-amd64.exe`  |

Grab from the [Releases page](https://github.com/os-tack/ostk-cache/releases/latest), `chmod +x`, drop on `PATH`. Quick install on Linux/macOS:

```bash
PLATFORM=linux-amd64   # or macos-amd64 / macos-arm64
BASE=https://github.com/os-tack/ostk-cache/releases/latest/download
curl -L "$BASE/ostk-cache-$PLATFORM"        -o /usr/local/bin/ostk-cache       && chmod +x /usr/local/bin/ostk-cache
curl -L "$BASE/ostk-cache-hooks-$PLATFORM"  -o /usr/local/bin/ostk-cache-hooks && chmod +x /usr/local/bin/ostk-cache-hooks
curl -L "$BASE/ostk-cache-stats-$PLATFORM"  -o /usr/local/bin/ostk-cache-stats && chmod +x /usr/local/bin/ostk-cache-stats
```

**Building from source** (contributors only):

```bash
git clone https://github.com/os-tack/ostk-cache && cd ostk-cache
cargo build --release --bins
# Binaries land in target/release/{ostk-cache,hooks,stats}
```

`ostk-cache` depends on three private membrane crates from `os-tack/haystack` (resolved via git-deps with HTTPS auth). For local development with a sibling haystack checkout, see the `[patch]` recipe at the bottom of `Cargo.toml`.

## Quick start

```bash
# 1. Start the proxy
ANTHROPIC_API_KEY=sk-ant-... ostk-cache
# ostk-cache 0.3.1 listening on 127.0.0.1:8080
#   provider=anthropic  mode=mutate  soft-cap=30MB  tail=off
#   rewrite=on  capture=off  kernel-timeout=500ms

# 2. Point your agent surface at it
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080

# 3. Use the agent normally — claude, codex, cursor, custom MCP host, etc.
claude   # or codex / cursor / your harness
```

Common operator flags (see `ostk-cache --help` for the full list):

```bash
ostk-cache --mode rebuild-kernel --soft-cap-mb 28 --tail-transcript
ostk-cache --provider gpt --upstream https://api.openai.com
ostk-cache --print-config           # show resolved config + source attribution
```

Every turn appends an `AmpRow` to `.ostk/memory/ledger.jsonl` in the proxy's cwd, tagged with the active mode for later A/B partitioning. The proxy supports **graceful shutdown** (SIGINT/SIGTERM) — in-flight requests are drained and the server waits for active SSE streams to finish before exiting.

## Modes

The Anthropic adapter has four mutation strategies, selected by `--mode` (or
`OSTK_CACHE_REBUILD` / `OSTK_CACHE_PASSTHROUGH` env legacy, or the
`mode = "…"` key in `.ostk/cache.toml`). All four ledger their
accounting; only the Anthropic request-body rewrite differs. The GPT adapter
uses `mode="gpt"` in the ledger and applies OpenAI prompt-cache parameters to
`/v1/responses` requests.

| `--mode`         | Ledger tag         | What it does to `messages[]`                                         |
|------------------|--------------------|----------------------------------------------------------------------|
| `passthrough`    | `passthrough`      | Byte-identical forward. Control baseline.                            |
| `mutate` (default) | `mutate`         | Collapse system to one 1h cache block; HUD prepend; strip user `cache_control`. |
| `rebuild`        | `rebuild_local`    | Discard prior turns; replace with synthesized **kernel projection** (envelope + tool summary + intent thread + recent assistant turn digests). In-flight chain preserved. |
| `rebuild-kernel` | `rebuild_kernel`   | Same as `rebuild` but the live envelope is fetched from a running ostk kernel daemon over `.ostk/ostk.sock`. Falls back to `rebuild_local` if the kernel isn't reachable. |

Optional layer-3 add-on (combinable with any rebuild mode): `--tail-transcript`
(or `OSTK_CACHE_TAIL_TRANSCRIPT=1`) ingests cross-session activity from
the local Claude Code transcript directory and appends it to the
synthetic context.

The `Makefile` wires every combination as a `make run-*` target. See `make help`.

## Configuration

The proxy resolves every setting from four sources, **highest precedence
first**:

1. CLI flags (`--port`, `--mode`, `--soft-cap-mb`, ...)
2. Environment variables (legacy escape hatch — same names as before)
3. Workspace TOML at `<cwd>/.ostk/cache.toml` (or `--config PATH`)
4. Built-in defaults

Run `ostk-cache --print-config` to dump the resolved table with the
**source** column showing where each value came from — invaluable when
debugging "why is my flag being ignored?":

```
port                       = 8089          (toml)
provider                   = anthropic     (default)
mode                       = rebuild-kernel(cli)
soft_cap                   = 28MB          (toml)
tail.transcript            = true          (env)
rewrite.enabled            = true          (default)
kernel.timeout_ms          = 250           (toml)
```

A workspace `.ostk/cache.toml` looks like:

```toml
provider = "anthropic"
mode = "rebuild-kernel"
port = 8089
soft_cap_mb = 28

[tail]
transcript = true
limit = 75

[rewrite]
enabled = true

[kernel]
timeout_ms = 250

[capture]
http = false
# dir = ".ostk/http-capture"
```

## Observability

Each turn emits a single compact line to stdout:

```
[turn s=a323be mode=rebuild_kernel req=12.4MB→0.18MB resp=14.2KB tok_in=5421 cache_r=98% drop=701/1.7MB→14KB elapsed=3.4s]
```

When a section gets bloated (any single section >5MiB or the post-rewrite
request >80% of the soft cap), an indented second line shows the
per-section breakdown:

```
  └─ sys=2.1MB tools=8.4MB synthetic=14KB in_flight=1.8MB (dominant: tools)
```

Run with `--verbose` to keep the legacy multi-line per-pass output in
addition to the one-liner.

Ledger columns added: `req_bytes_in`, `req_bytes_out`, `resp_bytes`, `system_bytes`, `tools_bytes`, `synthetic_bytes`, `in_flight_bytes` (all `Option<u64>` — old rows without them deserialize as `null`). `ostk-cache-stats` aggregates them into `bytes_in_total`, `bytes_out_total`, `resp_bytes_total`, and `bytes_reduction_ratio`, providing a section-level view of where the bytes are spent across the entire session.

For wire-body investigations, run with `--capture-http` to write one directory per proxied request under `.ostk/http-capture` by default. Each capture contains `request-in.body`, `request-out.body`, `response.body`, and `metadata.json` with status, byte counts, SHA-256 hashes, route path, and redacted auth headers. This is off by default; the normal proxy path only records byte/token metadata.

## GPT adapter

Run `ostk-cache --provider gpt` and point OpenAI-compatible clients at `/v1/responses` through the proxy. The GPT adapter preserves the caller's request shape and adds `prompt_cache_key` when absent. `prompt_cache_retention` is wire-gated: added only when the upstream host is exactly `api.openai.com` (Platform wire); the ChatGPT oauth backend rejects the parameter with a named-field 400, so on that wire the adapter never injects it. Usage accounting reads OpenAI `usage.input_tokens_details.cached_tokens` into the shared ledger so cache hit rate can be compared with Anthropic runs (note: GPT `input_tokens` is cache-inclusive; Anthropic's is cache-exclusive).

## Soft cap

Anthropic enforces a 32MB hard limit on `/v1/messages`. To rescue
requests before they hit that wall, the proxy enforces a configurable
soft cap (`--soft-cap-mb`, default `30`, `0` disables) with a
progressive reduction pipeline. Tiers fire in order until under cap:

1. **Tier A — tool-result ejection.** `tool_result` content bodies
   larger than 100KB are replaced with a `[ejected: …]` stub, preserving
   the `tool_use_id` pairing. Largest-first ordering. The model can
   re-run the call if it needs the data.
2. **Tier B — in-flight pair pruning.** Oldest `tool_use`/`tool_result`
   message pairs in the active cycle are removed as a unit so neither
   side is orphaned. The most recent pair is always retained.
3. **Tier C — tool-defs trimming.** Tool definitions not referenced by
   any in-flight `tool_use` are dropped. Conservative: never drops a
   tool the assistant is actively using.
4. **Tier D — structured 413.** All tiers exhausted; the proxy returns
   HTTP 413 with a `reduction` payload showing what was tried.

Every reduction event lands in the per-turn one-liner (`reduce=A→B
ej=N(bytes) prune=N tools=N`) and a dedicated **accounting row** in the
ledger (`mode="reduce"`). This persists the specific tier counters and
the `irreducible` flag, allowing long-term analysis of how often the
soft cap is triggered and which tiers are most effective at recovering
space.

## Kernel orientation

When `rebuild_*` modes are active, the proxy appends a discipline block to the system prompt instructing the model to:

- Treat the projection as authoritative working state, not the full transcript
- Reach for the right primitive (re-run / `recall:<addr>` / handles) when historical artifacts are needed
- Trust that `[ok]` tool results in the projection are shapes-only and `[error]` results carry full bodies
- End every turn with a `<turn-digest>{...}</turn-digest>` fence so intent survives the next projection

The orientation text is byte-stable across turns and cached at the 1h tier — the model pays for it once per cache window and gets a coherent operating discipline for free.

## Hooks (Claude Code)

`ostk-cache-hooks` installs Claude Code lifecycle hooks (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`) that POST to the proxy's `/hook/event` endpoint. The proxy ledgers each event into `.l1.5/hooks.jsonl` and snapshots `manifest.json` on session stop.

```bash
ostk-cache-hooks install     # idempotent; appends, never overwrites; backs up settings.json
ostk-cache-hooks status
ostk-cache-hooks uninstall   # --purge to also remove dispatch script
```

Other agent surfaces with similar hook conventions (any tool that exposes session-lifecycle hooks and lets you shell out) can post to `/hook/event` directly — the endpoint is generic HTTP. See [docs/HOOKS.md](docs/HOOKS.md) for the wire format and a manual `settings.json` snippet.

## Stats and A/B analysis

`ostk-cache-stats` reads `.ostk/memory/ledger.jsonl` and emits per-session JSON or CSV.

```bash
ostk-cache-stats --window 24h --format json
ostk-cache-stats --mode rebuild_local        # filter by mode
ostk-cache-stats --workspace <16-char-hash>  # filter by workspace
```

Fields per session: `amp_mean`, `amp_p50`, `cache_hit_rate`, `turns`, `state_bytes_mean`, `mode`. For the recommended A/B comparison protocol (collect a window in each mode, partition by `mode` field, run side-by-side aggregation), see [docs/PASSTHROUGH.md](docs/PASSTHROUGH.md).

## Legacy environment variables

For pre-existing scripts and Makefile targets, the env-var surface is
kept intact (CLI flags and `.ostk/cache.toml` override these silently
when set):

| Variable                          | Default     | Purpose                                                |
|-----------------------------------|-------------|--------------------------------------------------------|
| `ANTHROPIC_API_KEY`               | *(required)*| Forwarded as `x-api-key` upstream.                     |
| `ANTHROPIC_BASE_URL`              | `https://api.anthropic.com` | Anthropic upstream override (matches `--upstream`).|
| `OPENAI_BASE_URL`                 | `https://api.openai.com` | GPT upstream override when `--provider gpt`.       |
| `OSTK_PROVIDER`                   | `anthropic`  | `anthropic` or `gpt` adapter selection.             |
| `PROXY_PORT`                      | `8080`      | TCP port the proxy binds (matches `--port`).           |
| `OSTK_CACHE_PASSTHROUGH`          | unset       | `1`/`true`/`yes` → byte-identical forward.             |
| `OSTK_CACHE_REBUILD`              | unset       | `1` → standalone rebuild; `kernel` → federated.        |
| `OSTK_CACHE_TAIL_TRANSCRIPT`      | unset       | `1` → ingest local Claude Code transcript tail.        |
| `OSTK_CACHE_TAIL_LIMIT`           | `50`        | Per-request transcript event cap.                      |
| `OSTK_CACHE_KERNEL_TIMEOUT_MS`    | `500`       | Per-IPC timeout when fetching a kernel projection.     |
| `OSTK_CACHE_CLAUDE_PROJECTS_DIR`  | `~/.claude/projects` | Override transcript-tail source directory.    |
| `OSTK_KERNEL_SOCKET`              | unset       | Pin explicit kernel socket path (skip cwd-walk).       |
| `OSTK_REWRITE_ENABLED`            | `1`         | `0`/`false` → disable file-handle rewrite pass.        |
| `OSTK_CAPTURE_HTTP`               | unset       | `1`/`true`/`yes` → capture request/response bodies.    |
| `OSTK_CAPTURE_HTTP_DIR`           | `<cwd>/.ostk/http-capture` | Override capture output directory.       |
| `OSTK_DIR`                        | `<cwd>/.ostk` if exists | Workspace `.ostk/` for file-handle cache.  |

## Workspace identity

The proxy partitions cache logic per workspace to prevent cross-repo pollution. Workspace identity is resolved in priority order:

1. **Explicit:** sha256 of `<cwd>/.l1.5/workspace-id` if present.
2. **Git origin:** sha256 of `git -C <cwd> config --get remote.origin.url` (normalized).
3. **Path:** sha256 of `realpath(cwd)`.

The first 16 hex chars become the workspace_id used in `hooks.jsonl` rows.

## Layout

```
.ostk/memory/
  ledger.jsonl              append-only AmpRow log (cache hits, token usage, mode tag)
.ostk/http-capture/
  <request-id>/             opt-in full-body capture (`--capture-http`)
    request-in.body
    request-out.body
    response.body
    metadata.json
.l1.5/
  workspace-id              optional explicit workspace identifier
  hooks.jsonl               session lifecycle events (rotated hourly to .gz)
  manifest.json             snapshot written on Stop hook
```

## Architecture

Hyper + Axum HTTP listener. `tokio::net::TcpListener` for incoming connections, `reqwest` for upstream forwarding. Streaming responses are mapped block-by-block via `async-stream` so SSE flush boundaries survive. The page-table substrate is the `Page` / `PageState` types from the `ostk-page` membrane crate; the in-memory backend is the default but the `PageTable` trait is open for alternate implementations.

The `kernel_client` module speaks the haystack daemon's IPC protocol over `.ostk/ostk.sock` (Unix domain socket). On Windows, federation is unavailable (the kernel projection path is `cfg(unix)`-stubbed); the proxy runs in standalone modes only.

## Documentation

- [docs/HOOKS.md](docs/HOOKS.md) — Claude Code lifecycle hook integration, manual settings.json snippet, troubleshooting.
- [docs/PASSTHROUGH.md](docs/PASSTHROUGH.md) — A/B comparison protocol for evaluating mutation impact.

## License

Dual-licensed under either:

- [MIT License](LICENSE-MIT)
- [Apache License, Version 2.0](LICENSE-APACHE)

at your option. Contributions are accepted under the same terms (Apache-2.0 §5).
