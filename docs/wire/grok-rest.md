# Grok web REST protocol (`wire/grok-rest.md`)

State of the grok.com adapter as of 2026-08-20. Evidence base: OmniRoute
GrokWebExecutor capture (2026-08-16) + live verification in the gateway.

## Transport

- Base URL: `https://grok.com`
- Endpoint: `POST /rest/app-chat/conversations/new`
- Client: `StealthHttpClient` (wreq Chrome-145 TLS emulation on Windows),
  with `Timeouts::streaming()` (connect 5 s, request 300 s) so long SSE
  turns are not cut. Grok sits behind Cloudflare Enterprise: the native
  rustls/reqwest ClientHello is rejected with anti-bot 403 even with valid
  `sso` + `sso-rw` cookies, so the stealth client is mandatory.
- Cookie jar is **filtered** to `sso` + `sso-rw` only. The profile's
  `cf_clearance` is pinned to the Firefox TLS fingerprint that earned it;
  replaying it under Chrome TLS is itself an anti-bot trigger.

## Auth requirements

- `sso` and `sso-rw` cookies on `grok.com` (both required). Missing cookies
  surface as `auth_missing` via the pre-flight probe
  (`providers/authcheck.rs`): validate at `DirectClient::new` time.
- No `Authorization` header; session is cookie-based.

## Headers (per-request)

Built fresh in `build_request_headers` (`direct.rs`):

- `x-statsig-id`: base64 of a synthetic browser TypeError string
  (`browser_statsig_id()` in `statsig.rs`). Randomized per request; two
  shapes: `TypeError: Cannot read properties of null (reading 'children["xxxxx"]')`
  and `TypeError: Cannot read properties of undefined (reading 'xxxxxxxxxx')`.
  This matches what the frontend error handler actually sends today.
  The retired 70-byte challenge blob format now triggers anti-bot 403.
- `x-xai-request-id`: fresh UUID per request.
- `traceparent`: fresh trace/span ids per request.
- `Baggage`: pinned sentry release tag
  `d6add6fb0460641fd482d767a335ef72b9b6abb8` (rotate with grok deploys).
- Browser headers: `Sec-Ch-Ua` (Chrome 145), `Sec-Fetch-*`, `Referer
  https://grok.com/`, UA `Chrome/145.0.0.0`.

## Anti-bot handling (403)

`check_and_retry` (direct.rs):

1. On `403` the response body is drained (up to 2 KB) into the log.
2. Retry **once** with a fresh `x-statsig-id` (per-request marker, so the
   retry naturally carries a new one).
3. If the retry also fails: `GatewayError::Provider` with the body and the
   hint that the session may be expired; the classifier maps grok 403 to
   `ErrorClass::Challenge`, opening the circuit for 20 minutes (escalating)
   so a single anti-bot trip cannot hammer the endpoint.

No browser navigation or constant re-extraction is involved.

## Conversation flow

- `conversation_id` is client-generated (`new_uuid()`), sent in the payload.
- Responses come back as NDJSON/SSE over `send_single_streaming`; parsed
  line-by-line by the stream handler (`parse_ndjson_line` /
  `GrokStreamResponse` / `GrokStreamEvent` with tool-card parsing for
  native intents).
- `session_url` is `https://grok.com/chat/<conversation_id>` and maps to
  per-conversation state in `GrokSessionStore`, persisted to
  `<data_dir>/grok.json` (atomic write via `*.tmp` rename).
- `parent_response_id` threading is tracked in the store for continuation.

## Uploads

`providers/grok/upload.rs` — POST files to the grok upload endpoint and
cache the result by content hash (`UploadCache`, TTL-backed). The upload
response is parsed for its URI in this priority (matching live captures):

1. `fileUri`
2. `parsedFileUri`
3. `url`

`data:` URIs are decoded to bytes and uploaded natively (never pasted as
text). Attached chat files use the returned URI in the payload.

## Capability notes (honest wire status)

- Thinking: surfaced via reasoning phases in stream events.
- Tools: native-intent tool cards are parsed (`tool_for_native_intent` +
  `parse_native_tool_card` / `parse_xml_tool_card`), plus `handle_tool_results`
  injects tool results with `tool_call_id` binding.
- Upload: native (above).
- Model IDs: grok-auto / grok-fast / grok-expert / grok-heavy / grok-4.5 /
  grok-4.3 map to wire mode ids in `mode_id_for_wire` (`direct.rs:30`).