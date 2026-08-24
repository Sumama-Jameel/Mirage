# Agent-loop (opencode) tool-use scorecard — 2026-08-23

Real-world MTP/1 verification: opencode's agent harness (14 tools: bash,
edit, write, read, grep, glob, ...) driven against the gateway's
OpenAI-compatible surface, per-model task = "create file X with content Y".

Config: `~/obscura-agent-test/opencode.json` → `http://127.0.0.1:8080/v1`.
Gateway fixes made during this run are listed at the bottom.

## Results

| Model | File created | Tool loop | Notes |
|---|---|---|---|
| deepseek-chat | ✅ exact | ✅ | Full pass; also continuation + read-back |
| qwen-max | ✅ exact | ✅ | Multi-turn list→write now passes after flat-prompt ordering fix |
| muse-spark | ✅ content ok | ✅ | Wrote a stray second filename (model sloppiness) |
| mimo-v2.5 | ✅ (curl-verified) | ✅ | Auth fixed (userId lives on account.xiaomi.com, Xiaomi SSO); non-stream path unified onto streaming after leak found in live test |
| kimi-k2.6 | ❌ via harness / ✅ forced + few-tools | ⚠️ | 14-tool auto prompts overwhelm it; forced `tool_choice` and small toolsets work perfectly. Streaming verified separately |
| gemini-3.5-flash / 3.6-flash / 3.1-pro | ❌ | ⚠️ loop runs, drifts off-task | Reads project files, then chats about them instead of executing. Plain chat (with reasoning) works fine |
| gpt-5.6 | ❌ | ⚠️ | Conflates MTP markers with apply-patch format (`*** End Patch`), emitting malformed blocks. Plain chat works |

### Flat-prompt ordering fix (2026-08-24)

qwen-max failed a real `list files → write file` agent loop: the gateway
re-prepended the MTP policy every turn and buried the newest instruction
inside a mislabeled "User request:" section containing the whole flattened
history. Fixed via `mtp::compose_flat_prompt` — stable order (policy once →
transcript → tool results → **newest instruction last**, delimited), with
`has_mirage_history` skipping re-injection on continuations. Applied to
qwen, mimo, minimax, grok. MiMo additionally unified its non-streaming chat
onto the streaming path after live testing caught unprocessed MTP markers
leaking into tool-call responses.

## Gateway fixes made during this testing

1. Gemini no longer rejects `temperature`/`max_tokens`/`top_p`/`stop`/
   penalties — agent harnesses always send them; they are ignored upstream.
2. MTP prompt gained explicit anti-patch-format and exact-marker rules.
3. Earlier same-day: `--data-dir` wiring, Kimi kimi.ai auth + server-side
   refresh, ChatGPT split-cookie probe, Gemini cumulative-stream dedupe,
   BardErrorInfo classification, Grok dynamic release + statsig harvest.



Note: opencode keeps a background server that caches provider config;
`OPENCODE_CONFIG` forces a fresh read per invocation.
