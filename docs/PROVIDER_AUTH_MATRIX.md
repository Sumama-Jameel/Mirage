# Provider Auth + Native Feature Matrix

Living research document. Updated as providers are audited. Every claim below is
verified against the actual source in `crates/obscura-gateway/src/providers/`,
not from memory.

## Policy (from GOAL.md)

- Every provider MUST use native auth (imported cookies / localStorage tokens).
  No anonymous/guest/temp-user mode. No provider may run without the user's real
  logged-in session.
- Missing native credentials MUST fail closed with `GatewayError::Auth`
  (HTTP 401), with a message telling the user how to log in and re-import.
  `GatewayError::Provider` (HTTP 502) is reserved for upstream API failures.
- No hardcoded fixes. Every feature must be the real background API
  (reverse-engineered internal endpoint), not chat automation, not hacks.
- Session continuation/resuming (`session_url`) is required per provider.

## Auth fail-closed audit (error type when credentials are missing)

| Provider    | Auth source(s)                          | Missing-credential error type | Verified |
|-------------|-----------------------------------------|-------------------------------|----------|
| chatgpt     | session cookies + requirements token    | `Auth` (401)                  | yes      |
| claude      | claude.ai cookies (org uuid + token)    | `Auth` (401)                  | yes      |
| deepseek    | localStorage `userToken` + cookies      | `Auth` (401)                  | yes      |
| gemini      | session cookies + CSRF                  | `Auth` (401)                  | yes      |
| glm         | localStorage `token` / `token` cookie   | `Auth` (401)                  | yes      |
| grok        | grok.com/x.ai cookies (`sso`)           | `Auth` (401)                  | yes      |
| kimi        | kimi.moonshot.cn bearer                 | `Auth` (401)                  | yes      |
| metaai      | meta.ai cookies + ecto1 token           | `Auth` (401)                  | yes      |
| minimax     | localStorage `_token` JWT / env         | `Auth` (401)                  | yes      |
| mimo        | xiaomimimo.com cookies (`serviceToken`, `userId`, `xiaomichatbot_ph`) | `Auth` (401) | yes      |
| mistral     | chat.mistral.ai / mistral.ai cookies (`mistral-chat-session`/`anonymous-id`) | `Auth` (401) | yes |
| qwen        | localStorage `token` JWT                | `Auth` (401)                  | yes      |

Status: all 12 providers fail closed with 401 on missing credentials. None has
an anonymous/guest path.

The exact probe tables (cookie names, domains, localStorage origins, and the
`MINIMAX_JWT` env fallback) are codified in
`providers/authcheck.rs` (`AuthProbe::probes`) and are enforced at request
time by the pre-flight check in `api/mod.rs::chat_completions` (step 3 of the
guard pipeline; see `docs/InitialPlan-infra.md`). Providers without a probe
are not gated.

## Native feature matrix

Legend: Y = native in the background API, N = not supported (fails closed in
`validate_request`), - = not applicable / unimplemented.

| Provider  | Native tool calling | Streaming | File upload | Thinking toggle | Research/search toggle | Session continuation (`session_url`) |
|-----------|---------------------|-----------|-------------|-----------------|------------------------|--------------------------------------|
| chatgpt   | Y                   | Y         | Y           | Y               | Y                     | Y                                    |
| claude    | Y                   | Y         | Y           | Y               | Y                     | Y                                    |
| deepseek  | Y (XML fallback only if root API lacks it) | Y | Y | Y (reasoner/expert) | N (no native channel) | Y                                    |
| gemini    | Y                   | Y         | Y           | Y               | Y                     | Y                                    |
| glm       | Y                   | Y         | Y           | Y               | Y                     | Y                                    |
| grok      | Y                   | Y         | Y           | -               | -                     | Y                                    |
| kimi      | Y                   | Y         | Y           | Y               | Y                     | Y                                    |
| metaai    | Y (in-stream XML tool-call stripping) | Y | N (fail closed) | Y | Y | Y                                |
| minimax   | Y                   | Y         | Y (platform API key) | Y        | Y                     | Y                                    |
| mimo      | Y (XML fallback; web endpoint has no native function channel) | Y | Y | Y | Y (webSearchStatus) | Y   |
| mistral   | Y (native `delta.tool_calls` in Le Chat SSE) | Y | Y | - | - | Y |
| qwen      | Y                   | Y         | Y           | -               | -                     | Y                                    |

Notes:

- Meta AI `supports_attachments()` returns false; `validate_request` fails
  closed when the request carries image/file content. Tool calls are produced
  by stripping XML from the stream (its real DGW transport carries tools via
  XML-in-prompt; verified in `metaai/direct.rs`).
- MiMo uses the XML `<tool_call>` fallback for function tools: the web chat
  endpoint's payload only carries `enableThinking`, `webSearchStatus`, `query`
  and `multiMedias` — no native function-calling channel (verified live, proof
  documented in `mimo/mod.rs::validate_request`). Per GOAL.md the function
  definitions are injected into the prompt and `<tool_call>` markers parsed out
  of the reply (same as DeepSeek/Gemini). Verified live 2026-08-05 for
  non-streaming, streaming, and tool-result round-trips.
- DeepSeek search: the request model supports thinking for `deepseek-reasoner`
  and `deepseek-expert`; no native search toggle exists on the web endpoint.
- Minimax file upload requires the separate `MINIMAX_API_KEY` platform key
  (a web-app JWT always returns error code 1004) — documented in
  `minimax/upload.rs::resolve_upload_token`.

## To-do / watch items

- [ ] Confirm chatgpt session continuation format against live session.
- [ ] Grok: confirm research/deep-research toggle exists in current REST API
      (needs live capture; do not guess).
- [ ] Qwen: confirm thinking toggle + search native fields (needs live capture).
- [x] Mistral web UI provider now ships; it runs direct (`chat.mistral.ai`
      Le Chat protocol), uses native cookies + session continuation, and is
      rate-limited conservatively (rpm 10, concurrency 2, 120 s 429 cooldown)
      because the upstream throttles burst traffic.

## Re-verification procedure

When any provider's upstream web app rotates endpoints/fields:

1. Capture the real request/response with Obscura browser or DevTools.
2. Update the provider code to match the actual API (no hardcoded guess).
3. Update this matrix if the feature set changed.
4. Run `cargo build --release` and `cargo nextest run -p obscura-gateway`.
