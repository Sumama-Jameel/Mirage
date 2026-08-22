# Tool Calling Architecture

## Overview

The gateway provides tool/function calling to clients via two mechanisms:

1. **XML Prompt Injection** (primary) — the gateway appends XML-formatted tool
   definitions and usage instructions into the user prompt. The model responds
   with `<tool_call>` XML markers that the gateway parses.

2. **Native `tools` Parameter** (when supported) — the gateway forwards the
   OpenAI `tools` array in the API request body. If the provider supports it,
   the model responds with `tool_calls` in the message.

## Key Finding: XML Injection Is The Primary Mechanism

**Tested 2026-08-21** with `OBSCURA_NATIVE_TOOLS_ONLY=1` (XML injection disabled,
`tools` parameter still sent in API request):

| Provider | Models Tested | HTTP | Native Tool Call | Text Response |
|----------|--------------|------|------------------|---------------|
| deepseek | deepseek-chat, deepseek-v4-pro | 200 | No | Yes |
| gemini | gemini-3.1-pro, gemini-3.6-flash | 200 | No | Yes |
| mistral | mistral-large-latest, mistral-medium-latest | 200 | No | Yes |
| metaai | muse-spark | 200 | No | Yes |
| qwen | qwen3.8-max | 200 | No | Yes |

**Conclusion:** All 5 providers that returned 200 produced text responses with
`finish_reason: "stop"` and `tool_calls: None`. The `tools` parameter alone is
insufficient. Models need the explicit XML instructions in the system prompt to
understand they should use tools.

A controlled test confirmed this: even WITH a system prompt
("Use the provided tools when the user asks you to perform actions") but WITHOUT
XML injection, DeepSeek still returned text and ignored the `tools` parameter.

## How XML Injection Works

### `inject_tool_prompt(provider, prompt, tools, tool_choice)`

Located in `providers/tool_call.rs`. Called by every provider adapter that
supports tool calling.

**Flow:**
1. Check `native_tools_only()` — if `OBSCURA_NATIVE_TOOLS_ONLY=1`, skip injection
2. Build instruction block with tool definitions (name, description, parameters JSON)
3. Append `tool_choice` policy (auto, required, named, none)
4. Append `TOOL_CALL_FORMAT_INSTRUCTION` — exact XML format the model must follow
5. Prepend instructions to user prompt
6. Return modified prompt

### `native_tools_only()`

Returns `true` when env `OBSCURA_NATIVE_TOOLS_ONLY=1`. Used for capability
data collection to test native tool channels without XML interference.

### `debug_dump_llm_input(provider, input)`

When env `OBSCURA_DUMP_DIR=<dir>` is set, appends the full LLM input as JSONL
to `<dir>/prompts.jsonl`. Used by data-collection runs.

## Response Parsing

### `parse_tool_calls(text) -> Option<Vec<ToolCall>>`

Parses `<tool_call>` XML markers from model text output:

```xml
<tool_call>{"name":"function_name","arguments":{...}}</tool_call>
```

Multiple calls are supported. Each marker is parsed as JSON. Malformed markers
are logged and skipped.

### `gemini_tool_use_prompt(provider, prompt, tools, tool_choice)`

Specialized injection for Gemini that wraps tool definitions in Gemini-specific
formatting when the provider's StreamGenerate endpoint does not accept the
standard OpenAI `tools` parameter.

## Provider-Specific Behavior

| Provider | Native `tools` Support | XML Injection Used | Notes |
|----------|----------------------|-------------------|-------|
| deepseek | No | Yes | `tools` param in request ignored by model |
| chatgpt | Unknown | Yes | Needs capture data |
| claude | Unknown | Yes | Needs capture data |
| gemini | No | Yes | StreamGenerate does not accept tools |
| glm | Unknown | Yes | Needs capture data |
| grok | Unknown | Yes | Session expired; needs re-login |
| kimi | Unknown | Yes | Navigation timeout; needs network fix |
| metaai | No | Yes | DGW WebSocket does not support tools |
| minimax | Unknown | Yes | Quota exhausted |
| mistral | No | Yes | `tools` param ignored by model |
| mimo | No | Yes | `tools` param ignored by model |
| qwen | No | Yes | `tools` param ignored by model |

## Environment Variables

| Variable | Effect |
|----------|--------|
| `OBSCURA_NATIVE_TOOLS_ONLY=1` | Disables XML injection; only native `tools` param is sent |
| `OBSCURA_DUMP_DIR=<dir>` | Dumps LLM inputs as JSONL to `<dir>/prompts.jsonl` |

## Future: Native Tool Calling

If a provider is proven to support native tool calling via capture evidence:

1. Set `native_tools: Capability::Yes` in the provider manifest
2. Implement request mapping: `tools` -> provider native tools format
3. Implement response mapping: provider tool_calls -> OpenAI tool_calls
4. Skip XML injection for that provider

Until native support is proven by capture, XML injection remains the primary
mechanism for all providers.

## Files

- `providers/tool_call.rs` — `inject_tool_prompt`, `parse_tool_calls`, `gemini_tool_use_prompt`
- `providers/manifest.rs` — `FeatureSpec.native_tools` capability flag
- `docs/wire/tool-calling/` — collection data and analysis
