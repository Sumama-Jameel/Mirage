Yes. Below is a production-grade plan to fix the broken providers/features without turning the gateway into a manual capture/script/extract-every-time system.

This plan is built around one rule: **runtime model calls must never require manual labour, one-off scripts, or per-call extraction**. Anything that needs browser capture, challenge refresh, or protocol discovery must be:

1. captured once,
2. compiled into a cached provider manifest/session state
3. reused at runtime,
4. automatically re-healed only when upstream drift/auth/challenge is detected,
5. rate-limited and circuit-broken so it cannot loop.

---

# 1. High-level diagnosis from the files you provided

## 1.1 What your captures prove

### GLM: direct API is alive, UI fallback is the wrong path

Your `glm_capture.txt` proves that the direct GLM path is usable:

- `POST /api/v2/chat/completions?timestamp=...&requestId=...&user_id=...&version=0`
- response is SSE:
  ```json
  data: {"type":"chat:completion","data":{"delta_content":"...","phase":"thinking"}}
  ```
- file upload exists:
  ```json
  POST /api/v1/files/
  ```
  returns:
  ```json
  {
    "id": "...",
    "filename": "...",
    "meta": {
      "cdn_url": "..."
    }
  }
  ```
- chat creation exists:
  ```json
  POST /api/v1/chats/new
  ```
  with fields:
  ```json
  {
    "models": ["glm-5.2"],
    "enable_thinking": true,
    "reasoning_effort": "max",
    "auto_web_search": true
  }
  ```

This is huge. It means GLM should be moved to **direct-only**. The UI fallback is not the future; it is a diagnostic dead end.

### ChatGPT: your capture is incomplete

`chatgpt_capture.txt` captured useful auxiliary endpoints:

- `/backend-api/conversation/{id}/stream_status`
- `/backend-api/sentinel/ping`
- `/backend-api/conversations`
- `/backend-api/lat/r`

But it did **not** capture the main request that matters most:

- `POST /backend-api/conversation`

Without that exact request/response shape, tool calling, continuation, thinking, and upload cannot be fixed reliably.

So ChatGPT needs a **targeted one-time capture healer**, not manual labour per call.

### Kimi: capture file contains instructions only, not actual traffic

`kimi_capture.txt` currently contains only the procedure, not the captured JSON payload. Therefore the exact Kimi wire format is not yet proven by the attached file.

Kimi K3 needs a full evidence capture for:

- `POST /api/chat`
- streaming chunks
- tool call request/response
- file upload
- parse/process flow
- thinking toggle
- search toggle
- continuation

### Gemini: capture file also lacks actual request/response evidence

`gemini_capture.txt` has only instructions. We do not yet have the actual Gemini internal request/response shape in the provided file.

So Gemini needs a targeted capture for:

- chat/stream endpoint
- search toggle
- deep research toggle
- upload
- tool behaviour
- continuation

### Grok: you have a useful clue, but not the full flow

`grok_gc.json` gives a useful hint:

```json
{
  "hi": "POST!/rest/app-chat/conversations/.../load-responses!...",
  "tok": "..."
}
```

This suggests Grok uses a token/challenge flow around `/rest/app-chat/conversations/...`.

But we still need the full live sequence:

- initial challenge
- token exchange
- conversation create/load
- message send
- stream/response shape
- upload response shape

Grok must not hardcode expired anti-bot constants. It needs a **dynamic challenge/session vault**.

---

# 2. Core architectural principle: replace hardcoded provider hacks with a Provider Protocol Registry

The gateway should not keep accumulating special-case `if provider == "glm"` hacks inside random places.

Instead, introduce a **Provider Protocol Registry**.

## 2.1 What the registry contains

For each provider, store a versioned manifest describing:

- auth requirements
- required cookies/localStorage keys
- base URLs
- endpoint templates
- request builders
- header rules
- streaming parser rules
- upload flow
- tool-calling capability
- thinking capability
- search/research capability
- continuation mode
- error classification rules
- healing policy
- rate-limit policy
- model capability overrides

Example conceptually:

```text
provider: glm
auth:
  cookies: [session token, user_id related cookies]
endpoints:
  chat: /api/v2/chat/completions
  files: /api/v1/files/
  new_chat: /api/v1/chats/new
stream:
  type: sse
  line_prefix: "data: "
  json_event: true
  delta: .data.delta_content
  reasoning_when: .data.phase == "thinking"
features:
  native_tools: unknown
  native_upload: true
  native_thinking: true
  native_search: unknown
  continuation: chat_id/message_id
```

This manifest is **not edited by hand on every call**. It is generated/updated only when:

- initial onboarding happens,
- protocol drift is detected,
- auth/challenge refresh happens,
- a provider breaks in a classified way.

## 2.2 Why this fixes “manual labour”

Because runtime calls use the cached manifest.

Manual work becomes a one-time onboarding/healing action, not a per-request dependency.

---

# 3. Replace fragile startup cookie import with a Session Vault + Provider Health States

Your current startup imports the whole browser profile and a single missing cookie can kill an entire provider. That is too brittle.

## 3.1 Create a Session Vault

The vault stores per-provider session state:

- cookies
- localStorage entries
- tokens
- device ids
- challenge tokens
- last successful build/version marker
- last verified timestamp
- capability flags

This vault should be:

- local-only,
- encrypted at rest if possible,
- redacted in logs,
- independently loadable per provider.

## 3.2 Provider health states

Each provider gets a state machine:

- `HEALTHY`
- `DEGRADED`
- `AUTH_MISSING`
- `AUTH_EXPIRED`
- `CHALLENGE_REQUIRED`
- `RATE_LIMITED`
- `QUOTA_EXHAUSTED`
- `PROTOCOL_DRIFT`
- `DISABLED`

This prevents one broken provider from poisoning the whole gateway.

## 3.3 Lazy per-provider startup

At gateway startup:

1. load config,
2. start HTTP listener immediately,
3. initialize providers independently,
4. validate each provider’s cookies/tokens independently,
5. mark providers as healthy/unhealthy independently.

This solves the “one missing cookie kills everything” problem.

---

# 4. Self-healing design that does not become a loop

You asked for self-healing, but it must not become:

- infinite retries,
- endless cargo/test loops,
- repeated manual extraction,
- browser automation on every call.

So the healing system must be **classified, bounded, and cached**.

## 4.1 Error classifier

Every provider error is classified into one of these categories:

| Class | Meaning | Action |
|---|---|---|
| `AUTH_MISSING` | required cookie/token absent | pause provider, request login once |
| `AUTH_EXPIRED` | session expired | refresh from browser profile/vault |
| `CHALLENGE_REQUIRED` | anti-bot/captcha/userTag/etc | quarantine, trigger human-assisted heal |
| `RATE_LIMITED` | 429 / quota window | backoff, cooldown, fail fast |
| `QUOTA_EXHAUSTED` | paid plan/credits exhausted | fail fast with remediation |
| `PROTOCOL_DRIFT` | response shape changed | trigger manifest relearn |
| `PARSE_FAILURE` | unexpected body/SSE format | snapshot raw response, retry conservative parser |
| `NETWORK` | timeout/reset | retry with backoff |
| `MODEL_UNSUPPORTED` | model lacks feature | return clean capability error |

## 4.2 Healing rules

### Auth expired

- reload cookies from Firefox profile,
- validate with a cheap provider-specific ping,
- retry once.

If still failing:

- mark provider `AUTH_EXPIRED`,
- do not hammer.

### Challenge required

This includes:

- GLM `FRONTEND_CAPTCHA_REQUIRED`
- GLM `IllegalUserTag`
- Grok anti-bot 403
- any human verification wall

Policy:

- do **not** attempt blind automated captcha solving,
- quarantine provider,
- open a healing browser session using the existing logged-in profile,
- if human interaction is required, emit a clear `HEAL_HUMAN_REQUIRED` state,
- after successful human verification, capture refreshed session/tokens and resume.

This is the only sane way to remain self-healing without becoming an anti-bot evasion machine.

### Protocol drift

Trigger when:

- parser sees unknown SSE event types,
- expected JSON fields disappear,
- status is 200 but content is empty repeatedly,
- provider returns a new error shape.

Action:

1. snapshot raw request/response,
2. run a one-time capture healer for that provider,
3. regenerate/update manifest,
4. verify with canary requests,
5. resume.

This is not per-call. It is per-protocol-change.

### Rate limit / quota

Action:

- honour `Retry-After` if present,
- exponential backoff with jitter,
- provider-level circuit breaker,
- local token bucket,
- queue requests instead of bursting.

For Minimax quota, the gateway should fail fast and surface remediation, not keep burning time.

---

# 5. Runtime request pipeline

Every request should follow the same pipeline:

```text
OpenAI-compatible request
  -> normalize request
  -> resolve provider/model
  -> check capability matrix
  -> check rate limiter / circuit breaker
  -> acquire warm session
  -> build native request from manifest
  -> send direct HTTP/SSE
  -> parse native stream/response
  -> normalize to OpenAI-compatible output
  -> persist session continuation state
  -> return result
```

No UI automation in this path.

---

# 6. Provider-by-provider fix plan

---

## 6.1 GLM — highest priority quick win

### Current root cause

GLM is currently half-integrated:

- direct path was captcha-blocked earlier,
- UI fallback cannot find textarea,
- some models return empty content because parser is wrong or incomplete.

But your new capture shows direct `/api/v2/chat/completions` is alive.

### Target design

GLM becomes **direct-only**.

### Implementation plan

#### A. Remove UI fallback as a primary path

UI fallback should be demoted to:

- diagnostics,
- healing capture,
- optional manual debug.

It must not be used for normal traffic.

#### B. Implement direct request builder

Use captured query pattern:

```text
POST /api/v2/chat/completions
  ?timestamp=<ms>
  &requestId=<uuid>
  &user_id=<user_id>
  &version=0
```

The exact request body must be captured once with Whelmer/body capture. It likely includes:

- chat id,
- message node/history,
- selected model,
- thinking flags,
- search flags,
- attachments.

Do not guess. Capture once, store as fixture, then build from manifest.

#### C. Implement SSE parser

From capture:

```json
{
  "type": "chat:completion",
  "data": {
    "delta_content": "...",
    "phase": "thinking"
  }
}
```

Parser rules:

- strip `data: `
- parse JSON event
- if `phase == "thinking"`:
  - append to `reasoning_content`
- else:
  - append to normal content
- continue until final/done event
- if stream ends without explicit done, synthesize `finish_reason: stop`

This likely fixes the “empty content” models, because the current parser is probably not recognizing this event shape.

#### D. Implement native upload

Use:

```text
POST /api/v1/files/
```

Response gives:

- `id`
- `meta.cdn_url`
- `meta.content_type`
- `meta.size`

The chat request should attach the uploaded file by its native identifier / CDN URL, not by hacky text injection.

#### E. Implement session continuation

From `/api/v1/chats/new`, GLM clearly has chat ids and message tree structure.

Store:

- chat id
- current message id
- parent id if present
- model
- feature flags

For continuation:

- reuse chat id,
- send only the new user message,
- preserve thread context natively.

#### F. Captcha handling

If GLM returns:

- `FRONTEND_CAPTCHA_REQUIRED`
- `人机验证失败...`
- `IllegalUserTag`

then:

1. mark provider `CHALLENGE_REQUIRED`,
2. stop retry storm,
3. trigger browser-based session heal,
4. require human verification if needed,
5. refresh vault and resume.

#### G. Verification matrix for GLM

For each GLM model, test:

- non-stream basic chat
- stream chat
- continuation
- upload where supported
- thinking where supported
- search/research where supported
- tool behaviour where supported

Expected result:

- no UI fallback,
- no empty content,
- no textarea timeout,
- no captcha loop.

---

## 6.2 ChatGPT — fix continuation first, then tools/thinking/search/upload

### Current root cause

The gateway is not fully aligned with ChatGPT’s actual web conversation flow.

Evidence gaps:

- missing `POST /backend-api/conversation` request body,
- missing SSE event taxonomy,
- missing continuation identifiers,
- missing tool behaviour evidence,
- missing upload flow evidence.

### Target design

ChatGPT must be driven by the real `backend-api` conversation flow, not by assumptions.

---

### A. Capture the real conversation request

Need one clean capture of:

- normal message send,
- continuation message send,
- streaming response,
- final message metadata.

Most important fields to discover:

- `conversation_id`
- `parent_message_id`
- message node structure
- model slug
- whether history is resent or only the new message
- how `session_url` maps to conversation state

This is the key to fixing continuation.

---

### B. Fix session continuation properly

The reported failure:

> session_url alone does not continue the thread unless full history is resent

This usually means the gateway is not preserving the correct continuation identifiers.

ChatGPT web continuation typically depends on more than just a URL:

- conversation id
- parent message id
- message tree node
- sometimes client-generated ids

Plan:

1. store a ChatGPT session object:
   ```text
   conversation_id
   last_assistant_message_id
   last_user_message_id
   model
   creation_time
   ```
2. encode enough information in `session_url` or in a gateway-side session store,
3. on follow-up, send only the new user message plus the correct continuation identifiers,
4. verify with the codeword test.

If live evidence proves that ChatGPT truly requires full history resend, then:

- mark ChatGPT continuation mode as `gateway-history-required`,
- keep a server-side conversation store,
- transparently resend history,
- document this as a provider-specific exception.

But first attempt must be native continuation.

---

### C. Native tool calling

This is the hardest ChatGPT item.

The current live behaviour says standard ChatGPT models do not emit native `tool_calls` through the web endpoint.

Plan:

1. research whether the web endpoint supports native tools for:
   - built-in tools,
   - GPT actions,
   - plugin-style calls,
   - internal tool call events.
2. capture any UI flow where ChatGPT actually calls a tool/function.
3. if native tool call evidence exists:
   - implement parser,
   - expose native `tool_calls`.
4. if no native tool path exists for standard models:
   - create an evidence dossier,
   - mark those models as native-tool unsupported,
   - use XML fallback only where GOAL allows it.

Do not fake native tool calling.

---

### D. Thinking toggle

Current issue:

- thinking accepted,
- no reasoning surfaced,
- previous verification hit `401 token_expired`.

Plan:

1. refresh session,
2. capture o-family model with thinking enabled,
3. inspect SSE events for:
   - hidden reasoning blocks,
   - partial thought events,
   - special metadata,
   - separate reasoning stream.
4. if reasoning is present:
   - map to `reasoning_content`.
5. if not present:
   - mark thinking capability as accepted-but-not-surfaced,
   - do not pretend reasoning exists.

---

### E. Search / citations

Plan:

1. capture search-enabled request,
2. inspect response for:
   - annotations,
   - citations,
   - search result metadata,
   - source URLs.
3. if citations exist in payload:
   - parse and expose them.
4. if not:
   - mark search as supported but citation-less,
   - or mark citation extraction unsupported if the UI itself does not expose it.

---

### F. Upload

Need capture of ChatGPT file flow:

- file upload endpoint,
- file id,
- attachment reference in conversation request.

Then implement native upload instead of text pasting.

---

### G. Auth/session healing

ChatGPT session should be validated with cheap endpoints such as:

- `/backend-api/me` if available,
- `/backend-api/conversations`,
- `/backend-api/sentinel/ping` if useful.

On `401 token_expired`:

- refresh cookies from profile,
- revalidate,
- retry once.

If still invalid:

- mark `AUTH_EXPIRED`.

---

## 6.3 Kimi — fix K3 by building the correct native flow from capture

### Current root cause

Kimi K3 is degraded across:

- streaming,
- tool calling,
- upload,
- thinking,
- search,
- continuation in stream mode.

And the provided capture file does not yet contain the actual traffic.

### Target design

Kimi must be fixed from real `/api/chat` evidence, not guesses.

---

### A. Capture the true Kimi request/response flow

Need:

- initial chat creation,
- message send,
- SSE chunks,
- final event,
- continuation payload,
- upload flow,
- parse/process flow,
- tool/search/thinking toggles.

Important endpoints to capture:

- `/api/chat`
- any `/api/conversation`
- any `/api/file`
- any `/api/file_parse` or similar
- any `/api/search` or search-result injection endpoint

---

### B. Fix streaming parser

Current symptom:

> 1 data chunk, 0 content deltas

That means parser is probably looking for the wrong field or wrong event type.

Plan:

1. record raw SSE chunks,
2. identify real delta fields,
3. identify reasoning fields,
4. identify citation fields,
5. implement incremental parser.

Expected output:

- multiple content deltas,
- proper final message,
- session continuation info in stream.

---

### C. Fix upload and parse flow

Current symptom:

> file will be processed async, then gateway continues too early

Plan:

1. upload file natively,
2. receive file id / token,
3. if parse is async:
   - poll parse status until complete,
   - timeout gracefully,
   - do not send chat before parse completes,
4. attach parsed file token to chat request.

This should eliminate:

- empty content,
- 85s wasted waits,
- unparsed-file failures.

---

### D. Fix tool calling

Need evidence:

- Does Kimi K3 support native tools?
- Does K2.7-code support it better?
- What is the native field shape?

Plan:

1. capture a tool-enabled request,
2. if native tool call exists:
   - implement,
   - expose `tool_calls`.
3. if only some Kimi models support tools:
   - set per-model capability flags.
4. if no native support:
   - use XML fallback only after evidence.

---

### E. Fix thinking and search

Capture:

- thinking toggle ON,
- search toggle ON.

Then parse:

- reasoning chunks,
- citations,
- source URLs.

If Kimi returns no reasoning/citations at wire level, mark the capability honestly.

---

### F. Fix `create chat failed`

This usually means one of:

- missing session token,
- wrong header,
- wrong chat initialization sequence,
- stale conversation id.

Plan:

- add a Kimi session bootstrap,
- recreate chat session when stale,
- retry once with fresh session,
- circuit-break if repeated.

---

## 6.4 Gemini — capture the real internal generate path and parse grounding correctly

### Current root cause

Gemini has missing evidence in the attached capture, and search citations are not being extracted.

### Target design

Implement Gemini from the real internal generate endpoint, not from assumptions.

---

### A. Capture the actual generate/stream endpoint

Likely candidates:

- internal `StreamGenerate`-style endpoint,
- frontend RPC endpoint,
- protobuf or JSON streaming endpoint.

Need:

- request body,
- headers,
- stream format,
- model selector fields,
- search toggle fields,
- upload flow.

Do not hardcode unknown protobuf fields unless captured.

---

### B. Fix model selection

Your BROKEN file mentions model routing via field values and headers.

Plan:

- store model mapping in Gemini manifest,
- tie mapping to captured evidence,
- add auto-relearn if model selection breaks,
- avoid hardcoded magic values in source where possible.

---

### C. Fix search citations

Search toggle accepted but 0 citations means one of:

1. request is not actually enabling grounding,
2. citations are in a field the parser ignores,
3. the web endpoint does not expose citations.

Plan:

1. capture a search-enabled response,
2. inspect for:
   - grounding metadata,
   - source metadata,
   - search result blocks,
   - citation chips,
   - URL references.
3. if present:
   - parse and expose.
4. if not present:
   - mark citation extraction unsupported for web endpoint.

---

### D. Fix upload

Capture:

- image upload endpoint,
- attachment token,
- how the generate request references uploaded media.

Then implement native attachment support.

---

### E. Fix continuation

Gemini likely uses internal conversation/session ids.

Plan:

- capture first turn,
- capture second turn,
- identify which ids must persist,
- store them in session object,
- verify codeword recall.

---

## 6.5 Grok — replace expired constants with a dynamic challenge vault

### Current root cause

Grok is dead because of:

- expired hardcoded anti-bot constants,
- 403 anti-bot rejection,
- upload parser expecting `url` when live response gives `fileUri` / `parsedFileUri`.

### Target design

Grok needs a dynamic session/challenge system, not hardcoded constants.

---

### A. Build a Grok challenge vault

Store:

- anti-bot token(s),
- session token(s),
- conversation token(s),
- expiry marker,
- deploy/build marker if visible.

Use evidence from `grok_gc.json` as a starting point:

```json
{
  "hi": "POST!/rest/app-chat/conversations/.../load-responses!...",
  "tok": "..."
}
```

This suggests a challenge/token flow around conversation loading.

Need full capture of:

- challenge issuance,
- token exchange,
- authenticated chat request,
- response stream.

---

### B. Do not hardcode x-statsig-id style constants

Instead:

- extract them dynamically during healing,
- cache them,
- refresh only on 403 / challenge failure,
- version them by grok.com deploy/build if possible.

If dynamic extraction cannot be made reliable, Grok should be quarantined rather than repeatedly hammering upstream.

---

### C. Fix upload parser immediately

Current parser requires `url`.

Live response contains:

- `fileUri`
- `parsedFileUri`
- `fileMetadataId`

Plan:

- accept `fileUri` and `parsedFileUri` as valid upload URLs,
- normalize to internal attachment object,
- remove false dependency on `url`.

This is a concrete parser fix independent of anti-bot.

---

### D. Fix timeouts/hangs

Current Grok requests hang for 120s.

Policy:

- connect timeout: ~5s,
- first-byte timeout: ~20–30s,
- stream idle timeout: ~60s,
- fail fast on anti-bot 403.

Do not let Grok block the global queue.

---

### E. Healing policy

On 403 anti-bot:

1. stop retrying the same request,
2. trigger Grok challenge refresh,
3. if refresh succeeds, retry,
4. if refresh fails, mark provider `CHALLENGE_REQUIRED`.

No infinite loop.

---

## 6.6 Mistral — solve rate limiting with local traffic control

### Current root cause

Mistral works for basic chat but hits 429 under burst/load.

### Fix plan

#### A. Add provider-level token bucket

Configure conservative defaults:

- max concurrency,
- requests per minute,
- messages per hour,
- cooldown after 429.

#### B. Queue instead of burst

Requests should be queued and dispatched with jitter.

#### C. Honour Retry-After

If present, use it. If not, back off exponentially.

#### D. Defer continuation tests until cooldown passes

Continuation cannot be verified while rate-limited.

#### E. Circuit breaker

If repeated 429s occur:

- mark provider `RATE_LIMITED`,
- pause for cooldown,
- resume with one canary request.

This makes Mistral reliable instead of flaky.

---

## 6.7 Minimax — quota is an account problem, not a parser problem

### Current root cause

Error 2056:

> Token Plan usage limit reached

This is not fixable by code alone.

### Plan

#### A. Keep fail-fast behaviour

Do not spend 8s per call when quota is exhausted.

#### B. Cache quota-exhausted state

For example:

- 15–30 min cooldown,
- re-check with one cheap request.

#### C. Surface remediation clearly

Return a structured error saying:

- quota exhausted,
- claim free credits / upgrade / wait / use another credential.

#### D. Support credential rotation if you provide multiple accounts/keys

If multiple credentials exist:

- try credential A,
- on quota exhaustion switch to credential B,
- mark A cooling down.

But without quota, no code can make Minimax pass.

---

## 6.8 Claude — missing sessionKey is auth, not code

### Plan

- validate required cookie `sessionKey`,
- if missing, mark `AUTH_MISSING`,
- do not attempt model calls,
- trigger browser login heal,
- resume after login.

No amount of parser work fixes missing sessionKey.

---

## 6.9 MiMo — missing userId cookie is auth, not code

### Plan

- validate required cookies:
  - `userId`
  - `xiaomichatbot_serviceToken`
  - `xiaomichatbot_ph`
- if `userId` missing:
  - mark `AUTH_MISSING`,
  - trigger login heal.

---

## 6.10 Qwen / DeepSeek / Meta AI — stabilize and protect

These are mostly working.

Plan:

- keep current adapters,
- add canary tests,
- add capability manifests,
- add rate limiting,
- prevent regressions.

Do not rewrite stable providers unnecessarily.

---

# 7. Feature-level plan

---

## 7.1 Native tool calling

### Rule

Use native tool calling only when there is live evidence.

### Implementation

For each provider:

1. capture a tool-enabled request,
2. identify native tool schema,
3. implement request mapping:
   ```json
   tools -> provider native tools
   tool_choice -> provider native equivalent
   ```
4. implement response mapping:
   ```json
   provider tool call -> OpenAI tool_calls
   ```
5. if no native support exists:
   - document evidence,
   - enable XML fallback,
   - expose fallback mode in capability matrix.

### XML fallback policy

Use XML fallback only when:

- native absence is confirmed by capture/research,
- model still follows structured instructions reliably,
- parser can robustly extract:
  - tool name,
  - arguments JSON.

Do not silently pretend XML is native.

---

## 7.2 Streaming and non-streaming

### Rule

Every provider must support both if the web endpoint supports both.

### Implementation

- unified SSE parser,
- provider-specific event mapping,
- incremental normalization,
- no full-body buffering,
- final usage/finish extraction where available.

### Stream health checks

A stream is not healthy just because HTTP 200 happened.

Mark stream healthy only if:

- at least one meaningful delta arrives,
- final event or clean termination occurs,
- no parser errors occur.

This catches cases like Kimi’s “1 chunk, 0 deltas”.

---

## 7.3 File upload

### Rule

Upload must use provider-native file APIs wherever they exist.

### Implementation per provider

- GLM: `/api/v1/files/`
- Grok: accept `fileUri` / `parsedFileUri`
- ChatGPT: capture native file endpoint
- Kimi: upload + parse/process + attach
- Gemini: capture native media attachment flow

### Common upload object

Normalize to:

```text
attachment_id
mime_type
size
provider_reference
expiry
```

Then pass that into the chat request.

---

## 7.4 Thinking toggle

### Rule

Expose reasoning only when the provider actually returns it.

### Implementation

For each provider:

- capture thinking-enabled response,
- identify reasoning field,
- map to `reasoning_content`,
- if absent, mark capability false or accepted-but-not-surfaced.

### Known cases

- DeepSeek: works,
- GLM: likely via `phase: thinking`,
- ChatGPT: needs evidence,
- Kimi: needs evidence,
- Gemini: needs evidence.

---

## 7.5 Search / deep research toggle

### Rule

Search support and citation support are separate capabilities.

A provider may:

- perform search,
- but not expose citations.

### Capability matrix

For each provider/model:

```text
search_toggle: true/false
research_toggle: true/false
citations_returned: true/false
```

### Implementation

- capture search/research traffic,
- parse source/citation metadata if present,
- if citations are not exposed, do not fabricate them.

---

## 7.6 Session continuation

### Rule

Continuation must be provider-native wherever possible.

### Implementation

Each provider adapter must define:

```text
continuation_mode:
  - native_conversation_id
  - native_message_tree
  - gateway_history_required
  - unsupported
```

### Session object

Store:

- provider,
- model,
- conversation id,
- parent/current message ids,
- last updated timestamp,
- capability flags.

### `session_url` design

The gateway should make `session_url` either:

1. a provider-native URL, or
2. an opaque gateway session locator.

If the provider needs internal ids beyond the visible URL, store them server-side and resolve them from the session URL.

This prevents clients from needing to know provider internals.

---

# 8. Rate limiting, queues, and circuit breakers

This is essential for reliability and speed.

## 8.1 Local rate limiter

For each provider:

- max concurrent requests,
- requests/min,
- messages/hour,
- burst size,
- cooldown after 429.

## 8.2 Priority queue

Prioritize:

1. user live requests,
2. continuation tests,
3. canary checks,
4. research probes.

## 8.3 Circuit breaker

States:

- closed,
- half-open,
- open.

Open on:

- repeated 401/403,
- repeated parse failures,
- repeated 429s,
- repeated protocol drift.

Half-open:

- send one canary,
- if success, close,
- if fail, reopen.

This prevents loops and protects upstream accounts.

---

# 9. Evidence-first research and capture workflow

This is how you avoid guesses.

## 9.1 Evidence hierarchy

1. live captured request/response from your own browser session,
2. repeated live behaviour,
3. open-source reverse-engineered references,
4. documentation/blog hints,
5. assumptions.

Assumptions are never implementation inputs.

## 9.2 One-time capture healer

For each broken provider, run a scoped capture once:

- normal message,
- stream,
- tool,
- upload,
- thinking,
- search/research,
- continuation.

Then compile that capture into:

- fixtures,
- parser expectations,
- manifest entries,
- docs.

After that, runtime uses cached knowledge only.

## 9.3 Save research into docs

Create docs such as:

```text
docs/wire/glm-v2.md
docs/wire/chatgpt-backend-api.md
docs/wire/kimi-api.md
docs/wire/gemini-internal.md
docs/wire/grok-rest.md
docs/wire/mistral-lechat.md
docs/research/provider_rate_limits.md
docs/research/tool_calling_matrix.md
```

This prevents repeated research later.

---

# 10. Target capability matrix

This is the matrix you want to reach.

| Provider | Basic chat | Streaming | Upload | Thinking | Search/Research | Tools | Continuation |
|---|---:|---:|---:|---:|---:|---:|---:|
| DeepSeek | yes | yes | yes | yes | yes/citations | yes | yes |
| Gemini | yes | yes | yes | verify | verify citations | verify | yes |
| ChatGPT | yes | yes | fix | verify | verify citations | fix/verify | fix |
| Kimi | fix | fix | fix | verify | verify citations | verify | fix |
| GLM | fix direct | fix direct | fix direct | likely yes | verify | verify | fix |
| Qwen | yes | yes | yes | verify | verify | yes | yes |
| Mistral | yes | verify | verify | verify | verify | verify | fix after rate limit |
| Grok | fix anti-bot | fix | fix parser | verify | verify | verify | verify |
| Minimax | quota-blocked | quota-blocked | quota-blocked | quota-blocked | quota-blocked | quota-blocked | quota-blocked |
| Claude | auth missing | auth missing | auth missing | auth missing | auth missing | auth missing | auth missing |
| MiMo | auth missing | auth missing | auth missing | auth missing | auth missing | auth missing | auth missing |
| Meta AI | yes | yes | yes | verify | verify | yes | yes |

The goal is not “mostly yes”. It is **verified yes** for every supported provider/model.

---

# 11. Implementation phases

---

## Phase 0 — Stop the bleeding

Goals:

- no global startup failure,
- no provider blocking others,
- no retry storms,
- no fake success.

Tasks:

1. add provider health states,
2. add per-provider session validation,
3. add circuit breaker,
4. add rate limiter,
5. add error classifier,
6. add raw response snapshot on parse failure.

Exit criteria:

- one broken provider cannot kill the gateway,
- broken providers fail fast and cleanly,
- no infinite loops.

---

## Phase 1 — Fix GLM direct path

Why first:

- you already have strong evidence it can work,
- it gives a fast win,
- it removes a bad UI fallback.

Tasks:

1. capture GLM full POST body,
2. implement direct request builder,
3. implement SSE parser with `phase: thinking`,
4. implement `/api/v1/files/` upload,
5. implement continuation,
6. remove UI fallback from normal path.

Exit criteria:

- GLM models return non-empty content,
- streaming works,
- upload works where supported,
- no textarea timeout,
- no UI automation in normal path.

---

## Phase 2 — Fix ChatGPT continuation and core conversation flow

Tasks:

1. capture real `POST /backend-api/conversation`,
2. implement session object,
3. implement continuation identifiers,
4. verify codeword recall without full history resend,
5. capture upload flow,
6. capture thinking/search behaviour.

Exit criteria:

- ChatGPT continuation works,
- streaming is stable,
- upload works,
- thinking/search capabilities are verified honestly.

Then:

- investigate native tool calling,
- if impossible, document and use fallback policy.

---

## Phase 3 — Fix Kimi K3

Tasks:

1. capture `/api/chat` full flow,
2. fix stream parser,
3. fix upload + parse polling,
4. fix continuation,
5. verify tool/search/thinking.

Exit criteria:

- K3 returns real streaming content,
- upload no longer returns empty,
- continuation works,
- capabilities are verified.

---

## Phase 4 — Fix Gemini

Tasks:

1. capture internal generate endpoint,
2. implement stream parser,
3. implement upload,
4. implement search/research parsing,
5. verify continuation.

Exit criteria:

- Gemini basic/stream/upload/continuation work,
- search citation behaviour is verified.

---

## Phase 5 — Fix Grok

Tasks:

1. capture full Grok challenge/token/chat flow,
2. build challenge vault,
3. remove hardcoded constants,
4. fix upload parser to accept `fileUri` / `parsedFileUri`,
5. verify basic chat/stream/upload.

Exit criteria:

- Grok no longer depends on expired constants,
- Grok heals on challenge drift,
- upload parsing works.

---

## Phase 6 — Fix auth-gated providers

Providers:

- Claude,
- MiMo,
- Meta AI if needed,
- any other login-dependent provider.

Tasks:

1. define required cookie sets,
2. validate them independently,
3. implement login heal,
4. retest after valid login.

Exit criteria:

- missing auth is detected cleanly,
- valid auth enables full provider tests.

---

## Phase 7 — Full matrix hardening

Tasks:

- run provider-by-provider feature matrix,
- add canary tests,
- add docs,
- remove warnings,
- optimize performance.

Exit criteria:

- every supported provider/model passes live tests,
- no warnings,
- no manual steps in runtime path.

---

# 12. Definition of Done

The work is done only when all of these are true:

## 12.1 Providers

Every target provider is either:

- fully working,
- or blocked only by external account/quota/auth with clear status.

## 12.2 Features

For each supported provider/model:

- native chat works,
- streaming works,
- non-streaming works,
- upload works where supported,
- thinking works where supported,
- search/research works where supported,
- tool calling works natively where supported,
- XML fallback exists only where native absence is proven,
- continuation works.

## 12.3 No manual runtime labour

No model call requires:

- manual script execution,
- manual capture,
- manual extraction,
- manual cookie fixing,
- manual UI interaction.

Healing may require a one-time browser-based refresh, but not per-call labour.

## 12.4 Reliability

- circuit breakers prevent loops,
- rate limits prevent bans,
- protocol drift triggers controlled healing,
- parse failures produce snapshots,
- broken providers fail cleanly.

## 12.5 Build quality

- release build succeeds,
- no compiler warnings,
- no clippy warnings,
- tests pass,
- live matrix passes.

---

# 13. What must be captured next

These are the highest-value missing evidence items.

## 13.1 GLM

Need:

- full request body for `/api/v2/chat/completions`
- final SSE event shape
- continuation request shape
- tool/search behaviour if available

## 13.2 ChatGPT

Need:

- full request body for `POST /backend-api/conversation`
- SSE response chunks
- continuation request body
- upload endpoint and attachment reference
- thinking-enabled response
- search-enabled response

## 13.3 Kimi

Need:

- full `/api/chat` request/response
- SSE chunks
- upload endpoint
- parse/process endpoint
- continuation request
- tool/search/thinking behaviour

## 13.4 Gemini

Need:

- actual generate/stream endpoint
- request body/headers
- SSE/protobuf event format
- search toggle request
- upload flow
- continuation identifiers

## 13.5 Grok

Need:

- full challenge/token sequence,
- conversation send endpoint,
- stream format,
- upload flow,
- anti-bot refresh behaviour.

---

# 14. Research workstream to run in parallel

Use targeted research only to accelerate capture interpretation. Do not implement from research alone.

## Useful research targets

### ChatGPT

Search for:

- `backend-api/conversation parent_message_id`
- `ChatGPT web reverse proxy backend-api`
- `ChatGPT SSE message metadata`
- `ChatGPT internal tool call`

### Gemini

Search for:

- `gemini.google.com internal API StreamGenerate`
- `BardFrontendService StreamGenerate`
- `Gemini web reverse engineered API`
- `gemini grounding metadata web`

### Grok

Search for:

- `grok.com rest/app-chat/conversations`
- `grok web anti-bot token`
- `x-statsig-id grok`
- `grok internal chat API`

### Kimi

Search for:

- `kimi.moonshot.cn /api/chat`
- `Kimi web API SSE`
- `Kimi file parse API`

### GLM

Search for:

- `chat.z.ai /api/v2/chat/completions`
- `GLM web API SSE`
- `chatglm web internal API`

### Mistral

Search for:

- `Le Chat internal API`
- `Mistral Le Chat rate limit`
- `Le Chat SSE conversation`

Again: research is only useful if it leads to captured proof.

---

# 15. Fast-path recommendations

If you want the fastest practical route, do this order:

1. **Phase 0: health states + rate limiter + circuit breaker**
   - prevents chaos.

2. **GLM direct-only**
   - biggest immediate win from existing evidence.

3. **ChatGPT continuation**
   - likely unblocks a large part of ChatGPT reliability.

4. **Kimi K3 upload/stream parser**
   - fixes one of the most degraded providers.

5. **Gemini capture + parser**
   - likely high value once actual wire format is captured.

6. **Grok challenge vault**
   - do after the more tractable providers are stable.

7. **Auth-gated providers**
   - Claude/MiMo once logins are present.

---

# 16. The most important design decisions

These are the decisions that make the plan strong:

## 16.1 No per-call extraction

Everything learned from captures becomes cached manifest/session state.

## 16.2 No hardcoded anti-bot constants

Use dynamic vault + healing.

## 16.3 No UI automation as primary transport

Direct API only wherever possible.

## 16.4 No fake capabilities

If native tools/search/reasoning/citations do not exist on the wire, mark them unsupported and use fallback only when allowed.

## 16.5 No global failure coupling

Each provider is isolated.

## 16.6 No burst traffic

Local rate limiting is mandatory.

## 16.7 No guess-driven implementation

Every parser change must be backed by captured evidence.

---

# 17. Final recommended plan in one paragraph

Build a provider protocol registry and session vault, then isolate each provider with health states, rate limiting, and circuit breakers. Fix GLM first by moving to the direct `/api/v2/chat/completions` flow proven in your capture and removing UI fallback. Then capture and implement ChatGPT’s real `backend-api/conversation` flow with proper continuation identifiers. Fix Kimi by capturing the true `/api/chat` stream/upload/parse flow and implementing correct SSE parsing plus async file parse polling. Fix Gemini by capturing the actual internal generate/stream endpoint and parsing search/grounding correctly. Fix Grok by replacing hardcoded anti-bot constants with a cached challenge vault and accepting `fileUri`/`parsedFileUri` in uploads. Add self-healing only for classified failures, with bounded retries and human-assisted reauth when required. The result is a gateway where runtime model calls use cached native protocols, no manual extraction, no per-call scripts, and no fragile UI automation.