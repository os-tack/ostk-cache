# ostk-cache

Drop-in **L1.5 caching proxy** for the Anthropic `/v1/messages` API. Sits between any Anthropic API client and `api.anthropic.com`, anchors long-lived context (system prompts, tool definitions, kernel orientation) into stable byte-boundaries that hit Anthropic's prompt cache, and ledgers per-turn cache efficiency for A/B analysis.

Works with **any surface that lets you set `ANTHROPIC_BASE_URL`** — Claude Code, Codex, Cursor, custom MCP servers, internal harnesses, anything that speaks Anthropic's API. The proxy is transparent at the protocol layer (chunked HTTP, SSE streaming, multipart file uploads all forward verbatim where appropriate); only request bodies are rewritten for cache placement.

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
# Capture Proxy running on 127.0.0.1:8080 (mode: mutate)

# 2. Point your agent surface at it
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080

# 3. Use the agent normally — claude, codex, cursor, custom MCP host, etc.
claude   # or codex / cursor / your harness
```

Every turn appends an `AmpRow` to `.ostk/memory/ledger.jsonl` in the proxy's cwd, tagged with the active mode for later A/B partitioning.

## Modes

The proxy has four mutation strategies, selected by environment variable. All four ledger their accounting; only the request-body rewrite differs.

| Mode             | Env vars                              | What it does to `messages[]`                                         |
|------------------|---------------------------------------|----------------------------------------------------------------------|
| `passthrough`    | `OSTK_CACHE_PASSTHROUGH=1`            | Byte-identical forward. Control baseline.                            |
| `mutate` (default) | *(none)*                            | Collapse system to one 1h cache block; HUD prepend; strip user `cache_control`. |
| `rebuild_local`  | `OSTK_CACHE_REBUILD=1`                | Discard prior turns; replace with synthesized **kernel projection** (envelope + tool summary + intent thread + recent assistant turn digests). In-flight chain preserved. |
| `rebuild_kernel` | `OSTK_CACHE_REBUILD=kernel`           | Same as rebuild_local but the live envelope is fetched from a running ostk kernel daemon over `.ostk/ostk.sock`. Falls back to `rebuild_local` if the kernel isn't reachable. |

Optional layer-3 add-on (combinable with any rebuild mode): `OSTK_CACHE_TAIL_TRANSCRIPT=1` ingests cross-session activity from the local Claude Code transcript directory and appends it to the synthetic context.

The `Makefile` wires every combination as a `make run-*` target. See `make help`.

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

## Configuration reference

| Variable                          | Default     | Purpose                                                |
|-----------------------------------|-------------|--------------------------------------------------------|
| `ANTHROPIC_API_KEY`               | *(required)*| Forwarded as `x-api-key` upstream.                     |
| `PROXY_PORT`                      | `8080`      | TCP port the proxy binds.                              |
| `OSTK_CACHE_PASSTHROUGH`          | unset       | `1`/`true`/`yes` → byte-identical forward.             |
| `OSTK_CACHE_REBUILD`              | unset       | `1` → standalone rebuild; `kernel` → federated.        |
| `OSTK_CACHE_TAIL_TRANSCRIPT`      | unset       | `1` → ingest local Claude Code transcript tail.        |
| `OSTK_CACHE_KERNEL_TIMEOUT_MS`    | `2000`      | Per-IPC timeout when fetching a kernel projection.     |
| `OSTK_CACHE_CLAUDE_PROJECTS_DIR`  | `~/.claude/projects` | Override transcript-tail source directory.    |

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

See `LICENSE`.
