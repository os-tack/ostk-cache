#!/usr/bin/env bash
# docs/smoke-hook.sh — End-to-end smoke test for the /hook/event endpoint.
#
# Simulates Claude Code firing the five hook events in a representative
# session arc (one user prompt -> one tool roundtrip -> stop) and asserts
# that .l1.5/hooks.jsonl ends up with the expected sequence of rows.
#
# This is the closest we can get to a real Claude Code session without
# actually running Claude. The HOOKS.md settings.json snippet boils each
# hook down to a curl POST against /hook/event; this script issues the
# same POSTs in the order Claude Code would fire them, then validates
# the resulting JSONL.
#
# Usage:
#   ./docs/smoke-hook.sh                # uses default port 18080
#   PROXY_PORT=9999 ./docs/smoke-hook.sh
#
# Exits 0 on success, non-zero on any assertion failure. Intended to be
# safe to run from a clean checkout: spawns its own proxy in a tempdir,
# tears it down, and prints a short verification report at the end.

set -euo pipefail

PROXY_PORT="${PROXY_PORT:-18080}"
URL="http://127.0.0.1:${PROXY_PORT}/hook/event"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${REPO_ROOT}/target/debug/ostk-cache"

if [ ! -x "$BIN" ]; then
    echo "[smoke] building ostk-cache binary..." >&2
    (cd "$REPO_ROOT" && cargo build --bin ostk-cache)
fi

TMPDIR="$(mktemp -d -t ostk-cache-smoke-XXXXXX)"
trap 'rm -rf "$TMPDIR"; [ -n "${PROXY_PID:-}" ] && kill "$PROXY_PID" 2>/dev/null; wait 2>/dev/null || true' EXIT

echo "[smoke] proxy cwd: $TMPDIR"
echo "[smoke] proxy port: $PROXY_PORT"

# Bind a unique port; refuse if already taken.
if lsof -nP -iTCP:"$PROXY_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "[smoke] FAIL: port $PROXY_PORT already in use; set PROXY_PORT=<free-port>" >&2
    exit 2
fi

# --- 1. Spawn proxy --------------------------------------------------------
# Run the proxy with the tempdir as its cwd so .l1.5/ lands there, not in
# whatever directory invoked this script.
cd "$TMPDIR"
PROXY_PORT="$PROXY_PORT" "$BIN" >"$TMPDIR/proxy.log" 2>&1 &
PROXY_PID=$!

# Wait for the port to be listening.
for _ in $(seq 1 50); do
    if (echo > /dev/tcp/127.0.0.1/"$PROXY_PORT") 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if ! (echo > /dev/tcp/127.0.0.1/"$PROXY_PORT") 2>/dev/null; then
    echo "[smoke] FAIL: proxy did not come up on $PROXY_PORT within 5s" >&2
    cat "$TMPDIR/proxy.log" >&2
    exit 3
fi

echo "[smoke] proxy up (pid=$PROXY_PID)"

# --- 2. Fire the five hooks in Claude Code lifecycle order -----------------
WS="smoke-workspace"
SID="smoke-session-1"

post() {
    local kind=$1
    local payload=${2:-null}
    local body
    if [ "$payload" = "null" ]; then
        body=$(printf '{"type":"%s","workspace_id":"%s","session_id":"%s"}' "$kind" "$WS" "$SID")
    else
        body=$(printf '{"type":"%s","workspace_id":"%s","session_id":"%s","payload":%s}' "$kind" "$WS" "$SID" "$payload")
    fi
    code=$(curl -s -o "$TMPDIR/last_resp.json" -w "%{http_code}" \
        -X POST "$URL" -H 'content-type: application/json' -d "$body")
    if [ "$code" != "200" ]; then
        echo "[smoke] FAIL: $kind returned $code, body: $(cat "$TMPDIR/last_resp.json")" >&2
        exit 4
    fi
    echo "[smoke]   $kind -> 200"
}

echo "[smoke] firing lifecycle hooks (one tool roundtrip)..."
post SessionStart      '{"reason":"startup"}'
post UserPromptSubmit  '{"prompt":"smoke test prompt"}'
post PreToolUse        '{"tool":"bash","input":"ls"}'
post PostToolUse       '{"tool":"bash","exit_code":0}'
post Stop              '{"reason":"end_turn"}'

# --- 3. Assert hooks.jsonl content -----------------------------------------
HOOKS="$TMPDIR/.l1.5/hooks.jsonl"
if [ ! -f "$HOOKS" ]; then
    echo "[smoke] FAIL: $HOOKS does not exist after 5 events" >&2
    exit 5
fi

ROW_COUNT=$(grep -c . "$HOOKS")
if [ "$ROW_COUNT" -ne 5 ]; then
    echo "[smoke] FAIL: expected 5 rows in hooks.jsonl, got $ROW_COUNT" >&2
    cat "$HOOKS" >&2
    exit 6
fi

# Validate each row: parses as JSON, has the right event_type in order.
EXPECTED=("SessionStart" "UserPromptSubmit" "PreToolUse" "PostToolUse" "Stop")
i=0
while IFS= read -r line; do
    # Bash-only JSON field check (no jq dependency for portability).
    if ! python3 -c "import json,sys; json.loads(sys.argv[1])" "$line" 2>/dev/null; then
        echo "[smoke] FAIL: row $i is not valid JSON: $line" >&2
        exit 7
    fi
    et=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['event_type'])" "$line")
    if [ "$et" != "${EXPECTED[$i]}" ]; then
        echo "[smoke] FAIL: row $i event_type was $et, expected ${EXPECTED[$i]}" >&2
        exit 8
    fi
    ws=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['workspace_id'])" "$line")
    sid=$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['session_id'])" "$line")
    if [ "$ws" != "$WS" ] || [ "$sid" != "$SID" ]; then
        echo "[smoke] FAIL: row $i identity mismatch: ws=$ws sid=$sid (expected $WS/$SID)" >&2
        exit 9
    fi
    i=$((i+1))
done < "$HOOKS"

# Stop should also produce manifest.json.
MANIFEST="$TMPDIR/.l1.5/manifest.json"
if [ ! -f "$MANIFEST" ]; then
    echo "[smoke] FAIL: Stop event should have written $MANIFEST" >&2
    exit 10
fi
if ! python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$MANIFEST" 2>/dev/null; then
    echo "[smoke] FAIL: $MANIFEST is not valid JSON" >&2
    exit 11
fi

# --- 4. Verification report ------------------------------------------------
echo
echo "[smoke] VERIFICATION REPORT"
echo "[smoke] ----------------------------------------"
echo "[smoke] proxy log (last 20 lines):"
tail -20 "$TMPDIR/proxy.log" | sed 's/^/[smoke]   /'
echo "[smoke] ----------------------------------------"
echo "[smoke] hooks.jsonl rows ($ROW_COUNT):"
sed 's/^/[smoke]   /' "$HOOKS"
echo "[smoke] ----------------------------------------"
echo "[smoke] manifest.json:"
sed 's/^/[smoke]   /' "$MANIFEST"
echo "[smoke] ----------------------------------------"
echo "[smoke] PASS: 5/5 lifecycle events recorded in expected order"
exit 0
