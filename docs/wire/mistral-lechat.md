# Mistral Le Chat internal API

Evidence: `providers/mistral/direct.rs` (stream parser unit-tested),
`docs/MISTRAL_PROTOCOL.md`, live rate-limit behaviour.

## Endpoints

| Call | Purpose |
|---|---|
| `POST /api/chat` with `mode: "start"` | first turn; the gateway first creates the chat server-side via the tRPC `message.newChat` mutation and persists the returned `chatId` so the turn survives stream failures |
| `POST /api/chat` with `mode: "append"` + `chatId` | continuation: only the new user message is sent; thread context is server-side |

Auth: `mistral-chat-session` cookie (+ `anonymous-id`); validated by
`authcheck.rs` before any call.

## Stream format

NOT standard SSE. Lines are `<code>:<json>` (optionally a
`__context__:<json>` suffix), decoded incrementally:

| code | meaning |
|---|---|
| 0/2 | direct text token (`text`) — appended to the shared accumulator |
| 4 | moderation |
| 5 | full tool_call object |
| 6 | error (`message` / `detail`) |
| 8 | done |
| 12 | reasoning part |
| 15 | `state_update`: fast-json-patch over per-message state, or `bootstrap` carrying chat/message ids |

Because code-15 patches re-emit FULL message content, deltas are computed by
prefix comparison against the accumulated text (`last_content`,
`last_reasoning`) — never forwarded verbatim.

## Continuation state

Per session: `{chat_id, message_id, message_version, model, features,
anonymous_identifier}` in the mistral session store. After a 429 cooldown
the next append resumes the same thread.

## Rate limiting

Le Chat throttles aggressively under burst. Gateway config: 10 rpm,
concurrency 2, burst 2, 120 s cooldown after a classified 429
(`rate_config_for("mistral")`), plus shared Retry-After handling.
