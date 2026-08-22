# GLM free web protocol findings

Research captured 2026-08-16; direct-path implementation verified in
`crates/obscura-gateway/src/providers/glm/{direct,rpc}.rs` (2026-08-20).

The free `chat.z.ai` web endpoint is not the same product as the paid Z.AI
API (which exposes an OpenAI-compatible `/paas/v4/chat/completions` and is
outside this browser-session provider). The web endpoint used by Obscura is
`POST /api/v2/chat/completions`.

## Current direct path (only path)

GLM is **direct-only**. The UI-automation fallback (`glm/ui.rs`,
`humanize.rs`, `captcha.rs`) was deleted in 2026-08-20. Key facts codified
in `rpc.rs` / `direct.rs`:

- **Endpoints**
  - `POST /api/v2/chat/completions?timestamp=<ms>&requestId=<uuid>&user_id=<user_id>&version=0`
  - `POST /api/v1/chats/new` (chat creation: `models`, `enable_thinking`,
    `reasoning_effort`, `auto_web_search`)
  - `POST /api/v1/files/` (upload; response carries `id`, `meta.cdn_url`)
- **Auth**: the `token` cookie (and `Bearer <token>` / `Authorization`
  header) plus `token`/`access_token` in localStorage on the
  `https://chat.z.ai` origin. Skipping expired cookies causes
  `FRONTEND_CAPTCHA_REQUIRED`. Pre-flight probe lives in
  `providers/authcheck.rs`.
- **Anti-bot**: the endpoint is gated by Aliyun NVC captcha for non-browser
  TLS fingerprints. The direct path sends browser-emulated TLS (wreq/stealth
  client) with the exact headers the web client uses (`X-FE-Version`,
  `X-Signature`, Bearer token, cookies), which does not trigger the challenge.
  `FRONTEND_CAPTCHA_REQUIRED` responses must be treated as
  `challenge_required` circuit events, never retried blindly.
- **`X_FE_VERSION = "prod-fe-1.1.84"`** (rpc.rs:15) needs periodic rotation
  when the web client ships a newer build (capture the live value).
- **SSE shape**: `data: {"type":"chat:completion","data":{"delta_content":"...","phase":"thinking"}}`.
  `phase: "thinking"` maps to `reasoning_content`, otherwise content. No
  explicit done event on some models: synthesize `finish_reason: stop`.

## Tool calling

The web endpoint does not accept OpenAI `tools` / assistant-tool roles
directly; GLM tool calls are carried as XML markers in content and parsed
with the shared split-SSE-aware parser in `providers/tool_call.rs` (see
`docs/wire/streaming_tool_calls.md`). Do not send native tool schemas.

## Gotchas re-extracted from code

- Expired cookies are filtered out; sending them triggers
  `FRONTEND_CAPTCHA_REQUIRED`.
- The warm-page path is gone; the only remaining page usage is a diagnostic
  probe that validates the session, and it calls the same direct endpoint.

Sources:

- https://docs.z.ai/api-reference/llm/chat-completion
- https://github.com/izaart95-jpg/GLM-Free-API (device-token flow context)