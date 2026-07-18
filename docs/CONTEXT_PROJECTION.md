# Context projection in ostk-cache

This document maps how an incoming `/v1/messages` request body is
**projected** into the cache-friendly form forwarded upstream. The
projection is the heart of the `rebuild` / `rebuild-kernel` modes: it
replaces the entire prior-turn history with a synthesized markdown
**kernel projection** so the unchanging part of the request stays
byte-stable across turns and hits Anthropic's prompt cache.

## End-to-end flow

```mermaid
flowchart TD
    subgraph Client["Agent surface (Claude Code / Codex / Cursor / ...)"]
        REQ["POST /v1/messages<br/>system + tools + messages[]"]
    end

    REQ -->|ANTHROPIC_BASE_URL| HANDLER["handle_anthropic_message<br/>(main.rs)"]

    HANDLER --> MODE{mode}

    MODE -- passthrough --> FWD
    MODE -- mutate --> LEGACY["HUD prepend + collapse system<br/>strip user cache_control"]
    LEGACY --> FWD

    MODE -- rebuild_kernel --> KFETCH["kernel_client::fetch_projection_with<br/>(.ostk/ostk.sock UDS, timeout)"]
    KFETCH -- ok --> SETENV["live_envelope_override = envelope"]
    KFETCH -- "timeout / socket missing / parse err" --> DEMOTE["demote to rebuild_local<br/>(graceful fallback)"]
    SETENV --> REBUILD
    DEMOTE --> REBUILD
    MODE -- rebuild_local --> REBUILD

    subgraph Projection["Projection assembly (rebuild.rs)"]
        REBUILD["apply_rebuild(req, config)"]
        REBUILD --> BOUND["find_cycle_boundary_idx<br/>= last real user message"]
        BOUND --> SPLIT["split messages[]<br/>dropped = [0..idx]<br/>in_flight = [idx..]"]

        SPLIT --> EXTRACT
        SPLIT --> KEEP["preserve in-flight chain<br/>(user + assistant tool_use<br/>+ tool_result + ... → end_turn)"]

        subgraph EXTRACT["Mine dropped slice"]
            E1["extract_latest_envelope<br/>(scans tool_result text for<br/>[procs]/[loadavg]/[meminfo]/[ctx])"]
            E2["summarize_native_tools<br/>last N tool_use signatures"]
            E3["extract_user_intent_thread<br/>last M prior user msgs"]
        end

        E1 --> ENV
        ENV{"envelope source"}
        SETENV -.live override wins.-> ENV
        ENV -->|kernel envelope| COMPOSE
        ENV -->|extracted| COMPOSE
        ENV -->|neither| ZERO["zeroed placeholder envelope"]
        ZERO --> COMPOSE

        OPT1["transcript_tail_summary<br/>(transcript_tail.rs:<br/>~/.claude/projects/&lt;hash&gt;/*.jsonl)"]
        OPT2["recent_assistant_digests<br/>(cycle_digest.rs:<br/>~/.ostk-cache/state/&lt;hash&gt;/cycle_digests.jsonl)"]

        E2 --> COMPOSE["compose_synthetic_context<br/>→ markdown string"]
        E3 --> COMPOSE
        OPT1 --> COMPOSE
        OPT2 --> COMPOSE

        COMPOSE --> ASSEMBLE["new messages[] =<br/>[ {role:user, text: &lt;synthetic&gt;} ]<br/>++ in_flight"]
        KEEP --> ASSEMBLE
        ASSEMBLE --> ORPHAN["repair_orphaned_tool_results<br/>(strip tool_result blocks<br/>whose paired tool_use was dropped)"]
    end

    ORPHAN --> ORIENT["append_kernel_orientation_to_system<br/>(rebuild.rs:62)<br/>discipline block + cache_control ttl=1h"]

    ORIENT --> REWRITE["rewrite_middleware::apply_rewrite<br/>inline content → file_id refs<br/>(SHA-256 match in .ostk/file_cache.jsonl)"]

    REWRITE --> SOFTCAP

    subgraph SOFTCAP["enforce_soft_cap (rebuild.rs:394)"]
        direction TB
        TA["Tier A — eject tool_result content &gt;100 KiB"]
        TB["Tier B — prune oldest in-flight tool_use/result pairs"]
        TC["Tier C — drop unreferenced tool defs"]
        TD["Tier D — irreducible → HTTP 413"]
        TA --> TB --> TC --> TD
    end

    SOFTCAP -- under cap --> FWD["forward upstream<br/>reqwest → api.anthropic.com"]
    SOFTCAP -- "Tier D" --> R413["413 reduction payload"]

    FWD --> UP["Anthropic /v1/messages"]
    UP --> STREAM["SSE / JSON response<br/>streamed back via async-stream"]
    STREAM --> CLIENT2["agent surface"]
    STREAM --> HARVEST["harvest &lt;turn-digest&gt;{...}&lt;/turn-digest&gt;<br/>fence from assistant reply"]
    HARVEST --> PERSIST["append to cycle_digests.jsonl<br/>(feeds next turn's projection)"]
    STREAM --> LEDGER["AmpRow → .ostk/memory/ledger.jsonl<br/>tagged with mode"]
```

## Layered byte-stability strategy

```mermaid
flowchart LR
    subgraph Outgoing["Final request body (post-projection)"]
        direction TB
        SYS["system[]<br/>━━━━━━━━━━━━<br/>original system<br/>+ kernel orientation<br/>(byte-stable, cache_control ttl=1h)"]
        TOOLS["tools[]<br/>━━━━━━━━━━━━<br/>tool defs (filtered by Tier C)"]
        SYNTH["messages[0]<br/>━━━━━━━━━━━━<br/>{role:user, text: kernel projection}<br/>━━━━━━━━━━━━<br/># Kernel projection — current cycle state<br/>## Live state envelope<br/>## Recent tool activity<br/>## Prior user intent thread<br/>(recent assistant digests)<br/>(transcript tail)<br/>## Current cycle"]
        INFLIGHT["messages[1..]<br/>━━━━━━━━━━━━<br/>real user msg<br/>+ in-flight tool_use/result chain"]
    end

    SYS -.->|"1h cache hit"| HIT1[("Anthropic<br/>prompt cache")]
    TOOLS -.->|"5m cache hit"| HIT1
    SYNTH -.->|"dynamic, no marker"| MISS[("recomputed<br/>per turn")]
    INFLIGHT -.->|"dynamic"| MISS
```

The orientation block in the system tier is **firmware-class** —
byte-identical across every turn — so it carries the 1h cache breakpoint.
The synthetic projection itself is intentionally **not** cache-marked: its
envelope, tool list, and digests shift every turn, and Anthropic caps
`cache_control` markers at 4 per request, so the marker was better spent
on system.

## Mode divergence

```mermaid
flowchart TD
    M{"--mode"}
    M -->|passthrough| P["byte-identical forward<br/>(control baseline)"]
    M -->|mutate (default)| MU["legacy: HUD prepend +<br/>1h system cache +<br/>strip user cache_control"]
    M -->|rebuild| RL["rebuild_local<br/>envelope from tool_result text"]
    M -->|rebuild-kernel| RK["rebuild_kernel<br/>envelope from .ostk/ostk.sock"]
    RK -.->|kernel unreachable| RL
    RL --> COMMON["+ optional transcript tail<br/>+ recent assistant digests<br/>+ orientation in system<br/>+ rewrite + soft-cap"]
    RK --> COMMON
```

## Per-turn feedback loop

```mermaid
sequenceDiagram
    participant A as Agent surface
    participant P as ostk-cache proxy
    participant K as ostk kernel<br/>(.ostk/ostk.sock)
    participant U as api.anthropic.com
    participant L as cycle_digests.jsonl<br/>+ ledger.jsonl

    A->>P: POST /v1/messages (full history)
    opt rebuild_kernel
        P->>K: fetch_projection (IPC)
        K-->>P: live envelope (or timeout)
    end
    P->>P: find cycle boundary
    P->>P: extract envelope / tools / intent
    P->>L: read recent digests
    P->>P: compose synthetic projection
    P->>P: append orientation to system
    P->>P: rewrite + soft-cap
    P->>U: rewritten /v1/messages
    U-->>P: SSE stream
    P-->>A: SSE stream (verbatim)
    P->>P: harvest <turn-digest> fence
    P->>L: append cycle digest + AmpRow
    Note over L: feeds next turn's projection
```

## Key code locations

| Concern | File:Function |
| --- | --- |
| HTTP entrypoint, mode dispatch | `src/main.rs::handle_anthropic_message` |
| Kernel IPC, fallback to local | `src/kernel_client.rs::fetch_projection_with` |
| Projection assembly | `src/rebuild.rs::apply_rebuild` (rebuild.rs:238) |
| Cycle boundary detection | `src/rebuild.rs::find_cycle_boundary_idx` (rebuild.rs:746) |
| Envelope extraction | `src/rebuild.rs::extract_latest_envelope` (rebuild.rs:781) |
| Tool summary | `src/rebuild.rs::summarize_native_tools` (rebuild.rs:887) |
| User intent thread | `src/rebuild.rs::extract_user_intent_thread` (rebuild.rs:997) |
| Final markdown composition | `src/rebuild.rs::compose_synthetic_context` (rebuild.rs:1156) |
| Orphan tool_result repair | `src/rebuild.rs::repair_orphaned_tool_results` (rebuild.rs:672) |
| Kernel orientation block | `src/rebuild.rs::append_kernel_orientation_to_system` (rebuild.rs:62) |
| File-handle rewrite | `src/rewrite_middleware.rs::apply_rewrite` |
| Soft-cap reduction (Tiers A–D) | `src/rebuild.rs::enforce_soft_cap` (rebuild.rs:394) |
| Transcript tail ingest | `src/transcript_tail.rs` |
| Turn-digest harvest + replay | `src/cycle_digest.rs` |
| Mode / option resolution | `src/config.rs` |
