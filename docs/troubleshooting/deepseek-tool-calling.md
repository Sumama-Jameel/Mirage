# DeepSeek Tool Calling - Issue & Fix

## Situation

When opencode (or any OpenAI-compatible client) sends tool-calling requests through
the gateway, DeepSeek's models either:
- Don't call tools (return plain text instead of `<tool_call>` markers)
- Return garbled/confused responses after the first tool-call round-trip
- Work on the first call but break on follow-up turns

## Root Cause

DeepSeek's internal web endpoint (`/api/v0/chat/completion`) does **not** support
native function calling. The gateway uses a prompt-injection approach instead:
it injects tool definitions into the user prompt and parses `<tool_call>` XML
markers from the model's response.

The bug was a **double-injection** problem:

1. On the **first turn** (user asks something with tools), `inject_tool_prompt`
   correctly wraps the prompt with tool definitions. This works fine.

2. On **follow-up turns** (opencode sends back tool results as `role: "tool"`),
   `inject_tool_prompt` was called **again**. The prompt sent to DeepSeek became:
   ```
   You have access to the following functions...
   Function: read_file
   ...
   When you need to call a function...
   User request:
   You used this tool: ...

   Here is its output:
   <tool result>
   ```
   The model saw redundant tool definitions (it already knew about them from the
   session history) plus a confusing "User request:" wrapper around the tool result.

3. Additionally, the gateway was sending `tools` and `tool_choice` fields in the
   upstream body. DeepSeek silently ignores these, but they added noise.

## Fix (applied 2026-08-19)

Two changes in `crates/obscura-gateway/src/providers/deepseek/direct.rs`:

1. **Skip tool prompt injection on follow-up turns.** When the last message in
   the request is `role: "tool"`, build the prompt as:
   ```
   User request:
   <original user question>

   <tool result from format_tool_results>
   ```
   This gives the model context about what was asked, without re-injecting tool
   definitions it already knows.

2. **Remove `tools`/`tool_choice` from the upstream body.** These fields are
   silently ignored by DeepSeek's internal endpoint and only add noise.

## How to Diagnose

If DeepSeek tool calling breaks again, check `/tmp/deepseek_upstream_body.json`
(dumped on every request). Look for:

- Double tool definitions in the `prompt` field
- `tools` or `tool_choice` present in the body
- Follow-up prompts that say `"User request:\nYou used this tool: ..."` wrapped
  in `"You have access to the following functions..."`

## Key Insight

DeepSeek's internal chat API is a **single-prompt** API. The `prompt` field is
the "user message" appended to an existing session. The full conversation history
lives in the DeepSeek session (keyed by `chat_session_id`), not in the prompt.
So on follow-up turns, you only need to send the new content (tool results),
not re-send everything.
