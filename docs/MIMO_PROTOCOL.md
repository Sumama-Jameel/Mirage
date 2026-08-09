# Xiaomi MiMo Studio internal web API protocol (aistudio.xiaomimimo.com)

Research notes for the direct HTTP provider in `crates/obscura-gateway`.
Source of truth: the open-source `friesipayung/mimo-chat-openai` Go wrapper
(which reverse-engineers the same web API) and the web app itself. The provider
is already implemented and registered; update this file if the live API changes.

## Host / base

- Studio host: `https://aistudio.xiaomimimo.com`
- Chat completion: `POST /open-apis/bot/chat?xiaomichatbot_ph=<escaped>`
- Upload: 3-step `genUploadInfo` -> FDS PUT -> `parse`

## Authentication

Native browser cookie auth. Three cookies are required:

- `xiaomichatbot_serviceToken` (Firefox stores it with this name; some notes
  abbreviate it `serviceToken`)
- `userId`
- `xiaomichatbot_ph`

All read from the Obscura cookie jar (filtered on domains containing
`xiaomimimo.com`, `xiaomichatbot`, or `mimo`). No hardcoded credentials.

**Quotes are part of the stored values.** Firefox persists these cookies with
literal double quotes inside the value, e.g. `xiaomichatbot_ph` is
`"Ja1BTWN5M..."`. The server is strict about where the quotes may appear:

- In the **URL** `?xiaomichatbot_ph=...`: quotes must be stripped. Sending the
  quoted value yields HTTP 400 Bad Request (verified live).
- In the **Cookie header**: the raw quoted values work fine (verified live:
  the header may carry either quoted or trimmed values, both accepted).

The gateway strips quotes in `extract_ph()` (the only place ph feeds a URL);
`build_mimo_cookie_header()` keeps the raw values, matching what a real browser
sends. Verified live 2026-08-04: auth passes and the SSE stream starts only
with the trimmed URL ph.

## Chat request body

```jsonc
{
  "msgId": "<fresh random hex>",
  "conversationId": "<stable conversation uuid>",
  "query": "<user text>",
  "isEditedQuery": false,
  "modelConfig": {
    "enableThinking": true|false,
    "webSearchStatus": "active" | "inactive",
    "model": "<wire model id>",
    "temperature": 0.6,
    "topP": 0.9
  },
  "multiMedias": [ /* uploaded media items */ ]
}
```

`conversationId` is stable across turns: reusing it continues the conversation
server-side (no history resend). `msgId` is fresh per turn.

### Model id mapping

The authoritative source is `GET /open-apis/bot/config`, which returns
`modelConfigList`. Each entry has a `name` (display id) and a `model` (wire id
sent in the payload); they are NOT always equal. Verified live 2026-08-04:

| public id (name)    | wire id (model)      |
|---------------------|----------------------|
| mimo-v2.5-pro       | mimo-v2.5-pro        |
| mimo-v2.5           | mimo-v2.5            |
| mimo-v2-flash-studio| mimo-v2-flash        |
| mimo-v2-pro         | mimo-v2-pro          |
| mimo-omni           | mimo-v2-omni         |
| unknown             | mimo-v2.5-pro        |

The older mimo-chat-openai mapping sent `mimo-v2-flash-studio` as the wire id;
that is wrong today, the config `model` field is `mimo-v2-flash`.

All five config entries report `thinkingDefaultOn: true`, so the gateway
defaults thinking on for every model (the previous flash/omni-think-off rule
is stale).

**Server-side availability varies by model.** Live tests (2026-08-04) show the
chat API serves `mimo-v2.5-pro` and `mimo-v2.5` (both stream real answers),
but `mimo-v2-flash`, `mimo-v2-pro`, and `mimo-v2-omni` return the SSE error
`服务器繁忙，请稍后再试` ("server busy, try again later") even though they are
present in `/open-apis/bot/config`. This is a server-side state, not a wire
format issue: the exact config wire ids were exercised and still rejected.
Treat these three as temporarily unavailable until the API serves them again.

## Response format: SSE

`text/event-stream` with `event:` / `data:` frames. The provider:

- splits reasoning from the final answer using `<think>` / `</think>` markers
- emits a final `MiMoUsage` trailer with prompt/completion/total tokens
- surfaces the conversation via `session_url` for continuation

Verified live event shapes (2026-08-04):

- `event:dialogId` / `data:{"content":"989xxxx"}`: server echo of the
  conversation id. NOT user-visible content; the gateway skips it (it used to
  leak a stray numeric delta as the first content chunk).
- `event:message` / `data:{"type":"text","content":"..."}`: deltas. Content is
  prefixed with `\u0000` (NUL) right after each `<think>` / `</think>` marker
  (e.g. `<think>\u0000The user is asking...`). The gateway strips control
  bytes so neither NUL nor the markers leak into output.
- `event:error` / `data:{"type":"text","content":"服务器繁忙，请稍后再试"}`:
  server-side rejection (see model availability note above).
- `event:usage` / `data:{"promptTokens":...,"completionTokens":...,"totalTokens":...}`.
- `event:finish` / `data:{"content":"[DONE]"}`.

## Web search / deep research

The web API has a `webSearchStatus` field on `modelConfig`
(`"active"` / `"inactive"`). The gateway maps the search toggle onto this
field. There is no separate "deep research" endpoint in the bot chat API.

## Function calling

The web chat API has NO native function-calling channel: the payload only
carries `enableThinking`, `webSearchStatus`, the text `query`, and
`multiMedias`. The mimo-chat-openai reference exposes `web_search` as the only
tool type. The gateway therefore uses the XML `<tool_call>` fallback (same as
DeepSeek/Gemini): function definitions are injected into the `query` text and
`<tool_call>{"name":...,"arguments":{...}}</tool_call>` markers are parsed out
of the reply. Verified live 2026-08-05:

- Non-streaming: `finish_reason: "tool_calls"` with a native `tool_calls`
  array, `<tool_call>` markers stripped from content.
- Streaming: content deltas are run through `XmlToolCallStripper`
  (`tool_call.rs`) so markers never leak; a single final `tool_calls` chunk
  with `finish_reason: "tool_calls"` is emitted.
- Tool-result round-trip: assistant `tool_calls` and `role: "tool"` messages
  are rendered into the query text (`format_tool_results`), so a follow-up
  turn continues with a final text answer instead of re-calling.

The model also emits a `webSearch` control token (without spaces) at the start
of content when it decides to run a web search on its own. The gateway strips
it (`strip_control_tokens`); it is not user-visible text.

Note: a fresh thread (no `session_url`) always starts a new conversation. The
gateway session id is a pooled browser session, so reusing its stored
conversation would wrongly continue a prior client's thread and drop tool
context. Continuation only happens when the client passes `session_url`.

## File upload

3-step flow:

1. `genUploadInfo` (obtains upload credentials / URL for FDS object storage)
2. `PUT` the file bytes to FDS with a `Content-MD5` header
3. `parse` to finalize and get the media reference

`multiMedias` carries the resulting media items. Supported MIME types map to
extensions (image/jpeg,png,gif,webp,bmp / audio/mpeg,wav,flac,m4a,ogg /
video/mp4,mov,avi,wmv); 50MB cap.

## Session continuation

Disk-persisted `MiMoSessionState { conversation_id, model, enable_thinking,
web_search_status }` in `state.rs`, keyed by gateway session id. Reusing the
`conversationId` continues the thread; `session_url` exposes
`https://aistudio.xiaomimimo.com/chat/{conversationId}`.

## Files

- `crates/obscura-gateway/src/providers/mimo/mod.rs` — provider, models, validation
- `crates/obscura-gateway/src/providers/mimo/direct.rs` — SSE chat client
- `crates/obscura-gateway/src/providers/mimo/state.rs` — session store
- `crates/obscura-gateway/src/providers/mimo/upload.rs` — upload flow + cookie helpers
