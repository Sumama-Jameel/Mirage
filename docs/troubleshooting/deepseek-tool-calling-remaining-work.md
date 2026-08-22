# DeepSeek Tool Calling - Remaining Work

## What's Done (this session)

### 1. Fixed double tool-prompt injection on follow-up turns
**File:** `crates/obscura-gateway/src/providers/deepseek/direct.rs`  
**Lines ~340-365**

When the last message is `role: "tool"` (follow-up turn), skip `inject_tool_prompt`
and instead just prepend `"User request:\n{original question}"` before the tool
result. The model already knows about tools from session history.

### 2. Removed useless `tools`/`tool_choice` from upstream body
**File:** `crates/obscura-gateway/src/providers/deepseek/direct.rs`  
**Lines ~560-565**

DeepSeek's internal endpoint ignores these fields. Removed them to reduce noise.

### 3. Strengthened format instruction
**File:** `crates/obscura-gateway/src/providers/tool_call.rs`  
**Line ~16**

Changed from:
```
When you need to call a function, output JSON wrapped in <tool_call> tags
```
To:
```
IMPORTANT: To call a function, you MUST output EXACTLY this format:
<tool_call>{"name":"function_name","arguments":{...}}</tool_call>
Example: <tool_call>{"name":"bash","arguments":{"command":"ls"}}</tool_call>
Do NOT output anything else.
```

### 4. Added Action/Action Input parser (partial)
**File:** `crates/obscura-gateway/src/providers/tool_call.rs`

Added `parse_action_input_format()` that parses:
```
Action: read
Action Input: {"filePath": "/etc/hosts"}
```

### 5. Added standalone JSON parser (partial)
**File:** `crates/obscura-gateway/src/providers/tool_call.rs`

Added `parse_standalone_json_tool_call()` that parses:
```
{"name": "read", "arguments": {"filePath": "/etc/hosts"}}
```

## What Still Needs to Be Done

### URGENT: Build & Test the Changes
Run `cargo build --release -p obscura-gateway` to verify the 5 changes compile.
The build was interrupted. Must complete before gateway restart.

### CRITICAL: Verify `parse_action_input_format` handles all edge cases
The current implementation:
- Only matches `Action:` on its own line
- Only matches `Action Input:` on the very next line
- May miss cases where there's whitespace or extra text between them
- May miss cases where DeepSeek outputs `Action:` and `Action Input:` in
  the same paragraph without a line break

**Test with this exact output from the screenshot:**
```
Action: read
Action Input: {"filePath": "/home/sumama/Private/Mirage/obscura/docs/InitialPlan"}
```

### CRITICAL: Verify `parse_standalone_json_tool_call` edge cases
The current standalone JSON parser:
- Requires the entire text to be valid JSON (starts with `{`, ends with `}`)
- May not handle cases where the model outputs explanatory text before/after
  the JSON (like "I'll use the read function\n```json\n{...}\n```")
- May not handle the case where DeepSeek outputs JSON with `Action Input:` prefix

### IMPORTANT: Handle the "second attempt" format from the screenshot
DeepSeek's second response was just explanatory text:
```
I need to read the plan file first to understand what needs to be implemented.
Let me check the content of the InitialPlan document.
```
This should NOT be parsed as a tool call - it's just text. The current code
should handle this correctly (no Action/JSON markers), but verify.

### IMPORTANT: Handle code-fenced JSON
DeepSeek sometimes outputs tool calls inside code fences:
```json
{"name": "read", "arguments": {"filePath": "..."}}
```
This is NOT currently parsed. Need to add code-fence parsing as a fallback
in `convert_xml_tool_calls`.

### IMPORTANT: Streaming path
The `XmlToolCallStripper` in `direct.rs` only handles `</think>` markers.
When DeepSeek emits `Action: read\nAction Input: {...}` during streaming,
the stripper won't catch it - the raw text leaks into content chunks.

Options:
a. Add Action/Input parsing to `XmlToolCallStripper.process()`
b. Or handle it in `process_content_text()` as a post-strip fallback
c. Or accept that streaming will leak and only parse in non-streaming mode

### RECOMMENDED: Test end-to-end with opencode
After building, restart gateway with `bash run.sh restart` and test with opencode
using `deepseek-chat` model. Try a coding task that requires tool calls (reading
files, running commands). Verify:
1. First turn: model outputs a tool call
2. Follow-up: model receives tool result and responds correctly
3. No more "Action: read" / "I'll read the plan" loops

### MINOR: Add tests for the new parsers
```rust
#[test]
fn parse_action_input_format_basic() { ... }

#[test]
fn parse_standalone_json_tool_call_basic() { ... }

#[test]
fn convert_xml_tool_calls_falls_back_to_action_format() { ... }
```

## Build Command
```bash
cargo build --release -p obscura-gateway
bash run.sh restart
```

## Key Files Changed
1. `crates/obscura-gateway/src/providers/deepseek/direct.rs` - Follow-up prompt fix + body cleanup
2. `crates/obscura-gateway/src/providers/tool_call.rs` - Stronger instructions + new parsers

## The Exact Failure Mode (from screenshot)
DeepSeek Chat was asked to read a file. Instead of outputting:
```
<tool_call>{"name":"read","arguments":{"filePath":"..."}}</tool_call>
```
It tried three different wrong formats:
1. `{"name": "read", "arguments": {"filePath": "..."}}` (raw JSON, not wrapped)
2. Explanatory text only (no tool call at all)
3. `Action: read\nAction Input: {"filePath": "..."}` (ReAct format)

The gateway only parses format #1 inside `<tool_call>` tags. Formats #2 and #3
are not caught, so the model loops endlessly without actually calling the tool.
