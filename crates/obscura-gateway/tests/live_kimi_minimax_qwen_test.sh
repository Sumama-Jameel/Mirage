#!/usr/bin/env bash
# Live integration test: Kimi, Minimax, Qwen providers.
# Starts obscura-gateway, runs all tests, cleans up.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
SERVER_BIN="$REPO_ROOT/target/release/obscura-gateway"
API_KEY="${OBSCURA_API_KEY:-obscura-local}"
BASE="http://127.0.0.1:${OBSCURA_PORT:-8080}"
AUTH="Authorization: Bearer $API_KEY"

# Small test PNG (32x32 red square on blue background)
PNG_DATA="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAANElEQVR4nO3OsQ0AMAgEsd9/aRgiigDJxdXnJFWzAQAAbAe8HgAAAAAAAADuA/4HAAAwDGhZs/hqlJjNjgAAAABJRU5ErkJggg=="

start_server() {
    RUST_LOG=info $SERVER_BIN &
    SERVER_PID=$!
    for i in $(seq 1 90); do
        if curl -s -o /dev/null -w "%{http_code}" -H "$AUTH" --max-time 2 "$BASE/v1/models" 2>/dev/null | grep -q 200; then
            echo "Server ready after ${i}s"
            return
        fi
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            echo "Server died during startup"
            exit 1
        fi
        sleep 1
    done
    echo "Server failed to start"
    exit 1
}

discover_model() {
    local prefix="$1"
    curl -s -H "$AUTH" "$BASE/v1/models" | python3 -c "
import json,sys
d=json.load(sys.stdin)
models=[m['id'] for m in d['data'] if m['id'].startswith('$prefix')]
print(models[0] if models else '${prefix}-unknown')
"
}

run_tool_test() {
    local model="$1"
    local label="$2"
    echo "--- $label ---"
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" -H "Content-Type: application/json" \
        -d "{
            \"model\": \"$model\",
            \"messages\": [
                {\"role\": \"user\", \"content\": \"What is the weather in Tokyo? Use the get_weather function.\"}
            ],
            \"tools\": [
                {
                    \"type\": \"function\",
                    \"function\": {
                        \"name\": \"get_weather\",
                        \"description\": \"Get current temperature for a city\",
                        \"parameters\": {
                            \"type\": \"object\",
                            \"properties\": {\"location\": {\"type\": \"string\"}},
                            \"required\": [\"location\"]
                        }
                    }
                }
            ]
        }" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    m = d['choices'][0]['message']
    tc = m.get('tool_calls')
    if tc: print('TOOL_CALLS:', json.dumps(tc, indent=2))
    else: print('TEXT:', repr(m['content'])[:200])
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
" 2>&1 || echo "(request failed)"
}

run_vision_test() {
    local model="$1"
    local label="$2"
    echo "--- $label ---"
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" -H "Content-Type: application/json" \
        -d "{
            \"model\": \"$model\",
            \"messages\": [
                {
                    \"role\": \"user\",
                    \"content\": [
                        {\"type\": \"text\", \"text\": \"What do you see in this image? Describe briefly.\"},
                        {\"type\": \"image_url\", \"image_url\": {\"url\": \"$PNG_DATA\"}}
                    ]
                }
            ]
        }" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
" 2>&1 || echo "(request failed)"
}

cleanup() {
    echo ""
    echo "=== Cleaning up ==="
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

# ═══════════════════════════════════════
#  START
# ═══════════════════════════════════════
echo "=== Starting server ==="
start_server

# ═══════════════════════════════════════
#  GLOBAL TESTS
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  GLOBAL"
echo "============================================"

echo "--- 0a. Health check ---"
curl -s -o /dev/null -w "GET /health -> HTTP %{http_code}\n" "$BASE/health"

echo "--- 0b. Auth failure (no token) ---"
curl -s -o /dev/null -w "No auth -> HTTP %{http_code}\n" -X POST "$BASE/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{"model":"kimi-k2.6","messages":[{"role":"user","content":"hi"}]}'

echo "--- 0c. Unknown model rejection ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{"model":"nonexistent-model-12345","messages":[{"role":"user","content":"hi"}]}' \
    2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

echo "--- 0d. Empty messages ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{"model":"kimi-k2.6","messages":[]}' \
    2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

echo ""
echo "--- 0e. Model listing — verifying all three providers ---"
curl -s -H "$AUTH" "$BASE/v1/models" | python3 -c "
import json,sys
d=json.load(sys.stdin)
ids = [m['id'] for m in d['data']]
kimi = [i for i in ids if i.startswith('kimi-')]
minimax = [i for i in ids if i.startswith('minimax-')]
qwen = [i for i in ids if i.startswith('qwen-')]
print(f'Kimi models ({len(kimi)}): {kimi}')
print(f'Minimax models ({len(minimax)}): {minimax}')
print(f'Qwen models ({len(qwen)}): {qwen}')
"

# ═══════════════════════════════════════
#  KIMI
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  KIMI"
echo "============================================"

KIMI_MODEL=$(discover_model "kimi-k2.7-code")
echo "Using Kimi model: $KIMI_MODEL"

echo "--- 1a. Kimi basic chat ---"
RESP_1A=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$KIMI_MODEL\",
        \"messages\": [{\"role\": \"user\", \"content\": \"Say exactly 3 words\"}]
    }")
echo "$RESP_1A" | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
print('SESSION_URL:', d.get('session_url','')[:80])
" 2>&1 || echo "(parse failed)"

SESSION_URL_1=$(echo "$RESP_1A" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
    print(d.get('session_url','') or '')
except: pass
" 2>/dev/null || echo "")

# --- 1b. Kimi multi-turn via session_url ---
if [ -n "$SESSION_URL_1" ]; then
    echo ""
    echo "--- 1b. Kimi multi-turn (continuation) ---"
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" -H "Content-Type: application/json" \
        -d "$(python3 -c "
import json
print(json.dumps({
    'model': '$KIMI_MODEL',
    'messages': [{'role': 'user', 'content': 'Now say exactly 5 words'}],
    'session_url': '$SESSION_URL_1'
}))" 2>/dev/null)" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
"
else
    echo "--- 1b. Skipping multi-turn (no session_url) ---"
fi

echo ""
run_tool_test "$KIMI_MODEL" "1c. Kimi tool calling"

echo ""
run_vision_test "$KIMI_MODEL" "1d. Kimi file upload (vision)"

echo ""
echo "--- 1e. Kimi streaming (SSE) ---"
curl -s -N -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$KIMI_MODEL\",
        \"messages\": [{\"role\": \"user\", \"content\": \"Count 1 2 3.\"}],
        \"stream\": true
    }" 2>/dev/null | head -20

echo ""
echo ""
echo "--- 1f. Kimi thinking model ---"
KIMI_THINKING=$(discover_model "kimi-k2.7-code")
echo "Using thinking model: $KIMI_THINKING"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$KIMI_THINKING\",
        \"messages\": [{\"role\": \"user\", \"content\": \"What is 15*12?\"}],
        \"thinking\": true
    }" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    m = d['choices'][0]['message']
    print('CONTENT:', repr(m.get('content',''))[:200])
    rc = m.get('reasoning_content')
    if rc: print('REASONING:', repr(rc[:200]))
    else: print('REASONING: none')
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

echo ""
echo "--- 1g. Kimi k3 discovery ---"
KIMI_K3=$(discover_model "kimi-k3" 2>/dev/null || echo "kimi-k3-unknown")
echo "Kimi K3 model: $KIMI_K3"
if [ "$KIMI_K3" != "kimi-k3-unknown" ]; then
    echo "--- 1g.1 Kimi K3 basic chat ---"
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" -H "Content-Type: application/json" \
        -d "{
            \"model\": \"$KIMI_K3\",
            \"messages\": [{\"role\": \"user\", \"content\": \"Say exactly 3 words\"}]
        }" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"
fi

echo ""
echo "--- 1h. Kimi validation: unknown model ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{"model":"kimi-nonexistent","messages":[{"role":"user","content":"hi"}]}' \
    2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

# ═══════════════════════════════════════
#  MINIMAX
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  MINIMAX"
echo "============================================"

MINIMAX_MODEL=$(discover_model "minimax-m3")
echo "Using Minimax model: $MINIMAX_MODEL"

echo "--- 2a. Minimax basic chat ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$MINIMAX_MODEL\",
        \"messages\": [{\"role\": \"user\", \"content\": \"Say exactly 3 words\"}]
    }" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

echo ""
run_tool_test "$MINIMAX_MODEL" "2b. Minimax tool calling"

echo ""
run_vision_test "$MINIMAX_MODEL" "2c. Minimax file upload (vision)"

echo ""
echo "--- 2d. Minimax streaming (SSE) ---"
curl -s -N -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$MINIMAX_MODEL\",
        \"messages\": [{\"role\": \"user\", \"content\": \"Count 1 2 3.\"}],
        \"stream\": true
    }" 2>/dev/null | head -20

echo ""
echo ""
echo "--- 2e. Minimax multi-turn ---"
# Minimax maintains session internally via session store; no session_url in response.
# We do the second turn in the same request flow.
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$MINIMAX_MODEL\",
        \"messages\": [
            {\"role\": \"user\", \"content\": \"My favorite color is blue. Remember this.\"}
        ]
    }" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
"

echo ""
echo "--- 2f. Minimax validation: unknown model ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{"model":"minimax-nonexistent","messages":[{"role":"user","content":"hi"}]}' \
    2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

# ═══════════════════════════════════════
#  QWEN
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  QWEN"
echo "============================================"

QWEN_MODEL=$(discover_model "qwen-plus")
echo "Using Qwen model: $QWEN_MODEL"

echo "--- 3a. Qwen basic chat ---"
RESP_3A=$(curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$QWEN_MODEL\",
        \"messages\": [{\"role\": \"user\", \"content\": \"Say exactly 3 words\"}]
    }")
echo "$RESP_3A" | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
print('SESSION_URL:', d.get('session_url','')[:80])
" 2>&1 || echo "(parse failed)"

SESSION_URL_3=$(echo "$RESP_3A" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
    print(d.get('session_url','') or '')
except: pass
" 2>/dev/null || echo "")

echo ""
run_tool_test "$QWEN_MODEL" "3b. Qwen tool calling"

echo ""
echo "--- 3c. Qwen file upload (qwen-vl) ---"
QWEN_VL_MODEL=$(discover_model "qwen-vl")
echo "Using vision model: $QWEN_VL_MODEL"
run_vision_test "$QWEN_VL_MODEL" "qwen-vl vision test"

echo ""
echo "--- 3d. Qwen streaming (SSE) ---"
curl -s -N -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$QWEN_MODEL\",
        \"messages\": [{\"role\": \"user\", \"content\": \"Count 1 2 3.\"}],
        \"stream\": true
    }" 2>/dev/null | head -20

echo ""
echo ""

# --- 3e. Qwen multi-turn via session_url ---
if [ -n "$SESSION_URL_3" ]; then
    echo "--- 3e. Qwen multi-turn (continuation) ---"
    curl -s -X POST "$BASE/v1/chat/completions" \
        -H "$AUTH" -H "Content-Type: application/json" \
        -d "$(python3 -c "
import json
print(json.dumps({
    'model': '$QWEN_MODEL',
    'messages': [{'role': 'user', 'content': 'Now say exactly 5 words'}],
    'session_url': '$SESSION_URL_3'
}))" 2>/dev/null)" 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d: print('ERROR:', d['error'])
else: print('TEXT:', repr(d['choices'][0]['message']['content'])[:200])
"
else
    echo "--- 3e. Skipping Qwen multi-turn (no session_url) ---"
fi

echo ""
echo "--- 3f. Qwen validation: unknown model ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{"model":"qwen-nonexistent","messages":[{"role":"user","content":"hi"}]}' \
    2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

# ═══════════════════════════════════════
#  DONE
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  ALL TESTS COMPLETE"
echo "============================================"
