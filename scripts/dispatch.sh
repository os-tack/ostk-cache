#!/usr/bin/env bash
# ostk-cache hook dispatch — installed by ostk-cache hooks install.
#
# Forwards a Claude Code lifecycle event to the local ostk-cache proxy.
# Designed to fail quietly: if the proxy isn't running, hook exits 0 so
# Claude Code is never blocked.
#
# Usage: dispatch.sh <EventType>
#   EventType ∈ {SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop}
#
# Reads:
#   - $1                      — event type (passed by settings.json command)
#   - stdin                   — Claude Code's per-event JSON payload
#                               (provides session_id; may also carry tool_use,
#                                user prompt, etc. depending on event)
#   - $CLAUDE_PROJECT_DIR     — project root, used to derive workspace_id
#   - $CLAUDE_SESSION_ID      — fallback for session_id if stdin lacks it
#   - $OSTK_CACHE_PROXY_URL   — override proxy base URL (default
#                               http://127.0.0.1:8080)
#
# Writes nothing on success or failure.

set +e

EVENT_TYPE="${1:?event type required as first arg}"
PROXY_URL="${OSTK_CACHE_PROXY_URL:-http://127.0.0.1:8080}"

# Read stdin (Claude Code may or may not pipe JSON depending on event).
STDIN_JSON=""
if [ ! -t 0 ]; then
    STDIN_JSON=$(cat)
fi

# Resolve session_id: stdin JSON first (canonical per Claude Code hooks spec),
# then env var fallback, then literal "unknown".
SID=""
if [ -n "$STDIN_JSON" ] && command -v jq >/dev/null 2>&1; then
    SID=$(printf '%s' "$STDIN_JSON" | jq -r '.session_id // empty' 2>/dev/null)
fi
[ -z "$SID" ] && SID="${CLAUDE_SESSION_ID:-unknown}"

# Resolve workspace_id: git origin URL hash, falling back to canonical
# project-dir hash, falling back to "unknown".
WS=""
if [ -n "$CLAUDE_PROJECT_DIR" ]; then
    WS=$(git -C "$CLAUDE_PROJECT_DIR" config --get remote.origin.url 2>/dev/null \
         | shasum -a 256 | cut -c1-16)
    if [ -z "$WS" ]; then
        WS=$(printf '%s' "$CLAUDE_PROJECT_DIR" | shasum -a 256 | cut -c1-16)
    fi
fi
[ -z "$WS" ] && WS="unknown"

# Forward stdin payload if jq is available; else send empty payload.
PAYLOAD="null"
if [ -n "$STDIN_JSON" ] && command -v jq >/dev/null 2>&1; then
    # Echo the full stdin JSON as the payload field.
    PAYLOAD=$(printf '%s' "$STDIN_JSON" | jq -c . 2>/dev/null || printf 'null')
fi

# Build request body.
if command -v jq >/dev/null 2>&1; then
    BODY=$(jq -cn \
        --arg t "$EVENT_TYPE" \
        --arg w "$WS" \
        --arg s "$SID" \
        --argjson p "$PAYLOAD" \
        '{type:$t, workspace_id:$w, session_id:$s, payload:$p}')
else
    # No jq — escape minimally (assumes WS/SID don't contain quotes; both
    # come from sha256 hex or "unknown" so this is safe).
    BODY="{\"type\":\"$EVENT_TYPE\",\"workspace_id\":\"$WS\",\"session_id\":\"$SID\"}"
fi

# Truly fire-and-forget: detach the curl into the background so the script
# returns to Claude Code in milliseconds regardless of proxy responsiveness.
# A synchronous curl with -m 2 still costs up to 2s wall-clock per hook fire,
# which compounds badly across the hook lattice on tool-heavy sessions.
#
# Pattern:
#   - Wrap the curl in a brace group.
#   - Redirect all three stdio to /dev/null so the parent has no fds to wait on.
#   - `&` puts it in the background.
#   - `disown` removes it from the shell's job table so the parent shell
#     doesn't SIGHUP it on exit. (`|| true` because some shells lack disown.)
{
    curl -fsS --connect-timeout 1 -m 2 \
        -X POST "$PROXY_URL/hook/event" \
        -H 'content-type: application/json' \
        -d "$BODY" \
        >/dev/null 2>&1 || true
} </dev/null >/dev/null 2>&1 &
disown 2>/dev/null || true

exit 0
