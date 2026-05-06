# Claude Code hooks → ostk-cache

This document explains how to wire Claude Code's [hook
system](https://docs.claude.com/en/docs/claude-code/hooks) to the
ostk-cache proxy so each session lifecycle event is recorded as a row
in `.l1.5/hooks.jsonl`.

The proxy exposes `POST /hook/event` (default
`http://127.0.0.1:8080/hook/event`). Each hook configured in Claude
Code's `settings.json` becomes a small `curl` command that POSTs a
JSON body describing the event. The proxy maps the event to a
`HookEventKind` and dispatches it through `DaemonAdapter`, which
appends a row to `.l1.5/hooks.jsonl` (and writes a `manifest.json`
snapshot on `Stop`).

## 1. Start the proxy

```bash
cargo run --bin ostk-cache
# Capture Proxy running on 127.0.0.1:8080
```

Set `PROXY_PORT` to bind a different port; the hook URLs below use
`8080`.

## 2. Wire `~/.claude/settings.json`

The `hooks` key is keyed by event name. Each value is an array of
`{matcher, hooks: [{type: "command", command: "..."}]}` rules. The
command runs in a shell with environment variables Claude exports
(`CLAUDE_PROJECT_DIR`, `CLAUDE_SESSION_ID`, ...) and any stdin Claude
provides for that event.

The snippet below derives a stable `workspace_id` from the project's
git origin URL (falling back to a hash of `CLAUDE_PROJECT_DIR`),
reads `CLAUDE_SESSION_ID` from the environment, and POSTs each event
to the proxy. Add it to `settings.json` under the top-level `hooks`
key:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "WS=$(git -C \"$CLAUDE_PROJECT_DIR\" config --get remote.origin.url 2>/dev/null | shasum -a 256 | cut -c1-16); [ -z \"$WS\" ] && WS=$(printf '%s' \"$CLAUDE_PROJECT_DIR\" | shasum -a 256 | cut -c1-16); curl -fsS -X POST http://127.0.0.1:8080/hook/event -H 'content-type: application/json' -d \"{\\\"type\\\":\\\"SessionStart\\\",\\\"workspace_id\\\":\\\"$WS\\\",\\\"session_id\\\":\\\"${CLAUDE_SESSION_ID:-unknown}\\\"}\" >/dev/null || true"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "WS=$(git -C \"$CLAUDE_PROJECT_DIR\" config --get remote.origin.url 2>/dev/null | shasum -a 256 | cut -c1-16); [ -z \"$WS\" ] && WS=$(printf '%s' \"$CLAUDE_PROJECT_DIR\" | shasum -a 256 | cut -c1-16); curl -fsS -X POST http://127.0.0.1:8080/hook/event -H 'content-type: application/json' -d \"{\\\"type\\\":\\\"UserPromptSubmit\\\",\\\"workspace_id\\\":\\\"$WS\\\",\\\"session_id\\\":\\\"${CLAUDE_SESSION_ID:-unknown}\\\"}\" >/dev/null || true"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "WS=$(git -C \"$CLAUDE_PROJECT_DIR\" config --get remote.origin.url 2>/dev/null | shasum -a 256 | cut -c1-16); [ -z \"$WS\" ] && WS=$(printf '%s' \"$CLAUDE_PROJECT_DIR\" | shasum -a 256 | cut -c1-16); curl -fsS -X POST http://127.0.0.1:8080/hook/event -H 'content-type: application/json' -d \"{\\\"type\\\":\\\"PreToolUse\\\",\\\"workspace_id\\\":\\\"$WS\\\",\\\"session_id\\\":\\\"${CLAUDE_SESSION_ID:-unknown}\\\"}\" >/dev/null || true"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "WS=$(git -C \"$CLAUDE_PROJECT_DIR\" config --get remote.origin.url 2>/dev/null | shasum -a 256 | cut -c1-16); [ -z \"$WS\" ] && WS=$(printf '%s' \"$CLAUDE_PROJECT_DIR\" | shasum -a 256 | cut -c1-16); curl -fsS -X POST http://127.0.0.1:8080/hook/event -H 'content-type: application/json' -d \"{\\\"type\\\":\\\"PostToolUse\\\",\\\"workspace_id\\\":\\\"$WS\\\",\\\"session_id\\\":\\\"${CLAUDE_SESSION_ID:-unknown}\\\"}\" >/dev/null || true"
          }
        ]
      }
    ],
    "Stop": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "WS=$(git -C \"$CLAUDE_PROJECT_DIR\" config --get remote.origin.url 2>/dev/null | shasum -a 256 | cut -c1-16); [ -z \"$WS\" ] && WS=$(printf '%s' \"$CLAUDE_PROJECT_DIR\" | shasum -a 256 | cut -c1-16); curl -fsS -X POST http://127.0.0.1:8080/hook/event -H 'content-type: application/json' -d \"{\\\"type\\\":\\\"Stop\\\",\\\"workspace_id\\\":\\\"$WS\\\",\\\"session_id\\\":\\\"${CLAUDE_SESSION_ID:-unknown}\\\"}\" >/dev/null || true"
          }
        ]
      }
    ]
  }
}
```

### Notes on the snippet

- `WS=...` derives a 16-char workspace id from the git origin URL. If
  the project has no remote (or `git` isn't on `PATH`), it falls back
  to hashing `CLAUDE_PROJECT_DIR`. The proxy doesn't enforce any
  particular shape — any string is accepted.
- `${CLAUDE_SESSION_ID:-unknown}` reads the session id Claude Code
  exports. If it isn't set, we fall back to the literal `unknown` so
  the hook never breaks the JSON body.
- `curl -fsS ... >/dev/null || true`: `-f` fails on HTTP errors,
  `-sS` keeps it quiet but surfaces real network errors, and the
  trailing `|| true` ensures a stalled or absent proxy never blocks
  the Claude Code session.
- `shasum -a 256` is available on macOS and most Linux distros. If
  you only have `sha256sum`, swap accordingly. The exact hash
  function is irrelevant — just be consistent across machines.
- The proxy also accepts snake_case event types (`session_start`,
  `user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `stop`) for
  callers that prefer that style.

### Sending payloads (optional)

Each Claude Code hook receives event-specific JSON on stdin. To
forward it as the `payload` field, swap `curl ... -d "..."` for
something like:

```bash
PAYLOAD=$(cat)
curl -fsS -X POST http://127.0.0.1:8080/hook/event \
  -H 'content-type: application/json' \
  -d "$(jq -n --arg t SessionStart --arg w \"$WS\" --arg s \"${CLAUDE_SESSION_ID:-unknown}\" --argjson p \"$PAYLOAD\" '{type:$t,workspace_id:$w,session_id:$s,payload:$p}')"
```

This requires `jq`. The proxy treats `payload` as opaque
`serde_json::Value` — it's stored verbatim in the JSONL row.

## 3. Verify

After installing the snippet, start the proxy in one terminal:

```bash
cd ~/your-project
cargo run --bin ostk-cache --manifest-path ~/projects/ostk-cache/Cargo.toml
```

In another terminal, run a curl smoke test:

```bash
curl -fsS -X POST http://127.0.0.1:8080/hook/event \
  -H 'content-type: application/json' \
  -d '{"type":"SessionStart","workspace_id":"smoke","session_id":"s1"}'
# {"ok":true}
```

The proxy stdout should print `Binding file_id / firmware
materialization`, and `.l1.5/hooks.jsonl` (relative to the proxy's
cwd) will gain a row.

Then start a Claude Code session in the same project. After issuing
a prompt and stopping, the file should contain at least
`SessionStart`, `UserPromptSubmit`, several
`PreToolUse`/`PostToolUse`, and a final `Stop`.

## 4. Inspect `.l1.5/hooks.jsonl`

`hooks.jsonl` is JSONL (one JSON object per line) so it streams
cleanly through the usual tools:

```bash
# Tail live as a session runs
tail -f .l1.5/hooks.jsonl

# Count events by kind
jq -r .event_type .l1.5/hooks.jsonl | sort | uniq -c

# Filter to one session
jq 'select(.session_id == "abcd1234")' .l1.5/hooks.jsonl

# Pretty-print the most recent 20 events
tail -n 20 .l1.5/hooks.jsonl | jq .
```

Each row has the shape:

```json
{
  "timestamp": 1730000000,
  "workspace_id": "1a2b3c4d5e6f7890",
  "session_id": "abcd1234",
  "event_type": "PreToolUse",
  "payload": null
}
```

`Stop` events additionally produce/refresh `.l1.5/manifest.json`
with the most recent persistence timestamp and the workspace +
session that triggered it.

## 5. Troubleshooting

- **Hook fires, file doesn't grow**: confirm the proxy was started
  in the same working directory you're inspecting — `.l1.5/` is
  created relative to the proxy process's cwd, not the project's.
- **`curl: (7) Failed to connect`**: the proxy isn't running or
  `PROXY_PORT` isn't `8080`. The `|| true` in the hook keeps Claude
  Code working; you'll just lose those events.
- **`{"type":"error","error":{"type":"invalid_request_error",...}}`**:
  the `type` field doesn't match a known event kind. Accepted
  values are the five PascalCase names (`SessionStart`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`) and their
  snake_case equivalents.
- **Session id is `unknown`**: Claude Code didn't export
  `CLAUDE_SESSION_ID` for this hook event. This happens for some
  early-lifecycle hooks; the proxy still records the row so you can
  audit whether all expected events fired.
