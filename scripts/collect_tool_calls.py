#!/usr/bin/env python3
"""Collect native tool-calling data: one flagship model per provider, 3 runs each.

Raw request: user message only + write_file tool. No system prompt, no injection.
Raw responses saved verbatim to docs/wire/tool-calling/.
"""
import json
import os
import sys
import time
import urllib.request
import urllib.error

BASE = "http://localhost:8080"
KEY = "test-key-123"
OUT_DIR = "docs/wire/tool-calling"

# Latest/flagship model per provider (from /v1/models registry)
TARGETS = [
    ("deepseek", "deepseek-v4-pro"),
    ("gemini", "gemini-3.1-pro"),
    ("chatgpt", "gpt-5.6"),
    ("kimi", "kimi-k3"),
    ("glm", "glm-5.2"),
    ("claude", "claude-opus-5"),
    ("grok", "grok-4.5"),
    ("qwen", "qwen3.8-max"),
    ("minimax", "minimax-m3"),
    ("mimo", "mimo-v2.5-pro"),
    ("mistral", "mistral-large-latest"),
    ("metaai", "muse-spark"),
]

TOOLS = [{
    "type": "function",
    "function": {
        "name": "write_file",
        "description": "Write content to a file",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The filename"},
                "content": {"type": "string", "description": "The file content"},
            },
            "required": ["name", "content"],
        },
    },
}]

MESSAGE = "Write a file called hello.txt with the content 'Hello, world!'"


def post(body, timeout=180):
    req = urllib.request.Request(
        f"{BASE}/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": f"Bearer {KEY}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {"raw": "unparseable error body"}
    except Exception as e:
        return 0, {"error": str(e)}


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    summary = []
    for provider, model in TARGETS:
        for run in (1, 2, 3):
            body = {
                "model": model,
                "messages": [{"role": "user", "content": MESSAGE}],
                "tools": TOOLS,
                "stream": False,
            }
            t0 = time.time()
            status, resp = post(body)
            elapsed = round(time.time() - t0, 1)
            fname = f"{OUT_DIR}/{provider}-{model}-run{run}.json"
            with open(fname, "w") as f:
                json.dump({"request": body, "http_status": status,
                           "elapsed_s": elapsed, "response": resp},
                          f, indent=2)
            # quick classification
            has_tool = False
            tool_name = None
            args = None
            finish = None
            if status == 200 and "choices" in resp:
                msg = resp["choices"][0].get("message", {})
                tcs = msg.get("tool_calls")
                if tcs:
                    has_tool = True
                    tool_name = tcs[0].get("function", {}).get("name")
                    raw_args = tcs[0].get("function", {}).get("arguments")
                    try:
                        args = json.loads(raw_args) if isinstance(raw_args, str) else raw_args
                    except Exception:
                        args = f"<unparseable: {raw_args!r}>"
                finish = resp["choices"][0].get("finish_reason")
            summary.append({
                "provider": provider, "model": model, "run": run,
                "status": status, "elapsed_s": elapsed,
                "native_tool_call": has_tool, "tool_name": tool_name,
                "args": args, "finish_reason": finish,
            })
            print(f"{provider}/{model} run{run}: http={status} tool={has_tool} "
                  f"name={tool_name} finish={finish} ({elapsed}s)", flush=True)
            time.sleep(3)
    with open(f"{OUT_DIR}/SUMMARY.json", "w") as f:
        json.dump(summary, f, indent=2)
    print("DONE")


if __name__ == "__main__":
    main()
