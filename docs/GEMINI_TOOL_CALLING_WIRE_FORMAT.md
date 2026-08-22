# Gemini Web tool-calling wire format (live-verified 2026-08-07)

Re-verified against `crates/obscura-gateway/src/providers/gemini/rpc.rs` and
`direct.rs` on 2026-08-20: `card_content` handling, `candidate[22][0]`
render-text fallback, and the `candidate[28]` no-op fallback are all still
implemented exactly as described below.

## Root finding

Gemini Web's `StreamGenerate` API has **NO native structured tool-call field**.
In live raw responses the candidate slot `candidate[28]` (which the gateway
used to try to read) is always `[]`, even when a function is invoked. Tool
calls surface as **plain text** in the candidate text slot `candidate[1][0]`.

Third-party reverse-engineering references agree:
- HanaokaYuzu/Gemini-API: `CARD_CONTENT_RE = ^http://googleusercontent\.com/card_content/\d+`
  and when the text matches it, the real rendered text is at `candidate[22][0]`.
- nicobailon/pi-web-access: same `card_content` handling, "latest non-empty chunk".
- RYUK999/gemini-web2api (3.5k stars): confirms no native structured tool call;
  uses prompt-injected ` ```function_call {...} ``` ` blocks and parses 3 text
  formats; strips `googleusercontent.com/card_content` URLs from text.

## Live capture (gemini-3.5-flash, get_weather tool, tool_choice required)

Format A — card prefix + inline function call:
```
cand[1][0] = "http://googleusercontent.com/card_content/0\nget_weather(location='Paris')"
cand[22][0] = weather answer text   (card render; NOT the tool call)
cand[28] = []                       (native slot always empty)
```

Format B — fenced JSON block (no card prefix):
```
cand[1][0] = "```json\n{\"name\": \"get_weather\", \"arguments\": {\"location\": \"Paris\"}}\n```"
cand[22] = null
cand[28] = []
```

Prompt-injected format (gemini-web2api style, model trained to emit):
```
```function_call
{"name": "<tool_name>", "args": {<arguments>}}
```
```

## Implementation notes

- Parse tool calls from the **text**, not from `candidate[28]`.
- Strip the `card_content/N` prefix (and any `googleusercontent.com` artifact
  URLs) from content.
- Fall back to `candidate[22][0]` for display text only when the stripped
  `candidate[1][0]` is empty.
- Inline calls use Python-like `name(key='value', ...)` syntax; arguments must
  be parsed into JSON (handle single/double quotes, nested dicts/lists).
- Inject a tool-use instruction into the prompt so the model emits a stable
  parseable form.
- `candidate[28]` handling kept as a no-op fallback for safety (may be populated
  by future Gemini releases).
