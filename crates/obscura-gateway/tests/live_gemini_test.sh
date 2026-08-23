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
echo "=== 1. Multi-turn conversation ==="
echo "--- Turn 1 ---"
RESP1=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "gemini-3.5-flash",
        "messages": [
            {"role": "user", "content": "Say exactly 3 words"}
        ]
    }')
echo "$RESP1" | python3 -m json.tool 2>/dev/null || echo "$RESP1"

SESSION_URL=$(echo "$RESP1" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(d.get('session_url','') or '')
" 2>/dev/null || echo "")
echo "Session URL: $SESSION_URL"

if [ -n "$SESSION_URL" ]; then
    echo ""
    echo "--- Turn 2 (continuation) ---"
    RESP2=$(curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" \
        -H "Content-Type: application/json" \
        -d "$(python3 -c "
import json
d={
    'model': 'gemini-3.5-flash',
    'messages': [
        {'role': 'user', 'content': 'Say exactly 2 more words'}
    ],
    'session_url': '$SESSION_URL'
}
print(json.dumps(d))
")")
    echo "$RESP2" | python3 -m json.tool 2>/dev/null || echo "$RESP2"
fi

echo ""
echo "=== 2. Tool calls ==="
RESP3=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "gemini-3.5-flash",
        "messages": [
            {"role": "user", "content": "What is the weather in Paris? Use the get_weather function."}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current temperature for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "location": {"type": "string"}
                        },
                        "required": ["location"]
                    }
                }
            }
        ]
    }')
echo "$RESP3" | python3 -m json.tool 2>/dev/null || echo "$RESP3"

echo ""
echo "=== 3. File upload (data URI) ==="
RESP4=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "gemini-3.5-flash",
        "messages": [
            {
                "role": "user",
                "content": [
                    {"type": "text", "text": "What is in this image? Describe it briefly."},
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAAKCAYAAAB10jRKAAAADklEQVR42mP8/5+hnoEIAAO4Af3t2TWkAAAAAElFTkSuQmCC"
                        }
                    }
                ]
            }
        ]
    }')
echo "$RESP4" | python3 -m json.tool 2>/dev/null || echo "$RESP4"

echo ""
echo "=== 4. Streaming ==="
echo "--- First 20 lines of stream ---"
curl -s -N -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" \
    -H "Content-Type: application/json" \
    -d '{
        "model": "gemini-3.5-flash",
        "messages": [
            {"role": "user", "content": "Count from 1 to 5"}
        ],
        "stream": true
    }' 2>/dev/null | head -20

echo ""
echo ""
echo "=== ALL TESTS COMPLETED ==="
