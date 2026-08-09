#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVER_BIN="$SCRIPT_DIR/target/release/obscura-gateway"
API_KEY="test-key-123"
BASE="http://127.0.0.1:8080"
AUTH="Authorization: Bearer $API_KEY"

start_server() {
    RUST_LOG=info $SERVER_BIN &
    SERVER_PID=$!
    for i in $(seq 1 60); do
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

cleanup() {
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

# Small test PNG (32x32 red square on blue background — actual visual content for OCR)
PNG_DATA="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAANElEQVR4nO3OsQ0AMAgEsd9/aRgiigDJxdXnJFWzAQAAbAe8HgAAAAAAAADuA/4HAAAwDGhZs/hqlJjNjgAAAABJRU5ErkJggg=="

echo "=== Starting server ==="
start_server

# ═══════════════════════════════════════
#  GEMINI
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  GEMINI"
echo "============================================"

# Use /v1/models to find the actual Gemini model name at runtime.
GEMINI_MODEL=$(curl -s -H "$AUTH" "$BASE/v1/models" | python3 -c "
import json,sys
d=json.load(sys.stdin)
models=[m['id'] for m in d['data'] if m['id'].startswith('gemini-')]
print(models[0] if models else 'gemini-3.5-flash')
")
echo "Using Gemini model: $GEMINI_MODEL"

echo "--- 1a. Gemini tool calling ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$GEMINI_MODEL\",
        \"messages\": [
            {\"role\": \"user\", \"content\": \"What is the weather in Paris? Use the get_weather function.\"}
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
    else: print('TEXT:', repr(m['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

echo ""
echo "--- 1b. Gemini file upload ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"$GEMINI_MODEL\",
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
    print('TEXT:', repr(d['choices'][0]['message']['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
    print('SESSION_URL:', d.get('session_url','')[:80])
"

# ═══════════════════════════════════════
#  DEEPSEEK
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  DEEPSEEK"
echo "============================================"

echo "--- 2a. DeepSeek tool calling ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{
        "model": "deepseek-chat",
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
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }
        ]
    }' 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    m = d['choices'][0]['message']
    tc = m.get('tool_calls')
    if tc: print('TOOL_CALLS:', json.dumps(tc, indent=2))
    else: print('TEXT:', repr(m['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

echo ""
echo "--- 2b. DeepSeek file upload (vision) ---"
timeout 30 curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"deepseek-vision\",
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
    print('TEXT:', repr(d['choices'][0]['message']['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
" 2>&1 || echo "(no valid response in 30s)"

# ═══════════════════════════════════════
#  CHATGPT
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  CHATGPT"
echo "============================================"

echo "--- 3a. ChatGPT tool calling (native) ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{
        "model": "gpt-4o-mini",
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
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }
        ]
    }' 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    m = d['choices'][0]['message']
    tc = m.get('tool_calls')
    if tc: print('TOOL_CALLS:', json.dumps(tc, indent=2))
    else: print('TEXT:', repr(m['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

echo ""
echo "--- 3b. ChatGPT file upload ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d "{
        \"model\": \"gpt-4o-mini\",
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
    print('TEXT:', repr(d['choices'][0]['message']['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
    print('SESSION_URL:', d.get('session_url','')[:80])
"

echo ""
echo "--- 3c. ChatGPT native tool calling (forced) ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "user", "content": "Use the get_weather function to get the weather in Paris."}
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current temperature for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }
        ],
        "tool_choice": "required"
    }' 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    m = d['choices'][0]['message']
    tc = m.get('tool_calls')
    if tc: print('TOOL_CALLS:', json.dumps(tc, indent=2))
    else: print('TEXT:', repr(m['content']))
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

# ═══════════════════════════════════════
#  ADVANCED TESTS
# ═══════════════════════════════════════
echo ""
echo "============================================"
echo "  ADVANCED TESTS"
echo "============================================"

echo ""
echo "--- 4a. DeepSeek streaming (SSE) ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{
        "model": "deepseek-chat",
        "messages": [{"role": "user", "content": "Count 1 2 3."}],
        "stream": true
    }' 2>&1 | head -10

echo ""
echo "--- 4b. DeepSeek reasoner with thinking ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{
        "model": "deepseek-reasoner",
        "messages": [{"role": "user", "content": "What is 15*12?"}],
        "thinking": true
    }' 2>&1 | python3 -c "
import json,sys
d=json.load(sys.stdin)
if 'error' in d:
    print('ERROR:', d['error'])
else:
    m = d['choices'][0]['message']
    print('CONTENT:', repr(m.get('content','')))
    rc = m.get('reasoning_content')
    if rc: print('REASONING:', repr(rc[:200]))
    else: print('REASONING: none')
    print('FINISH_REASON:', d['choices'][0]['finish_reason'])
"

echo ""
echo "--- 4c. Health check ---"
curl -s -o /dev/null -w "GET /health -> HTTP %{http_code}\n" "$BASE/health"

echo ""
echo "--- 4d. Unknown model rejection ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{
        "model": "nonexistent-model",
        "messages": [{"role": "user", "content": "hi"}]
    }' 2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

echo ""
echo "--- 4e. Auth failure (no token) ---"
curl -s -o /dev/null -w "No auth -> HTTP %{http_code}\n" -X POST "$BASE/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{"model":"deepseek-chat","messages":[{"role":"user","content":"hi"}]}'

echo ""
echo "--- 4f. Empty messages ---"
curl -s -X POST "$BASE/v1/chat/completions" \
    -H "$AUTH" -H "Content-Type: application/json" \
    -d '{"model":"deepseek-chat","messages":[]}' \
    2>&1 | python3 -c "import json,sys; d=json.load(sys.stdin); print('ERROR:', d.get('error',{}).get('message',''))"

echo ""
echo "============================================"
echo "  ALL TESTS COMPLETE"
echo "============================================"
