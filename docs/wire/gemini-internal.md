# Gemini internal API (StreamGenerate)

Evidence: `captures/gemini_capture.txt`, `providers/gemini/rpc.rs` (parser
unit-tested against captured envelope shapes).

## Endpoint

```
POST https://gemini.google.com/_/BardFrontendService/StreamGenerate
Content-Type: application/x-www-form-urlencoded
```

Body is the standard Google batchexecute wrapper:

- query params: `bl` (boq server build), `hl=en`, `reqid` (per-request counter)
- form fields: `f.req` (JSON-in-string), `at` (XSRF token from cookies)

## f.req payload

Nested JSON arrays (no object keys upstream — positions are the contract):

| slot | meaning |
|---|---|
| `[0][0]` | user prompt text |
| `[1][0]` | locale (`en`) |
| `[2]` | selected conversation state (response ids) or null |
| `[7]` | think mode (4 = shallow/fast) |
| `[9][]` | **tool slots** — MUST stay an array; the gateway always sends `[]` because client tools ride MTP/1, not native slots |
| `[13][...]` | image attachment references |
| `[79]` | protocol version flag |

## Response

NDJSON lines of the form `<len><json>`; each parsed line is a nested array:

- `payload[4][0][1][0]` — content delta (new format)
- `payload[0][2]` — content delta (old format)
- `payload[4][0][22][0]` — real rendered text when a `card_content`
  (`googleusercontent.com/card_content/`) prefix wraps the response
- `payload[4][0][28]` — **native tool call slots** (function name + args
  arrays); fallback scan across candidate slots for 2+-element arrays whose
  first element is a string
- citation metadata lives in sibling candidate slots and is surfaced as
  gateway `citations`
- conversation ids for continuation come from the trailing frame
  (`conversation_id`, `response_id`, `choice_id`)

## Gateway mapping

- Parser: `providers/gemini/rpc.rs::parse_response_line` + extractors.
- Tools: client tools are compiled into the MTP system prompt; slot [9]
  stays empty (strip_upstream_tools invariant). Model-emitted function_call
  fences / inline calls still parse via the legacy dialect as a fallback.
