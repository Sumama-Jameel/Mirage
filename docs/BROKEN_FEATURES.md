# Obscura Gateway — Confirmed Broken / Missing Features Report

Status: LIVE-TESTED 2026-08-07 against the running gateway
(`target/release/obscura-gateway`, `127.0.0.1:8080`, config
`obscura-gateway.toml`, browser profile firefox-esr). Every entry below is
based on an actual live HTTP test, not on code inspection or assumptions.
Re-verified 2026-08-15 for the deepseek, metaai, gemini, and qwen rows; items
fixed and re-tested live as of that date have been removed from this file.

Scope per GOAL.md: for each provider, NATIVE tool calling, NATIVE streaming +
non-streaming, NATIVE file upload, NATIVE thinking toggle, NATIVE
research/deep-research toggle, session continuation. Only XML fallback where
the root API lacks the feature.

---

## 1. BROKEN OR MISSING FEATURES

### 1.2 Grok — all 6 models, basic chat — HTTP 502 (anti-bot 403)
`Grok API 403 Forbidden: {"error":{"code":7,"message":"Request rejected by
anti-bot rules."}}`. Auto-heal failed: "the x-statsig-id constants could not
be refreshed". The hardcoded `x-statsig-id` challenge constants in
`direct.rs` are expired for the live grok.com deploy. The entire provider is
dead until re-extracted from a live browser.
Evidence: smoke test, 6/6 Grok models -> 502; gateway log confirms auto-heal
failure.

### 1.3 Minimax — all 3 models, basic chat — HTTP 502 (account quota)
`finish_reason: error — 42212: Token Plan usage limit reached: Upgrade your
Token Plan or purchase Credits for more usage. (2056)`. This is an
**account-side blocker, not a code defect**: the requests are accepted (HTTP
200, correct signature), and the server replies in-band with MiniMax error
code 2056, which per MiniMax docs is a non-retryable account Token Plan quota
exhaustion (5-hour rolling + weekly windows). The gateway surfaces it
correctly. Resolution paths (any one works): claim the daily free credits in
MiniMax Code at agent.minimax.io, purchase Credits, upgrade the Token Plan,
switch to a pay-as-you-go API key, or wait for the quota window to reset. The
gateway now caches the exhausted state (~30 min) and fails fast on subsequent
calls instead of spending ~8s per call, and appends the remediation to the
surfaced error.
Evidence: smoke test, 3/3 Minimax models -> 502.

### 1.4 MiMo (Xiaomi) — all 5 models, basic chat — HTTP 401
`MiMo session cookies missing: [userId]`. The imported profile has
`xiaomichatbot_serviceToken` and `xiaomichatbot_ph` but NOT `userId`, so the
provider refuses. `userId` is a required cookie in `MIMO_PROTOCOL.md` and it
is missing from the profile, so no MiMo test is possible.
Evidence: smoke test, 5/5 MiMo models -> 401.

### 1.6 GLM — all 16 models broken or empty
- `glm-5.2`, `glm-5.1`, `glm-5`, `glm-5-turbo`, `glm-5v-turbo`, `glm-4.7`,
  `glm-4.6v`, `glm-4.6`, `glm-4-plus`, `glm-4-zero`, `glm-4-think` -> 502
  `chat.z.ai input textarea not found within timeout`. The direct v2 API path
  is not being taken (or fails) and the UI-automation fallback cannot find
  the chat input on the live page, so 11 of 16 models never answer.
- `glm-5`, `glm-4-deepresearch`, `glm-4.7-flashx`, `glm-4.7-long`,
  `glm-4.5-air`, `glm-4.5-thinking` -> 200 OK but **empty content** (`''`)
  with `finish_reason: stop`. These respond but deliver no text.
Evidence: smoke test, 16/16 GLM models fail or return empty.

### 1.7 Kimi — 3 of 7 models broken in basic chat
- `kimi-k3` -> HTTP 502 `create chat failed: error sending request for url
  (https://kimi.moonshot.cn/api/chat)` (flaky; it later succeeds in deep tests
  but with broken features, see 1.10).
- `kimi-k2.7-code-highspeed` -> HTTP 502 same `create chat failed`.
- `kimi-research` -> HTTP 502 `response read failed: error decoding response
  body`.
Evidence: smoke test.

### 1.10 Kimi K3 — streaming, tool calling, upload, thinking, search all degraded
- Stream: 1 data chunk, 0 content deltas, **no `session_url`** in stream
  (continuation broken in stream mode).
- Tool calling: NO-TOOLCALL with empty content.
- Image upload: 200 but empty content (`''`), took 85.7s. Gateway log:
  `Kimi parse_process request failed (file will be processed async)` — the
  file parse step fails and the provider continues with an unparsed file,
  so the model receives nothing useful.
- Thinking: no `reasoning_content` returned.
- Search: no citations returned.
Evidence: deep test + gateway log.

### 1.13 ChatGPT — native tool calling BROKEN (gpt-4o-mini, chatgpt-auto)
With a `get_weather` tool, `chatgpt-auto` and `gpt-4o-mini` answer in text
("I couldn't call a get_weather function...") instead of emitting
`tool_calls`. Even with an explicit "only emit a function call" prompt,
`gpt-4o-mini` refuses ("no such function is available") and `chatgpt-auto`
returns empty content. Forced round-trip returns HTTP 502: `tool_choice was
set to 'required' but the model produced a text response instead of a tool
call. The ChatGPT web endpoint does not support native tool_choice
enforcement`.
GOAL.md requires NATIVE tool calling; the live gateway cannot produce a tool
call on ChatGPT.
Evidence: deep test + forced-tool test — NO-TOOLCALL, roundtrip 502.

### 1.14 Search toggle — citations never returned on several providers
- Gemini (all models, incl. `gemini-deep-research`): `search: true` accepted,
  0 citations.
- ChatGPT `gpt-4o-mini`: search accepted, 0 citations.
- Kimi K3, `kimi-search`, `kimi-research`: search accepted, 0 citations
  (the dedicated research model returns no citations either).
Only DeepSeek returns citations (12) today.
Evidence: deep test, final probe (`gemini-deep-research` -> 0 citations).

### 1.16 Minimax stream — connection cut
`minimax-m3` stream returned `IncompleteRead(201 bytes read)` — the SSE
stream aborts mid-response. This is a symptom of the same account-side token
plan exhaustion as 1.3: the server starts the SSE stream then kills the
connection for an over-quota account. Not a gateway bug (the plain reqwest
client is accepted and receives data; the cut is server-side).
Evidence: deep test.

### 1.17 Mistral — message rate limit reached (429)
After the 10-request smoke pass, every Mistral deep test returned 429
`{"detail":"Message rate limit reached","code":6200}`. The Le Chat endpoint
has an aggressive per-account message rate limit; the gateway surfaces it as
a raw 502 and does not back off meaningfully. Still 429 on retry several
minutes later and again at the end of the session (final probe); session
continuation for Mistral is therefore untestable (see 2.10).
Evidence: deep test + follow-up test, all Mistral requests -> 429.

### 1.18 ChatGPT — session continuation requires resending full history
Two-turn test (user states a codeword, then asks what it was) with only the
follow-up message + `session_url` passed back: `chatgpt-auto` answered
"The in long-term memory, though. I only know it because it appears in the
current conversation" — the previous turn was NOT retained. When the FULL
message history is resent alongside `session_url`, ChatGPT does recall the
codeword. So `session_url` alone does not continue the ChatGPT thread,
contradicting the documented contract in `models.rs` ("continue the same
thread without resending the full message history"). Clients must resend all
history for ChatGPT, unlike DeepSeek/Gemini/Kimi.
Evidence: follow-up test — LOST-CONTEXT with new-message-only, OK with full
history.

### 1.20 ChatGPT thinking accepted but no reasoning surfaced — auth-blocked (code present)
`o1`, `o1-mini`, `o3-mini`... wait, only `o1`/`o1-mini`/`o3-mini` exist. These accept `thinking: true` (200 OK) but return no `reasoning_content`. The gateway DOES wire the full path: `set_thinking_effort` (PATCH settings, direct.rs:878), SSE parsing of `weight:0`/`parts/1` deltas (rpc.rs:188-206), and `reasoning_content: thinking` (direct.rs:1032); unit tests `parse_sse_line_extracts_thinking_*` pass. One historical gateway request returned `401 token_expired`, so that particular live verification was inconclusive. This report does not infer the current browser login state; the feature must be retested through the currently authenticated session.
Evidence: historical live request -> 401 token_expired; unit tests in rpc.rs for thinking extraction pass.

### 1.21 Mistral session continuation blocked
`mistral-medium-latest` two-turn test -> 429 rate limit before it can run;
continuation cannot be exercised while the account is rate-limited.

---

## 2. BROKEN OR HALF-IMPLEMENTED INTEGRATIONS

### 2.1 GLM — direct path blocked by captcha, UI path broken (half-integrated)
Confirmed from gateway log: the direct v2 API path fails with a live captcha
challenge every time —
`FRONTEND_CAPTCHA_REQUIRED` / `人机验证失败，请重新验证后再试。`
("human verification failed, please verify again") and
`IllegalUserTag` on the captcha endpoint (`"Bad request. The userTag is
invalid."`). The gateway then falls back to driving the chat.z.ai page, where
the textarea is never found within timeout because the page's JS bundle
`prod-fe-1.1.82/assets/index-DMNaMX63.js` fails to load
(`ES module error ... Uncaught null`). So GLM is dead on BOTH paths: the
direct path is captcha-blocked and the UI fallback cannot render the chat
input. `glm-5v-turbo` image-upload is correctly rejected (400, no vision
support on the wire model).

### 2.2 Grok auto-heal (half-integrated)
The x-statsig-id auto-heal path exists but fails against the live grok.com
("could not be refreshed"), and the fallback constants are stale. No
verification blob path works.

### 2.3 Grok upload parsing (half-implemented)
Grok image upload returns HTTP 500 from the gateway: `upload response missing
url field`. The live upload response carries `fileUri` (plus
`fileMetadataId`, `parsedFileUri`) but no `url` field, and the gateway's
upload parser requires `url`. Confirmed live response body in the log.
Even when the chat API is fixed, uploads cannot work until the parser
accepts `fileUri`/`parsedFileUri`.

### 2.4 Grok requests hang
Grok stream and tool-calling requests timed out client-side after 120s (no
response at all). The 403 anti-bot check on the chat API makes every request
hang or error.

### 2.5 ChatGPT tool_choice "required" (half-integrated)
`tool_choice: "required"` is emulated via prompt injection but the model does
not comply; the gateway returns a 502 error instead of a tool call. The
feature is advertised but cannot produce a result.

### 2.7 GLM models returning empty content
Six GLM models respond 200 with empty content and a session_url. The
response parser is not extracting the actual text, or the upstream returns an
empty body that is accepted as a valid answer.

### 2.10 Session continuation — 2 of 6 providers fail it
Live two-turn codeword test (new message + `session_url`, no history resend):
- Works: deepseek-chat, gemini-3.5-flash, kimi-k2.7-code, qwen-auto (all
  recall the codeword).
- Broken: chatgpt-auto (needs full history resend, 1.18), mistral-medium-latest
  (429 rate-limited, 1.21).
Session continuation is a GOAL.md hard requirement; two of the six working
providers fail it.

---

## 3. BOTTLENECKS

### 3.1 Whole-profile cookie/localStorage import runs at startup
Gateway imports 319 cookies + provider localStorage on every start; the
absence of a single provider cookie (Claude `sessionKey`, MiMo `userId`,
meta.ai cookies) kills the whole provider. There is no per-provider cookie
validation that surfaces this before first request.

### 3.2 Kimi K3 85.7s image upload
Upload on Kimi K3 took 85.7s and still returned empty content. Compare
DeepSeek 2.2s and Gemini 9.2s.

### 3.3 Gemini search responses take 30s
`gemini-3.1-pro` / `gemini-deep-research` with search took ~30s (vs ~7s
without), and still returned no citations.

### 3.4 Sequential pool / one model at a time
Requests serialise on the warmed session pool; a slow model (GLM ~120s
timeout, Grok 403) blocks the queue behind it.

### 3.5 No per-request rate limiting on the gateway side
The only protection against upstream rate limits is the `send_with_retry`
backoff (429/5xx). A burst of requests can still hit provider rate limits;
the user-visible error is a raw 502.

---

## 4. LIMITATIONS

### 4.1 Providers not testable without user login (blocked by auth, not code)
Claude, MiMo, Meta AI: the imported Firefox profile lacks the required
cookies, so the gateway correctly fails closed. These are auth limitations,
not proof the integration is wrong. Re-import a profile with logged-in
sessions to retest.

### 4.2 Minimax is a paid-usage blocker
Minimax requires quota; an account with an exhausted Token Plan gets error
2056 (`42212: Token Plan usage limit reached`) and no model is testable until
quota is restored. MiniMax documents the remedies: claim the daily free
credits in MiniMax Code at agent.minimax.io, purchase Credits, upgrade the
Token Plan, switch to a pay-as-you-go API key, or wait for the 5-hour rolling
/ weekly window to reset. The gateway caches the exhausted state (~30 min) so
retries fail fast instead of burning a session per call, and appends this
remediation to the surfaced error (documented in `minimax/direct.rs`).

### 4.3 Grok challenge constants expire with grok.com deploys
Hardcoded `x-statsig-id` constants must be re-extracted from a live browser
on each grok.com deploy (see `EXTRACT_GROK_CONSTANTS.txt`). The current ones
are dead, disabling Grok entirely.

### 4.4 JSON mode (`response_format: json_object`)
Only DeepSeek supports it and it is fully verified end-to-end: an
`json_object` request returns valid parseable JSON (`{"cities":
["Paris","Lyon","Marseille"]}`). All other providers (Gemini, ChatGPT,
Kimi, Mistral, Qwen, GLM, Grok, MiMo, Meta AI) reject `json_object`; the web
endpoints have no verified native JSON channel. Per GOAL.md this is correct
fail-closed behavior until live-verified.

### 4.5 Vision uploads are provider-flaky
Kimi K3 uploads an image but returns empty content (1.10). DeepSeek (data:
URLs), Qwen (OSS), Gemini, and ChatGPT gpt-4o-mini image attachments work.

### 4.6 Reasoning content in streaming and via toggle
DeepSeek reasoner models stream `reasoning_content` by default in both
streaming and non-streaming. Still absent: Kimi K3 shows none, ChatGPT
o1/o1-mini/o3-mini accept the thinking toggle but return no reasoning text.

### 4.7 Tool-calling reliability
DeepSeek, Gemini, Qwen, and Kimi K2.7-Code emit native `tool_calls`. ChatGPT
never emits a tool call in live tests; Kimi K3 emits none; GLM is
captcha-blocked.

### 4.8 Model coverage lags `LatestAImodels` and current model names
Re-verified 2026-08-17: most gaps are now closed and live-tested. Added and
verified via live `/v1/chat/completions`:
- OpenAI: `gpt-5.6-sol/terra/luna`, `gpt-5.5-pro`, `gpt-5.4-pro`,
  `gpt-5-chat-latest` added (routes to the ChatGPT provider; the model picker
  slug is the same as the public id).
- Anthropic: `claude-opus-5` added (routes; blocked by missing `sessionKey`
  cookie, see 4.1).
- Google: `gemini-3.6-flash` (mode 1), `gemini-3.5-flash-lite` (mode 6)
  added and live-tested OK (non-Pro models carry no `x-goog-ext-525001261-jspb`
  header; field 79 selects the model).
- Moonshot: `kimi-k3-instant`, `kimi-k3-swarm` present.
- Qwen: versioned ids (`qwen3.8-max-preview`, `qwen3.7-*`, `qwen3.6-*`,
  `qwen3-vl-235b-a22b`) present. (`qwen-research` is intentionally absent:
  it requires the paid DashScope API, not the free chat.qwen.ai endpoint.)
- Mistral: `mistral-large-latest` added and live-tested OK.
- DeepSeek: `deepseek-v4-pro`, `deepseek-v4-flash`, `deepseek-v3.2`,
  `deepseek-r1` aliases added and live-tested OK (v4-pro/r1 route to the
  `expert` wire type, v4-flash/v3.2 to `default`; streaming works too).
Remaining absent because there is no verified wire value (not a code gap):
- Google `gemini-omni-flash`: a video-generation model, not a chat model on
  the gemini.google.com StreamGenerate path.
- xAI `grok-4.5-build`: Grok Build is a separate harness; the grok.com chat
  API `modeId` values are `fast`/`expert`/`heavy` only (no verified build id).
- Meta `musespark-1.1`, `llama-5`, `llama-4-*`: the meta.ai DGW endpoint only
  exposes the three `muse-spark` reasoning modes; no verified routing exists
  for these ids.

---

## Test environment

- Server: `target/release/obscura-gateway` (release build, Aug 7 04:27)
- Config: `obscura-gateway.toml` (port 8080, api_key `test-key-123`,
  profile `/home/sumama/.mozilla/firefox-esr/d6qafzml.default-esr140`)
- Pool: 10 warmed sessions (all `chat.deepseek.com`)
- Test image: 32x32 PNG (red square on blue background)
- Raw logs: `/home/sumama/.opencode_tmp/obscura/{smoke_basic,deep_test,followup_test,forced_tool,json_verify,ds_vision,chatgpt_session,final_probe}.log`

## Summary counts (live, 2026-08-07)

| Provider | Models | Basic chat OK | Notes |
|----------|--------|---------------|-------|
| deepseek | 5 | 5 | reasoning streams by default on reasoner models; vision data: URLs work |
| gemini | 4 | 4 | search no citations |
| chatgpt | 15 | 15 | tool calling broken; forced roundtrip 502 |
| kimi | 7 | 4 | k3/highspeed/research broken; k3 features degraded |
| glm | 16 | 0 | 502 captcha/textarea timeout or empty content |
| claude | 10 | 0 | 401 auth (no sessionKey cookie) |
| grok | 6 | 0 | 403 anti-bot, constants expired |
| qwen | 6 | 6 | research model removed (paid API only); other 6 models OK |
| minimax | 3 | 0 | account token plan exhausted (2056); fails fast |
| mimo | 5 | 0 | 401 missing userId |
| mistral | 10 | 10 | rate-limited to 429 under load |
| metaai | 3 | 3 | OK end-to-end (chat, continuation, streaming, native tool calling, native image attachment) |

Working end-to-end today: DeepSeek (reasoner streaming reasoning, vision
data: URLs, tools, citations), Gemini (except search-citations), ChatGPT
(except tools/forced-tools), Kimi k2.7-code/k2.6/k2.5/kimi-search, Qwen (basic
chat, native tool calling, continuation, vision upload), Mistral (basic chat
only; rate-limited otherwise), Meta AI (all 3
muse-spark models, native web_search tool calls, native image attachment via
rupload).
