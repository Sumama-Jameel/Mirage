#!/usr/bin/env python3
"""Native tool-calling data collection for obscura-gateway.

Sends raw requests (no system prompt, no XML injection) with a write_file tool
to every provider model, 3 runs each, and documents which models produce native
tool_calls vs text responses.

Usage:
    python3 scripts/collect_native_tools.py

Requires the gateway to be running on http://localhost:8080 with:
    OBSCURA_NATIVE_TOOLS_ONLY=1
    OBSCURA_DUMP_DIR=docs/wire/tool-calling
"""

import json
import os
import time
import urllib.request
import urllib.error
from datetime import datetime, timezone

GATEWAY = "http://localhost:8080"
API_KEY = "test-key-123"
OUTPUT_DIR = "docs/wire/tool-calling"
RUNS_PER_MODEL = 3

USER_MESSAGE = "Write a file called hello.txt with the content Hello, world!"

WRITE_FILE_TOOL = {
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
}

# Models to test: (provider, model_id)
MODELS = [
    ("deepseek", "deepseek-chat"),
    ("deepseek", "deepseek-v4-pro"),
    ("chatgpt", "gpt-5.6"),
    ("chatgpt", "gpt-4o"),
    ("claude", "claude-opus-5"),
    ("claude", "claude-sonnet-5"),
    ("gemini", "gemini-3.1-pro"),
    ("gemini", "gemini-3.6-flash"),
    ("kimi", "kimi-k3"),
    ("kimi", "kimi-k2.7-code"),
    ("glm", "glm-5.2"),
    ("glm", "glm-4.7"),
    ("grok", "grok-4.5"),
    ("grok", "grok-auto"),
    ("mistral", "mistral-large-latest"),
    ("mistral", "mistral-medium-latest"),
    ("metaai", "muse-spark"),
    ("mimo", "mimo-v2.5-pro"),
    ("mimo", "mimo-v2.5"),
    ("minimax", "minimax-m3"),
    ("qwen", "qwen3.8-max"),
    ("qwen", "qwen3.7-max"),
]


def send_request(model_id: str) -> dict:
    """Send a chat completion request and return the result dict."""
    request_body = {
        "model": model_id,
        "messages": [{"role": "user", "content": USER_MESSAGE}],
        "tools": [WRITE_FILE_TOOL],
        "stream": False,
    }

    data = json.dumps(request_body).encode("utf-8")
    req = urllib.request.Request(
        f"{GATEWAY}/v1/chat/completions",
        data=data,
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
        },
        method="POST",
    )

    start = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            body = resp.read().decode("utf-8")
            elapsed = time.monotonic() - start
            return {
                "http_status": resp.status,
                "elapsed_s": round(elapsed, 1),
                "response": json.loads(body),
            }
    except urllib.error.HTTPError as e:
        elapsed = time.monotonic() - start
        body = e.read().decode("utf-8") if e.fp else ""
        try:
            resp_json = json.loads(body)
        except json.JSONDecodeError:
            resp_json = {"raw": body}
        return {
            "http_status": e.code,
            "elapsed_s": round(elapsed, 1),
            "response": resp_json,
        }
    except Exception as e:
        elapsed = time.monotonic() - start
        return {
            "http_status": 0,
            "elapsed_s": round(elapsed, 1),
            "response": {"error": str(e)},
        }


def analyze_response(result: dict) -> dict:
    """Analyze whether the response contains native tool calls."""
    status = result["http_status"]
    resp = result["response"]

    if status != 200:
        return {
            "native_tool_call": False,
            "tool_name": None,
            "args": None,
            "finish_reason": None,
            "error": resp.get("error", {}).get("message", f"HTTP {status}"),
        }

    choices = resp.get("choices", [])
    if not choices:
        return {
            "native_tool_call": False,
            "tool_name": None,
            "args": None,
            "finish_reason": None,
            "error": "no choices in response",
        }

    msg = choices[0].get("message", {})
    finish_reason = choices[0].get("finish_reason")
    tool_calls = msg.get("tool_calls")

    if tool_calls and finish_reason == "tool_calls":
        tc = tool_calls[0]
        return {
            "native_tool_call": True,
            "tool_name": tc.get("function", {}).get("name"),
            "args": tc.get("function", {}).get("arguments"),
            "finish_reason": finish_reason,
            "error": None,
        }

    return {
        "native_tool_call": False,
        "tool_name": None,
        "args": None,
        "finish_reason": finish_reason,
        "error": None,
    }


def main():
    os.makedirs(OUTPUT_DIR, exist_ok=True)
    summary = []

    for provider, model_id in MODELS:
        print(f"\n{'='*60}")
        print(f"  {provider} / {model_id}")
        print(f"{'='*60}")

        for run in range(1, RUNS_PER_MODEL + 1):
            print(f"  Run {run}/{RUNS_PER_MODEL}...", end=" ", flush=True)
            result = send_request(model_id)
            analysis = analyze_response(result)

            # Save per-run JSON
            filename = f"{provider}-{model_id}-run{run}.json"
            filepath = os.path.join(OUTPUT_DIR, filename)
            with open(filepath, "w") as f:
                json.dump(
                    {
                        "request_body": {
                            "model": model_id,
                            "messages": [{"role": "user", "content": USER_MESSAGE}],
                            "tools": [WRITE_FILE_TOOL],
                            "stream": False,
                        },
                        **result,
                    },
                    f,
                    indent=2,
                )

            # Add to summary
            summary.append(
                {
                    "provider": provider,
                    "model": model_id,
                    "run": run,
                    "http_status": result["http_status"],
                    "elapsed_s": result["elapsed_s"],
                    **analysis,
                }
            )

            # Print result
            if analysis["native_tool_call"]:
                print(
                    f"NATIVE TOOL CALL -> {analysis['tool_name']}({analysis['args']}) [{result['elapsed_s']}s]"
                )
            elif result["http_status"] != 200:
                err = analysis.get("error", f"HTTP {result['http_status']}")
                print(f"HTTP {result['http_status']}: {err} [{result['elapsed_s']}s]")
            else:
                content = result["response"]["choices"][0]["message"]["content"][:80]
                print(f"TEXT (no tool call): {content}... [{result['elapsed_s']}s]")

            # Small delay between requests
            time.sleep(0.5)

    # Write summary
    summary_path = os.path.join(OUTPUT_DIR, "SUMMARY.json")
    with open(summary_path, "w") as f:
        json.dump(summary, f, indent=2)

    # Write collection markdown
    date_str = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    md_path = os.path.join(OUTPUT_DIR, f"COLLECTION-{date_str}.md")
    write_markdown(summary, md_path)

    print(f"\n\nDone. Results in {OUTPUT_DIR}/")
    print(f"  SUMMARY.json — structured results")
    print(f"  COLLECTION-{date_str}.md — human analysis")

    # Print summary table
    print(f"\n{'='*80}")
    print("SUMMARY")
    print(f"{'='*80}")
    print(f"{'Provider':<12} {'Model':<25} {'HTTP':>4} {'Native':>6} {'Tool':>20} {'Time':>6}")
    print("-" * 80)
    for s in summary:
        tool = s.get("tool_name") or "-"
        native = "YES" if s["native_tool_call"] else "no"
        http = s["http_status"]
        elapsed = f"{s['elapsed_s']}s"
        print(f"{s['provider']:<12} {s['model']:<25} {http:>4} {native:>6} {tool:>20} {elapsed:>6}")


def write_markdown(summary: list, path: str):
    """Write a human-readable markdown collection report."""
    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    lines = [
        f"# Native Tool-Calling Data Collection — {now.split()[0]}",
        "",
        "## Setup",
        "",
        "- Gateway running with `OBSCURA_NATIVE_TOOLS_ONLY=1` (XML tool-prompt injection disabled)",
        "- No system prompt, no XML injection, no prompt engineering",
        "- Tool definition sent: `write_file(name: string, content: string)`",
        f"- User message: `{USER_MESSAGE}`",
        "- 3 runs per model, non-streaming",
        "",
        "## Results",
        "",
        "| Provider | Model | HTTP | Native Tool Call | Tool Name | Finish | Error |",
        "|---|---|---:|---|---|---|---|",
    ]

    for s in summary:
        native = "**Yes**" if s["native_tool_call"] else "No"
        tool = s.get("tool_name") or "—"
        finish = s.get("finish_reason") or "—"
        error = s.get("error") or "—"
        if len(error) > 40:
            error = error[:37] + "..."
        lines.append(
            f"| {s['provider']} | {s['model']} | {s['http_status']} | {native} | {tool} | {finish} | {error} |"
        )

    # Group by provider for analysis
    lines.extend([
        "",
        "## Analysis",
        "",
    ])

    providers = {}
    for s in summary:
        key = s["provider"]
        if key not in providers:
            providers[key] = []
        providers[key].append(s)

    for provider, runs in providers.items():
        native_count = sum(1 for r in runs if r["native_tool_call"])
        total = len(runs)
        http_ok = sum(1 for r in runs if r["http_status"] == 200)
        models_tested = list(set(r["model"] for r in runs))

        if native_count > 0:
            status = f"NATIVE TOOL CALLING — {native_count}/{total} runs produced tool_calls"
        elif http_ok == 0:
            errors = list(set(r.get("error", "unknown") for r in runs))
            status = f"BLOCKED — {errors[0] if errors else 'unknown error'}"
        else:
            status = "NO native tool calling — models returned text responses"

        lines.append(f"### {provider.title()}")
        lines.append(f"- **Status**: {status}")
        lines.append(f"- **Models tested**: {', '.join(models_tested)}")
        lines.append("")

    lines.extend([
        "## Files",
        "",
        "- `SUMMARY.json` — structured results",
        "- `<provider>-<model>-run<N>.json` — full request+response per run",
    ])

    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
