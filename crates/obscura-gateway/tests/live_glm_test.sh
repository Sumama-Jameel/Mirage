#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
SERVER_BIN="$REPO_ROOT/target/debug/obscura-gateway"
API_KEY="obscura-local"
BASE="http://127.0.0.1:${OBSCURA_PORT:-3000}"
AUTH="Authorization: Bearer $API_KEY"

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

echo "=== Starting server ==="
OBSCURA_GATEWAY__AUTH__API_KEY=$API_KEY $SERVER_BIN &
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
echo "=== 1. GLM model discovery ==="
curl -s -H "$AUTH" "$BASE/v1/models" | python3 -c "
import json, sys
data = json.load(sys.stdin)
glm = [m for m in data.get('data', []) if 'glm' in m['id']]
for m in glm:
    print(f\"  {m['id']} (owned_by: {m['owned_by']})\")
" || echo "model discovery failed"

echo ""
echo "=== 2. Single-turn chat with glm-5.2 ==="
RESP1=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "glm-5.2",
        "messages": [
            {"role": "user", "content": "Say exactly 3 words"}
        ]
    }')
echo "$RESP1" | python3 -m json.tool 2>/dev/null || echo "$RESP1"

SESSION_URL=$(echo "$RESP1" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    print(data.get('session_url', ''))
except Exception:
    pass
" 2>/dev/null || echo "")

if [ -z "$SESSION_URL" ]; then
    echo "WARN: no session_url in response; multi-turn test will use a fresh chat"
else
    echo ""
    echo "=== 3. Multi-turn continuation via session_url ==="
    echo "--- Turn 2 ---"
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" \
        -H "Content-Type: application/json" \
        -d "$(python3 -c "
import json
print(json.dumps({
    'model': 'glm-5.2',
    'session_url': '$SESSION_URL',
    'messages': [
        {'role': 'user', 'content': 'Now say exactly 5 words'}
    ]
}))
")" | python3 -m json.tool 2>/dev/null || true
fi

echo ""
echo "=== 4. Streaming chat ==="
curl -s -N -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "glm-5.2",
        "stream": true,
        "messages": [
            {"role": "user", "content": "Count from 1 to 3"}
        ]
    }' | head -c 2000
echo ""

echo ""
echo "=== 5. Tool calling (prompt-injected) ==="
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "glm-5.2",
        "messages": [
            {"role": "user", "content": "What is the weather in Tokyo?"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get current weather for a city",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"}
                    },
                    "required": ["city"]
                }
            }
        }]
    }' | python3 -m json.tool 2>/dev/null || true

echo ""
echo "=== 6. Validation: unknown model ==="
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "glm-99-nonexistent",
        "messages": [{"role": "user", "content": "hi"}]
    }' | python3 -m json.tool 2>/dev/null || true

echo ""
echo "=== All tests done ==="
