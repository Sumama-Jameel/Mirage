# Streaming Tool Call Parsing — Known Pitfalls

## The Problem

When a model emits `<tool_call>` XML markers in streaming SSE content, the markers are
split across multiple SSE deltas. Each delta contains a small fragment like `<`, `tool`,
`_call`, `>`, `{`, `"name"`, etc. No single delta contains the full 11-char
`<tool_call>` string.

A naive per-delta parser (like `process_content_text` in `deepseek/direct.rs` before the
fix) uses `rest.find("")` on each delta. Since no delta contains the full marker, the
parser never enters "in tool call" mode, never collects the JSON, and the tool call is
silently lost. The response comes back with `finish_reason: "stop"` and no `tool_calls`.

## The Fix

Use `XmlToolCallStripper` from `providers/tool_call.rs`. It is a stateful incremental
parser designed specifically for this case:

- Tracks `in_tool_call` state across calls
- Has a `suffix` buffer to handle partial marker matches at chunk boundaries
- Accumulates `tool_call_buffer` across multiple deltas
- Returns `Some(ToolCall)` when a complete `</tool_call>` block finishes
- `finish_pending()` recovers truncated calls at stream end

**Do NOT write your own `<tool_call>` detection in streaming paths.** The split
marker problem is subtle and error-prone.

## Provider-specific notes

### DeepSeek
- Does NOT emit native `tool_calls` in SSE (the web API ignores the `tools` field)
- Emits `<tool_call>` XML in content, which must be parsed from split SSE deltas
- Fixed by replacing `process_content_text` with `XmlToolCallStripper`

### Qwen
- DOES emit native `tool_calls` via `delta.tool_calls` in SSE
- Also sometimes echoes `<tool_call>` XML in content (informational, not functional)
- XML fallback is unnecessary and should be removed — native path is reliable
- The `finish_reason` must be set to `"tool_calls"` when native calls were emitted

### General rules for streaming tool calls

1. **Never use `rest.find("")` on individual SSE deltas** — the marker will
   be split across chunks
2. **Use `XmlToolCallStripper`** from `tool_call.rs` for XML marker parsing
3. **Native `delta.tool_calls`** are preferred over XML when available
4. **`finish_reason` must be `"tool_calls"`** when any tool calls were emitted
   (streaming or non-streaming), not `"stop"`
5. **Test with `stream: true`** — non-streaming accumulates full text and regex
   works, but streaming fails silently
