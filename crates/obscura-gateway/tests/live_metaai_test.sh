#!/usr/bin/env bash
# Live test for the Meta AI (muse-spark) DGW provider.
#
# The provider talks to the Ecto-era DGW WebSocket with native browser-session
# auth: meta.ai cookies (`ecto_1_sess` + `datr`) plus the page-injected
# `ecto1:` WebSocket token. There is no anonymous mode.
#
# Run from a machine whose warmed browser profile is logged in to meta.ai, or
# export a fresh token:
#   export META_AI_ECTO1_TOKEN="<hex from DevTools/console>"
#
# When auth is missing every chat request fails with a clear Auth/Provider
# error; the script reports that as the provider failing closed (expected).
# meta.ai also geo-blocks some regions (including many datacenter IPs).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
SERVER_BIN="$REPO_ROOT/target/debug/obscura-gateway"
API_KEY="obscura-local"
BASE="http://127.0.0.1:3000"
AUTH="Authorization: Bearer $API_KEY"

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Starting server ==="
# Pin the port to 3000 regardless of obscura-gateway.toml (which may set 8080),
# so BASE above matches the bound address. Env overrides the config file.
OBSCURA_GATEWAY__AUTH__API_KEY=$API_KEY \
OBSCURA_GATEWAY__SERVER__PORT=3000 \
$SERVER_BIN &
SERVER_PID=$!

echo "Waiting for server..."
for i in $(seq 1 90); do
    if curl -s -o /dev/null -w "%{http_code}" -H "$AUTH" --max-time 2 "$BASE/v1/models" 2>/dev/null | grep -q 200; then
        echo "Server ready after ${i}s"
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "Server died during startup"
        exit 1
    fi
    sleep 1
done

echo ""
echo "=== 1. Meta AI model discovery ==="
curl -s -H "$AUTH" "$BASE/v1/models" | python3 -c "
import json, sys
data = json.load(sys.stdin)
meta = [m for m in data.get('data', []) if 'muse' in m['id']]
for m in meta:
    print(f\"  {m['id']} (owned_by: {m['owned_by']})\")
" || echo "model discovery failed"

echo ""
echo "=== 2. Single-turn chat ==="
RESP1=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "muse-spark",
        "messages": [
            {"role": "user", "content": "Say exactly 3 words"}
        ]
    }')
echo "$RESP1" | python3 -m json.tool 2>/dev/null || echo "$RESP1"

SESSION_URL=$(echo "$RESP1" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    print(data.get('session_url') or '')
except Exception:
    print('')
" 2>/dev/null || true)
echo ""
echo "session_url: $SESSION_URL"

echo ""
echo "=== 3. Streaming chat ==="
curl -s -N -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "muse-spark",
        "stream": true,
        "messages": [
            {"role": "user", "content": "Count from 1 to 3"}
        ]
    }' | head -c 2000
echo ""

echo ""
echo "=== 4. Continuation via session_url ==="
if [ -n "$SESSION_URL" ]; then
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" \
        -H "Content-Type: application/json" \
        -d "{
            \"model\": \"muse-spark\",
            \"session_url\": \"$SESSION_URL\",
            \"messages\": [
                {\"role\": \"user\", \"content\": \"Now what is 1 + 1?\"}
            ]
        }" | python3 -m json.tool 2>/dev/null || true
else
    echo "(skipped: no session_url from turn 2, likely auth/geo-block failure)"
fi

echo ""
echo "=== 5. Validation: unknown model ==="
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "meta-ai-99",
        "messages": [{"role": "user", "content": "hi"}]
    }' | python3 -m json.tool 2>/dev/null || true

echo ""
echo "=== 6. Validation: search flag fails closed ==="
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "muse-spark",
        "search": true,
        "messages": [{"role": "user", "content": "hi"}]
    }' | python3 -m json.tool 2>/dev/null || true

echo ""
echo "=== All tests done ==="
