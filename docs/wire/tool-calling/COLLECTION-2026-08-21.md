# Native Tool-Calling Data Collection — 2026-08-21

## Setup

- Gateway running with `OBSCURA_NATIVE_TOOLS_ONLY=1` (XML tool-prompt injection disabled)
- No system prompt, no XML injection, no prompt engineering
- Tool definition sent: `write_file(name: string, content: string)`
- User message: `Write a file called hello.txt with the content Hello, world!`
- 3 runs per model, non-streaming

## Results

| Provider | Model | HTTP | Native Tool Call | Tool Name | Finish | Error |
|---|---|---:|---|---|---|---|
| deepseek | deepseek-chat | 200 | No | — | stop | — |
| deepseek | deepseek-chat | 200 | No | — | stop | — |
| deepseek | deepseek-chat | 200 | No | — | stop | — |
| deepseek | deepseek-v4-pro | 200 | No | — | stop | — |
| deepseek | deepseek-v4-pro | 200 | No | — | stop | — |
| deepseek | deepseek-v4-pro | 200 | No | — | stop | — |
| chatgpt | gpt-5.6 | 401 | No | — | — | authentication error: model 'gpt-5.6'... |
| chatgpt | gpt-5.6 | 401 | No | — | — | authentication error: model 'gpt-5.6'... |
| chatgpt | gpt-5.6 | 401 | No | — | — | authentication error: model 'gpt-5.6'... |
| chatgpt | gpt-4o | 401 | No | — | — | authentication error: model 'gpt-4o' ... |
| chatgpt | gpt-4o | 401 | No | — | — | authentication error: model 'gpt-4o' ... |
| chatgpt | gpt-4o | 401 | No | — | — | authentication error: model 'gpt-4o' ... |
| claude | claude-opus-5 | 401 | No | — | — | authentication error: model 'claude-o... |
| claude | claude-opus-5 | 401 | No | — | — | authentication error: model 'claude-o... |
| claude | claude-opus-5 | 401 | No | — | — | authentication error: model 'claude-o... |
| claude | claude-sonnet-5 | 401 | No | — | — | authentication error: model 'claude-s... |
| claude | claude-sonnet-5 | 401 | No | — | — | authentication error: model 'claude-s... |
| claude | claude-sonnet-5 | 401 | No | — | — | authentication error: model 'claude-s... |
| gemini | gemini-3.1-pro | 200 | No | — | stop | — |
| gemini | gemini-3.1-pro | 200 | No | — | stop | — |
| gemini | gemini-3.1-pro | 200 | No | — | stop | — |
| gemini | gemini-3.6-flash | 200 | No | — | stop | — |
| gemini | gemini-3.6-flash | 200 | No | — | stop | — |
| gemini | gemini-3.6-flash | 200 | No | — | stop | — |
| kimi | kimi-k3 | 502 | No | — | — | provider error: navigation to https:/... |
| kimi | kimi-k3 | 502 | No | — | — | provider error: navigation to https:/... |
| kimi | kimi-k3 | 502 | No | — | — | provider error: navigation to https:/... |
| kimi | kimi-k2.7-code | 502 | No | — | — | provider error: navigation to https:/... |
| kimi | kimi-k2.7-code | 502 | No | — | — | provider error: navigation to https:/... |
| kimi | kimi-k2.7-code | 401 | No | — | — | authentication error: Kimi auth token... |
| glm | glm-5.2 | 401 | No | — | — | authentication error: model 'glm-5.2'... |
| glm | glm-5.2 | 401 | No | — | — | authentication error: model 'glm-5.2'... |
| glm | glm-5.2 | 401 | No | — | — | authentication error: model 'glm-5.2'... |
| glm | glm-4.7 | 401 | No | — | — | authentication error: model 'glm-4.7'... |
| glm | glm-4.7 | 401 | No | — | — | authentication error: model 'glm-4.7'... |
| glm | glm-4.7 | 401 | No | — | — | authentication error: model 'glm-4.7'... |
| grok | grok-4.5 | 502 | No | — | — | provider error: Grok API 403 after ma... |
| grok | grok-4.5 | 502 | No | — | — | provider error: provider 'grok' is te... |
| grok | grok-4.5 | 502 | No | — | — | provider error: provider 'grok' is te... |
| grok | grok-auto | 502 | No | — | — | provider error: provider 'grok' is te... |
| grok | grok-auto | 502 | No | — | — | provider error: provider 'grok' is te... |
| grok | grok-auto | 502 | No | — | — | provider error: provider 'grok' is te... |
| mistral | mistral-large-latest | 200 | No | — | stop | — |
| mistral | mistral-large-latest | 200 | No | — | stop | — |
| mistral | mistral-large-latest | 200 | No | — | stop | — |
| mistral | mistral-medium-latest | 200 | No | — | stop | — |
| mistral | mistral-medium-latest | 200 | No | — | stop | — |
| mistral | mistral-medium-latest | 200 | No | — | stop | — |
| metaai | muse-spark | 200 | No | — | stop | — |
| metaai | muse-spark | 200 | No | — | stop | — |
| metaai | muse-spark | 200 | No | — | stop | — |
| mimo | mimo-v2.5-pro | 401 | No | — | — | authentication error: model 'mimo-v2.... |
| mimo | mimo-v2.5-pro | 401 | No | — | — | authentication error: model 'mimo-v2.... |
| mimo | mimo-v2.5-pro | 401 | No | — | — | authentication error: model 'mimo-v2.... |
| mimo | mimo-v2.5 | 401 | No | — | — | authentication error: model 'mimo-v2.... |
| mimo | mimo-v2.5 | 401 | No | — | — | authentication error: model 'mimo-v2.... |
| mimo | mimo-v2.5 | 401 | No | — | — | authentication error: model 'mimo-v2.... |
| minimax | minimax-m3 | 502 | No | — | — | provider error: Minimax returned fini... |
| minimax | minimax-m3 | 502 | No | — | — | provider error: provider 'minimax' is... |
| minimax | minimax-m3 | 502 | No | — | — | provider error: provider 'minimax' is... |
| qwen | qwen3.8-max | 200 | No | — | stop | — |
| qwen | qwen3.8-max | 200 | No | — | stop | — |
| qwen | qwen3.8-max | 200 | No | — | stop | — |
| qwen | qwen3.7-max | 400 | No | — | — | bad request: unknown Qwen model: qwen... |
| qwen | qwen3.7-max | 400 | No | — | — | bad request: unknown Qwen model: qwen... |
| qwen | qwen3.7-max | 400 | No | — | — | bad request: unknown Qwen model: qwen... |

## Analysis

### Key Finding

**ZERO providers produced native tool calls when XML injection was disabled.** All 5 providers that returned 200 produced text responses with `finish_reason: "stop"` and `tool_calls: None`. The models ignored the `tools` parameter in the API request and produced text instructions instead.

**Verified with controlled test:** Even WITH a system prompt ("Use the provided tools when the user asks you to perform actions") but WITHOUT XML injection, DeepSeek still returned text and ignored the `tools` parameter.

This confirms: **XML prompt injection is not a fallback — it is the primary mechanism that makes tool calling work.** The `tools` parameter alone is insufficient; models need the explicit XML instructions in the system prompt to understand they should use tools.

### Provider Status

| Provider | Models Tested | Status |
|---|---|---|
| deepseek | deepseek-chat, deepseek-v4-pro | NO native tool calling — text responses |
| gemini | gemini-3.1-pro, gemini-3.6-flash | NO native tool calling — text responses |
| mistral | mistral-large-latest, mistral-medium-latest | NO native tool calling — text responses |
| metaai | muse-spark | NO native tool calling — text responses |
| qwen | qwen3.8-max | NO native tool calling — text responses |
| chatgpt | gpt-5.6, gpt-4o | BLOCKED — no browser profile |
| claude | claude-opus-5, claude-sonnet-5 | BLOCKED — no browser profile |
| glm | glm-5.2, glm-4.7 | BLOCKED — no browser profile |
| kimi | kimi-k3, kimi-k2.7-code | BLOCKED — navigation timeout |
| mimo | mimo-v2.5-pro, mimo-v2.5 | BLOCKED — no userId cookie |
| grok | grok-4.5, grok-auto | BLOCKED — session expired / protocol drift |
| minimax | minimax-m3 | BLOCKED — quota exhausted |

## Architectural Implication

The gateway's `inject_tool_prompt` (XML injection) is **essential** for tool calling to work. Without it:
- Models receive the `tools` parameter in the API request but ignore it
- They produce text responses instead of structured tool calls
- `finish_reason` is always `stop`, never `tool_calls`

This means:
1. `OBSCURA_NATIVE_TOOLS_ONLY=1` is actually testing "what happens without XML injection" — and the answer is: nothing works
2. The XML prompt injection is not a hack or fallback — it's the primary mechanism
3. Future providers that want native tool calling need the XML injection too, unless they have a different API surface that supports tools natively

## Files

- `SUMMARY.json` — structured results (794 lines)
- `<provider>-<model>-run<N>.json` — full request+response per run
