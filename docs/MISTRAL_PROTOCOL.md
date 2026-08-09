# Mistral Le Chat internal web API protocol (chat.mistral.ai)

Research notes for the native direct HTTP provider in `crates/obscura-gateway`.
Source of truth: the minified JS bundles of the Le Chat web app (downloaded to
`/home/sumama/.opencode_tmp/opencode/chunks/`). Do not re-research what is
documented here; re-verify only if a field no longer matches live traffic.

## Host / base

- Web app base URL: `https://chat.mistral.ai`
- Chat completion: `POST /api/chat`
- Resume interrupted completion: `POST /api/chat/resume`
- Session bootstrap: `GET /api/session`
- File upload: `POST /api/file` (multipart form-data)

## Authentication

Native browser cookie auth, no API key. The web app reads the session from
`GET /api/session`, which returns a JSON blob with `status: "assigned"` once the
cookie session is valid. The gateway uses cookies from the Obscura cookie jar
(matching domain `chat.mistral.ai`), not hardcoded credentials.

### Cloudflare clearance gate (verified live 2026-08-05)

`chat.mistral.ai` is behind Cloudflare: a plain rustls reqwest request (its TLS
fingerprint) is answered `403` with the "Just a moment..." challenge, and the
API is never reached.

The gateway sends every chat/upload request through `obscura_net::StealthHttpClient`
(the wreq client with the Chrome 145 TLS/HTTP fingerprint plus the session cookie
jar). That fingerprint clears the Cloudflare gate, so no `cf_clearance` cookie is
required and the provider no longer navigates a browser session for it
(`navigate_to_mistral` was removed from `providers/mistral/mod.rs`).

## Request flow (verified live 2026-08-05)

The live API has no single create-and-start `mode`; the web app always:

1. Creates the chat server-side via the tRPC mutation
   `POST /api/trpc/message.newChat`, which returns a real `chatId`.
2. Sends the first message to `POST /api/chat` with `mode: "start"` and that
   `chatId`.
3. Sends later turns with `mode: "append"` and the same `chatId`.

Every `mode` branch of `/api/chat` requires an existing `chatId`; `start`,
`append`, `edit`, `rewrite`, `retry`, `toolCallConfirmation` are the only valid
modes (a made-up `chatId` yields `404 {"detail":"Chat not found","code":6300}`,
and `mode: "create"` is rejected by the union validator).

### Create chat (`POST /api/trpc/message.newChat?batch=1`)

```jsonc
{ "0": { "json": {
  "content": "…",            // first user message text
  "files": [],               // uploaded file refs [{type,url,name}]
  "features": [],
  "integrations": [],
  "libraries": [],
  "projectId": null
} } }
```

Response (tRPC v11 batch, array form): the mutation resolves to a message object.
`chatId` lives at `data.json.messages.chatId` (or `data.json.chat.id`); the
gateway walks the whole value for the first string `chatId`.

### Send / continue (`POST /api/chat`)

`mode: "start"` for the first message, `"append"` for continuation. Both carry
the real `chatId` and are otherwise identical:

```jsonc
{
  "chatId": "<uuid>",
  "mode": "append",
  "model": "<model-id>",
  "messageInput": [{ "type": "text", "text": "…" }],  // or content chunks
  "messageFiles": [],          // [{type, url, name}] after upload
  "messageId": "<uuid>",       // client-generated per turn
  "features": [],              // feature ids (see below)
  "libraries": [],
  "integrations": [],
  "clientPromptData": { "currentDate": "…", "userTimezone": "…" },
  "activeCanvaId": undefined,
  "stableAnonymousIdentifier": "…",
  "supportedTaskCallbacks": ["…"],
  "customConfig": undefined,
  "boostMode": true|false,
  "disabledFeatures": []
}
```

Note: `chatId`, `features` and `libraries` are added by the caller when
appending. Both start and append bodies include `model` explicitly, plus
`boostMode`, `clientPromptData`, `stableAnonymousIdentifier`,
`supportedTaskCallbacks`, `disabledFeatures` (see the body above). `messageInput`
is always an array of `{type:"text", text}` chunks.

### Resume (`POST /api/chat/resume`)

```jsonc
{
  "chatId": "<uuid>",
  "messageId": "<uuid>",
  "supportedTaskCallbacks": ["…"],
  "customConfig": undefined,
  "boostMode": true|false,
  "callbackResults": []
}
```

Resume is only needed for interrupted streams (recoverable network errors). For
the gateway, a normal chat/append flow does not need resume unless we add
auto-resume later.

## Response format: NDJSON stream

The response body is NDJSON, one JSON object per line. Each line is either a
JSON object with `{"code": <int>, "value": <json>}` or a raw string in the form
`<code>:<json>` (the line parser splits on the first `:` and maps codes). For
codes `>= 15` the value is parsed with the fast-json-patch parser; for codes
`< 15` plain `JSON.parse`.

Codes (module `2785233` in `0_x4cyb675mg~.js`):

| code | name                 | value shape                                        |
|------|----------------------|----------------------------------------------------|
| 0    | text                 | string (a text delta chunk)                        |
| 1    | moderation           | object                                             |
| 2    | references           | object                                             |
| 3    | canva                | {id, version}                                      |
| 4    | canva_token          | object                                             |
| 5    | tool_call            | {id, name, type, publicArguments, publicResult, isDone, isInterrupted, success, startTime, endTime, requiresConfirmation, confirmationStatus, …} |
| 6    | error                | object                                             |
| 7    | image                | {url, …}                                           |
| 8    | done                 | null                                               |
| 9    | references_ids       | array                                              |
| 10   | file_reference       | {fileReference, fileAlt, fileUrl}                  |
| 11   | widget               | {source}                                           |
| 12   | reasoning            | string                                             |
| 13   | deep_research_event  | object                                             |
| 14   | tool_reference       | {url, favicon, title, description}                 |
| 15   | state_update         | {type: "message"\|"canvas"\|"chat"\|"bootstrap", …}|
| 16   | disclaimer           | {disclaimers: […] }                                |

### state_update handling (verified live 2026-08-05)

Every event line for codes `6/15/16` is wrapped as `{"json":{...}}`; the gateway
unwraps that before dispatching. Observed events:

- `{type: "bootstrap", chat, messages, canvas}` — first event; the chat record
  and full message list. Gives us `chat.id` (chatId) and the initial messages.
- `{type: "message", messageId, messageVersion, patches}` — stream updates for
  one assistant message. Patches are fast-json-patch operations applied on top
  of the accumulated message object. Patch events for several messages are
  interleaved (an assistant content message and a separate moderation message),
  so the accumulator must be keyed by `messageId`, not a single shared value.
- `{type: "canvas", patches}`, `{type: "chat", patches}` — not needed by the
  gateway.

### Patch model

`isRootPatch` is `{op: "replace", path: "/"}` (or empty path) and replaces the
entire message value. Other ops are `add`/`replace`/`remove`/`append` on paths
into the message JSON. `append` concatenates the new string onto the string at
the given path. Paths traverse array indices (`/contentChunks/0/text`), so the
path resolver must index arrays, not only object keys.

### Text extraction from patches

The message object is seeded by the bootstrap/root patch as
`{content: "", contentChunks: null, ...}` and the real text streams into
`contentChunks` (an array of chunk objects); `content` stays an empty string
for the whole turn. Streaming text arrives as patches that grow the chunk's
`text`:

- text chunks: `{type: "text", text: "…"}` (with `_context: {type:"reasoning"}`
  marking a reasoning chunk, which is emitted as reasoning, not content)
- tool call chunks: `{type: "tool_call", id, name, type, publicArguments,
  publicResult, isDone, isInterrupted, success, startTime, endTime,
  requiresConfirmation, confirmationStatus}`

So an OpenAI-compatible gateway implementation can:

1. parse each NDJSON line to a code + value; unwrap `{"json":{...}}`
2. on `bootstrap`: record chatId + messageId/version
3. on `state_update.message`: apply patches to that message's accumulator (keyed
   by messageId); then read `contentChunks` from the patched message
4. emit text/reasoning deltas (compare to the previous full text, emit the new
   suffix); skip messages without `contentChunks` (e.g. moderation) so they do
   not disturb the delta trackers
5. on `tool_call` chunks (either code 5 direct or via patches): emit a
   `tool_calls` delta, including `isDone` markers for call completion
6. on `done`: finish the stream

### Reasoning

Reasoning deltas arrive as `code:12` `reasoning` stream parts and/or as
`state_update` patches on the message. The message schema includes
reasoning-related fields; extract them from the patched message when present.
Thinking toggle maps to the `beta-reasoning` feature.

## Feature ids (module `088.feyq_gh1p.js`)

```
beta-trampoline, beta-imagegen, beta-websearch, beta-code-interpreter,
beta-reasoning, beta-mcp, beta-airweave-connectors, beta-audio-transcription,
beta-realtime-transcription, beta-memory, beta-deep-reasoning,
beta-fast-reasoning, beta-deep-research, agentic-harness, agentic-fast
```

- Thinking on  = `beta-reasoning` (the `beta-deep-reasoning` / `beta-fast-reasoning` variants exist too)
- Deep research = `beta-deep-research`
- Web search = `beta-websearch`
- Code interpreter = `beta-code-interpreter`
- `chatTools` list = `[beta-code-interpreter, beta-imagegen, beta-websearch, beta-trampoline]`
- `chatFeatures` = all of the above + `beta-mcp`, connectors, memory, etc.

## Model ids seen in the bundles

`mistral-large-2411`, `mistral-large-2512`, `mistral-medium-latest`,
`mistral-medium-2508`, `mistral-medium-2508-lightspeed`, `mistral-medium-3-5`,
`mistral-small-latest`, `mistral-small-4`, `mistral-small-2603`,
`mistral-deepresearch-2507`. Also `devstral-latest`, `codestral-latest`.

## File upload (verified live 2026-08-05)

Primary path (the web app's, and what the gateway uses):

1. `POST /api/trpc/file.uploadFile?batch=1` (JSON `{type, count:1, includeReadUrl:true}`)
   returns `data.json.uploadURLs[0]`, an Azure Blob SAS PUT URL.
2. `PUT <uploadURLs>` with `Content-Type: <mime>` + `x-ms-blob-type: BlockBlob`
   uploads the bytes; the URL already authenticates (no session cookies).

The tRPC mutation goes through the stealth client (Cloudflare-gated); the Azure
PUT does not need it and uses plain reqwest because the body is binary (the
stealth client's `send_single` is string-only).

Fallback path (`POST /api/file` multipart `file` + `type`, response `{url, …}`)
mirrors the web app's proxy branch and is only reached when the tRPC upload
fails. It still uses plain reqwest and may hit the Cloudflare gate.

Use the returned URL in `messageFiles: [{type, url, name}]`.

## References to inspect on re-verification

- `0_x4cyb675mg~.js` — NDJSON decoder module `2785233`, `parseComplexResponse`,
  `useConsumeChatStreamResponse`, message patch cache, request body build for
  create, `useChatCreate` (`message.newChat.useMutation` returns `{chatId}`).
- `0lc8l1x7rgy8z.js` — append flow, resume flow, `webSupportedTaskCallbacks`.
- `15tsyuj9uey0o.js` — `isRootPatch`, fast-json-patch `apply`/`diff`/schema.
- `088.feyq_gh1p.js` — feature/chatTools constants.
- `1igs0aicx3mcg.js` — session bootstrap handling.
