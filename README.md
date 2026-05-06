# ostk-cache

`ostk-cache` is the L1.5 transparent proxy for the Anthropic API in the `llmOS` ecosystem. It acts as an intercepting proxy that takes standard Anthropic `/v1/messages` API requests, extracts long-lived context (like system prompts), and maps them into stable byte-boundaries (firmware) using Anthropic's ephemeral prompt caching mechanism.

## Features

- **Firmware Byte Stability:** Converts long system prompts into stable cache entries, avoiding cache invalidation across conversation turns with varying user messages.
- **Workspace-Aware Caching:** Partitions the cache logic by workspace (via git remote hashes or `.l1.5` markers) to prevent cross-repo cache pollution.
- **HUD Projection:** Automatically injects a caching "HUD" into the user's prompt to give operators visibility into L1.5 caching performance (AMP ratio, etc.).
- **Transparent Forwarding:** Full support for `chunked` HTTP/1.1 forwarding and proper SSE event mapping for streaming responses.
- **FileUpload Support:** Safely preserves image and document uploads inside user messages.

## Architecture

- **Proxy Layer:** Built using `tokio` TCP listeners and `reqwest` for upstream forwarding.
- **Memory Model:** Backed by a `PageTable` trait (currently using `InMemoryPageTable`). The daemon can materialize cache files directly via the Anthropic Files API.
- **Hook Adapters:** Integrates with the `llmOS` daemon via the `HookAdapter` trait for tracking sessions, turns, and prefetches.

## Running

Ensure `ANTHROPIC_API_KEY` is set in your environment.

```bash
cargo run --bin ostk-cache
```

The proxy will listen on `127.0.0.1:8080` (or `PROXY_PORT`).
