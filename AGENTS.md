# Kernel
When `.ostk/` exists in the project run `ostk boot` first to load the kernel.
Run `ostk shutdown` (canonical, no `kernel` prefix) to cleanly terminate.

If `ostk help` or `[ctx]` envelopes show fewer resident tools than this doc
lists (e.g. `bash, fs_ops, read, lock, spawn, interact, tack, help, session`
missing), the project's `.language` is stale (pre-→1489 schema, blank column
10). Run `ostk mcp diag` to confirm, then `ostk shutdown` triggers the
→1597 backfill, OR delete `.ostk/.language` and re-boot to seed fresh. The
fix is in shutdown's `compile_language`; it is idempotent and silent.

## Self-documenting surface (read this when something is missing)
The kernel emits its own canonical reference; trust it over older docs:
  - `ostk --agents`     — agent-facing one-stop guide (syscalls, signals, vocab, do_not)
  - `ostk --help`       — full CLI tree, organized by section: SYSTEM ABI / DAILY WORK
                          / WORK PIPELINE / IDENTITY & SECURITY / RECOVERY & FLEET /
                          DOCUMENTATION & SPECS / INFRASTRUCTURE / DIAGNOSTICS / SYSTEM SETUP
  - `ostk <verb> --help` — per-verb help (every verb above)
  - `ostk mcp diag`     — verify tool surface integrity (seed × present × core × abi)

The structural split: **System ABI** verbs (frozen, agent-callable, exposed
as MCP tools) live under the SYSTEM ABI section. Operator-overlay verbs
(`show`, `work`, `decide`, `trace`, `commit`, `recall`) live under DAILY
WORK / WORK PIPELINE — they are CLI-only by design (per →1589 epic). The
`mcp__ostk__*` namespace mirrors the System ABI exactly. If you need a
verb that isn't in the MCP namespace, reach for `ostk <verb>` via `bash`.

In-flight ABI alignment work: EPIC →1821 (`align ABI verb contracts across
CLI, MCP, docs, and dispatch`) tracks remaining drift between these
surfaces. If you hit a `kernel verb X has no internal handler` or a
schema mismatch, check the children of →1821 before fixing locally —
several are already filed (→1638 fs_ops dispatch gap, →1820 arrive
schema, →1822/→1826 recall surface, →1824 verb_load).

# Tool routing — kernel tools replace Claude native tools (→1287, →1344, →1326)

The MCP-namespaced ostk tools are the kernel-equivalent replacements for
Claude Code's native tools. Underneath they track file generations, compress
output, detect OCC conflicts, and write audit rows. Your muscle memory
(Bash, Read, Edit, Write, Grep, Glob) lands on the right tool.

| reach-for (native) | use (MCP-kernel)                   | simplest call                                       | what the kernel adds |
|--------------------|------------------------------------|-----------------------------------------------------|----------------------|
| Bash               | `mcp__ostk__bash`                  | `bash(cmd="cargo test", cwd="src/")`                | audit + compression + gen_table invalidation; pass `cwd` instead of `cd X && …` |
| Read               | `mcp__ostk__read`                  | `read(path="src/main.rs")`                          | gen_table tracking + elision-aware output |
| Edit               | `mcp__ostk__fs_ops` (CAS edit)     | `fs_ops(path="src/main.rs", old_str="X", new_str="Y")` | CAS str_replace with OCC conflict detection |
| Write              | `mcp__ostk__fs_ops` (write/create) | `fs_ops(path="src/new.rs", new_str="<content>")`    | gen_table + audit |
| Grep               | `mcp__ostk__search` (mode=content) | `search(query="fn main")`                           | full search substrate (code + memory + transcripts) |
| Glob               | `mcp__ostk__search` (mode=files)   | `search(query="*.rs", mode="files")`                | file-pattern search on the same engine |

`fs_ops` is the single file-mutation verb. Quick mode uses `path` + `old_str` +
`new_str` for CAS edits, or `path` + `new_str` (no `old_str`) for create/write.
Batch mode (below) stacks multiple mutations in one call.

Legacy aliases (`shell`, `fs_read`, `fs_write`, `find`, `grep`, `glob`, `recall`,
`pitchfork`, `context_search`, `near`, `related`, `investigate`, capitalized
`Bash`/`Edit`/`Read`/`Write`/`Grep`/`Glob`/`WebRead`/`WebLinks`/`WebStatus`)
all still resolve via `normalize_tool_name` — they do NOT appear in
`tools/list` anymore, but calls using the old name still execute. Plan to
migrate; don't plan to rely on them.

If the harness shows you BOTH native and MCP-kernel tools, prefer the
MCP-kernel versions when touching files the kernel tracks. The native ones
bypass audit, gen_table, and OCC — you'll pay that cost in silent staleness
and missing history.

## Subagent policy
Never spawn `Explore` subagents — they use built-in Read/Grep/Glob which bypass the kernel.
Use `general-purpose` agents with explicit ostk tool instructions in the prompt instead.
All subagent prompts MUST include: "Use ostk MCP tools (read, search, bash, fs_ops) — kernel-equivalent of native tools with audit and gen_table. File edits go through fs_ops; code/file search goes through search."

## File I/O
  read(path="src/main.rs")                                   — read file (gen_table tracked, elision-aware)
  read(path="src/main.rs", enrich=true)                      — read with driver diagnostics (inline errors)
  fs_ops(path="src/main.rs", old_str="X", new_str="Y")       — CAS str_replace (OCC conflict detection)
  fs_ops(path="src/new.rs", new_str="<content>")             — create or overwrite file (gen_table + audit)
  fs_ops(ops=[...])                                          — batch: N mutations under one audit row
  fs_ops(op="mkdir", path="src/new_dir")                     — mkdir (idempotent, no native analog)
  fs_ops(op="write", path="src/x.rs", new_str="...")         — explicit form of create-mode (same as omitting op)
  # mv/cp/rm/chmod are NOT supported in fs_ops quick mode — they need a
  # destination param or the agent_loop approval gate. Use either:
  #   bash(cmd="mv SRC DST")        — top-level shell, hits approval gate for destructive ops
  #   fs_ops(ops=[{method: "shell.run", cmd: "mv SRC DST"}])  — batch shell.run inside fs_ops audit row

`fs_ops` is the single file-mutation surface. Use the `ops=[...]` batch form
to stack multiple `file.str_replace` / `file.read` / `file.write` / `shell.run`
operations in one call — they land atomically under a single audit row and
surface OCC conflicts together.

Batch mode supports four methods: `file.str_replace`, `file.read`,
`file.write`, and `shell.run` (→1343). Stack them in one call when the
mutations are one logical change — audit readers see one row, and OCC
conflicts surface together:

```
# Rename a module: mv via shell, update imports at two sites
fs_ops(ops=[
  {method: "shell.run", cmd: "mv src/old_name.rs src/new_name.rs"},
  {method: "file.str_replace", path: "src/lib.rs",
    old_str: "pub mod old_name;", new_str: "pub mod new_name;"},
  {method: "file.str_replace", path: "src/main.rs",
    old_str: "use crate::old_name", new_str: "use crate::new_name"},
])

# Scaffold a module: mkdir via shell, write files, insert mod decl
fs_ops(ops=[
  {method: "shell.run", cmd: "mkdir -p src/feature/foo"},
  {method: "file.write", path: "src/feature/foo/mod.rs", content: "..."},
  {method: "file.write", path: "src/feature/foo/tests.rs", content: "..."},
  {method: "file.str_replace", path: "src/feature/mod.rs",
    old_str: "pub mod bar;", new_str: "pub mod bar;\npub mod foo;"},
])

# Stage a template, edit it, verify
fs_ops(ops=[
  {method: "shell.run", cmd: "cp template.toml config.toml"},
  {method: "file.str_replace", path: "config.toml",
    old_str: "PORT = 3000", new_str: "PORT = 8080"},
  {method: "file.read", path: "config.toml", start: 1, lines: 20},
])
```

`shell.run` is the same primitive as top-level `bash` — same timeout,
reap, and stderr separation. Dangerous commands (`rm -rf`, `chmod 777`,
force push, etc.) are rejected inside `fs_ops` and must go through
top-level `bash` so the agent_loop approval gate fires.

## Shell
  bash(cmd="cargo test")                         — run command, compressed output
  bash(cmd="git diff HEAD~1")                    — any shell command

## Search
  search(query="fn main")                        — content search (default mode)
  search(query="*.rs", mode="files")             — file-pattern search (replaces glob)
  search(query="authn", scope="work")            — find needles by concept (replaces near)
  search(query="decision", scope="decisions")    — search decisions log
  search(query="auth flow", scope="transcripts") — search session transcripts (replaces context_search)

Axes: `scope` ∈ {code, files, work, history, decisions, transcripts, all};
`mode` ∈ {literal, regex, glob, semantic, symbol, id}; `expand` ∈ {callers,
history, semantic, attribution}. All optional. The `find`, `grep`, `glob`,
`near`, `recall`, `pitchfork`, `context_search`, `related`, `investigate`
aliases still route via `normalize_tool_name` but are no longer advertised.

## Needles, decisions, substrate (operator overlay — CLI-only)
The work pipeline and substrate fetch are operator verbs, not System ABI.
For inspection inside an agent, route them through `bash(...)` — they do
not have a `mcp__ostk__*` shape. Needle IDs use the unicode arrow prefix:
`'→1612'` (the bare integer is rejected by `ostk show`).

  bash(cmd="ostk show '→1612'")                   — single needle (status, priority, created)
  bash(cmd="ostk work list --status open")        — list open needles
  bash(cmd="ostk work list --priority p0")        — filter by priority (p0/p1/p2/p3)
  bash(cmd="ostk work near 'cache control'")      — concept neighborhood
  bash(cmd="ostk work depends '→1821'")           — dependency graph for a needle
  bash(cmd="ostk work radiate '→1494'")           — frontier rings around a high-degree epic
  bash(cmd="ostk trace '→1612'")                  — attribution chain (needle → commit → spec)
  bash(cmd="ostk show needles")                   — first page of the open queue
  bash(cmd="ostk show threads")                   — active threads
  bash(cmd="ostk show clock")                     — kernel clock
  bash(cmd="ostk show status")                    — register dump

## Substrate fetch (recall family — MCP-exposed)
The `recall` opcode fetches a single substrate record by address; outline
+ search variants give hierarchical and substrate-wide views. These ARE
exposed as MCP tools (transitional under →1801, will eventually formalize
under →1822). Prefer the MCP tool — the bare CLI returns only a header,
the MCP tool returns the full content + attribution payload:

  recall(addr="→1612")                            — fetch needle/decision/path/journal seq by address
  recall_outline(addr="→1821")                    — title + children (epic structure)
  recall_search(query="ABI alignment")            — substrate-wide search (needles + decisions + code + transcripts)

For broad searches across the corpus from outside the kernel (e.g. when
ostk daemon is down), the `mcp__recall__*` family hits the standalone
hybrid index — same shape, separate index.

## Web access (MCP-exposed)
  web_read(url="...")                             — fetch + extract text
  web_links(url="...")                            — extract all links
  web_status(url="...")                           — HTTP status + headers
  fcp-web(query="...")                            — driver-level web ops

## Tack & dispatch (intent resolution + low-level)
  tack(input=":boot")                             — resolve tack grammar to kernel commands
  dispatch(verb="...", args="...")                — low-level verb dispatch primitive
  note(text="...")                                — inline annotation in audit stream
  arrive()                                        — bilateral presence handshake (start of session)
  heartbeat()                                     — agent health monitoring
  handoff(target="...", transition="...")         — switch model + inject transition message

## Context page tier ops (L1.5 cache surface — MCP-exposed)
The address space (per →1783 register-model: L0/L1/L1+/L1.5/L2/L3/L4/L5).
Hot pages live in working context; warm/cold pages live on disk and need
explicit promotion. Don't reach for these unless you've read the spec —
incorrect tier transitions waste cache.

  context_pages(state="hot")                      — list pages by tier
  context_load(name="...")                        — promote into hot context
  context_store(name="...", content="...")        — store as L1.5 cold page
  context_pin(name="...")                         — toggle eviction protection
  context_evict(name="...")                       — explicit evict hot → warm
  context_release(name="...")                     — mark warm (release hot slot)
  context_restore(name="...")                     — promote warm/cold → hot

## Background processes (no native analog — ostk-native)
  spawn(alias="server", cmd="npm run dev", wait_for="ready on port 3000")
  interact(alias="server", action="read_tail", lines=20)

## Coordination & locks
  lock(action="create", name="task-123")         — coordination lock (peer-visible)
  lock(action="release", name="task-123")        — release the lock
  lock(action="watch", name="task-123")          — block until lock is released
  lock(action="status", name="task-123")         — query without blocking
  session(action="list")                         — list active sessions
  session(action="create", name="...")           — create a named session

## Diagnostics
  bash(cmd="ostk mcp diag")                      — reconcile tool surface vs CORE_SEED_LANGUAGE
                                                   (table; or `--json` for hook selftest).
                                                   Exits 1 on any drift.
  bash(cmd="ostk inspect")                       — platform helpers, IPC endpoints, gate leakage
  bash(cmd="ostk profile")                       — ISA opcodes, tokens, TTFT, generation, drift
  bash(cmd="ostk validate")                      — CONTROL/REGISTER/JUDGE validation phase
  bash(cmd="ostk bench")                         — needle-bench cargo tests, scenarios, leaderboard
  bash(cmd="ostk ps")                            — fleet with pid/ppid/pgid/sid columns
  bash(cmd="ostk session-history")               — replay session event log
  session_history()                              — same, MCP shape

## Boot protocol (do this first, every session)
  bash(cmd="ostk boot")                          — read .ostk/ state, run POST checks
  bash(cmd="ostk mcp diag")                      — verify tool surface integrity (optional but cheap)

If a kernel tool errors, don't fall back to native — the kernel invariants
(gen_table, OCC, audit) are exactly what you want preserved. Check the call
syntax and retry with the kernel tool.

# Working directory
  - Never `cd` into the current working directory — it triggers an approval prompt and halts work. Use absolute paths instead.
  - Only use `cd` when moving to a genuinely different directory the user requested.
