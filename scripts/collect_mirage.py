#!/usr/bin/env python3
"""Mirage tool-calling data collection.

System prompt with XML tool instructions + native tools param.
Tests whether models follow XML format or use native tool calling.
"""
import json, os, re, sys, time, urllib.request, urllib.error

BASE = "http://localhost:8080"
KEY = "test-key-123"
OUT = "docs/wire/tool-calling-mirage"
os.makedirs(OUT, exist_ok=True)

# Read system prompt from file
with open("docs/wire/tool-calling-mirage/system-prompt.txt") as f:
    SYSTEM_PROMPT = f.read().strip()

TOOLS_PARAM = [{"type":"function","function":{"name":"write_file","description":"Write content to a file","parameters":{"type":"object","properties":{"name":{"type":"string","description":"The filename"},"content":{"type":"string","description":"The file content"}},"required":["name","content"]}}}]

LATEST = {
    "deepseek": "deepseek-chat",
    "gemini": "gemini-3.1-pro",
    "chatgpt": "gpt-4o",
    "kimi": "kimi-k3",
    "glm": "glm-5.2",
    "claude": "claude-opus-5",
    "grok": "grok-4.5",
    "qwen": "qwen3.8-max",
    "minimax": "minimax-m3",
    "mimo": "mimo-v2.5-pro",
    "mistral": "mistral-large-latest",
    "metaai": "muse-spark",
}

def post(body, timeout=180):
    req = urllib.request.Request(
        f"{BASE}/v1/chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read().decode())
        except Exception:
            return e.code, {"raw": "unparseable"}
    except Exception as e:
        return 0, {"error": str(e)}

def classify(text):
    m = re.search(r"<tool_call>(.*?)</tool_call>", text, re.DOTALL)
    if m:
        args = {}
        for am in re.finditer(r"<(\w+)>(.*?)</\1>", m.group(1), re.DOTALL):
            args[am.group(1)] = am.group(2).strip()
        name = re.search(r"<name>(.*?)</name>", m.group(1))
        return True, name.group(1).strip() if name else None, args
    return False, None, None

summary = []
for prov, model in LATEST.items():
    for run in (1, 2, 3):
        body = {
            "model": model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": "Write a file called hello.txt with the content 'Hello, world!'"},
            ],
            "tools": TOOLS_PARAM,
            "stream": False,
        }
        t0 = time.time()
        status, resp = post(body)
        elapsed = round(time.time() - t0, 1)

        with open(f"{OUT}/{prov}-{model}-run{run}.json", "w") as f:
            json.dump({"request_body": body, "http_status": status,
                       "elapsed_s": elapsed, "response": resp}, f, indent=2)

        has_xml = False
        tool_name = None
        args = None
        has_native = False
        finish = None
        text = ""
        if status == 200 and "choices" in resp:
            msg = resp["choices"][0].get("message", {})
            text = msg.get("content", "") or ""
            finish = resp["choices"][0].get("finish_reason")
            has_native = msg.get("tool_calls") is not None
            has_xml, tool_name, args = classify(text)

        summary.append({
            "provider": prov, "model": model, "run": run,
            "http_status": status, "elapsed_s": elapsed,
            "native_tool_call": has_native,
            "xml_tool_call": has_xml, "tool_name": tool_name,
            "args": args, "finish_reason": finish,
            "response_preview": text[:500] if text else "",
        })
        label = "NATIVE" if has_native else ("XML" if has_xml else "NONE")
        print(f"{prov}/{model} run{run}: http={status} mode={label} finish={finish} ({elapsed}s)", flush=True)
        time.sleep(3)

with open(f"{OUT}/SUMMARY.json", "w") as f:
    json.dump(summary, f, indent=2)
print("DONE")
