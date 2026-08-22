# Kimi K3 native response contract

Research captured 2026-08-15. Re-verified against
`crates/obscura-gateway/src/providers/kimi/rpc.rs` on 2026-08-20: the event
envelope types below (`delta`, `thinking`, `tool_call`, `complete_message`,
`message_finished`) and the nested `tool_calls` search are all still what the
parser implements; auth flow (navigate + refresh via the auth.kimi.com
Connect endpoint) is unchanged in `kimi/auth.rs`.

Moonshot's official Kimi K3 repository documents that K3 always reasons and
returns `reasoning_content`. It also documents native `tool_calls` and says
the complete assistant message, including `reasoning_content` and
`tool_calls`, must be preserved for later turns and tool-result requests:

https://github.com/MoonshotAI/Kimi-K3

The gateway's web K3 path uses the Mavis session SSE endpoint rather than the
public OpenAI-compatible API. Its event envelope has appeared as `delta`,
`thinking`, `tool_call`, `complete_message`, and `message_finished`. The
parser therefore treats the envelope as transport-only and searches nested
`tool_calls` / `tool_call` fields before falling back to the XML compatibility
path. This is intentionally limited to native fields actually present in the
SSE event; it does not synthesize a tool call from ordinary text.

The public K3 contract is evidence that native tool calls and preserved
thinking are supported by the model, but it is not evidence that every Mavis
web wrapper uses the same JSON envelope. A live Whelmer capture is still
required to verify the current web event shape and to finish the K3 upload,
search, and reasoning checks.

## Authentication finding

The imported Firefox profile contained a `www.kimi.com` `kimi-auth` JWT whose
`exp` was in the past, while the user was still logged in and could chat in
the website. This is expected for a browser-managed session: the imported
cookie is a snapshot, and the page can refresh an HttpOnly cookie after
navigation. The gateway now rejects the stale snapshot, navigates its
isolated session to Kimi, then checks that session's cookie jar before
attempting JavaScript
localStorage extraction. It no longer sends a `refresh_token` value as a
Bearer access token. The current Kimi web request bundle was also inspected
on 2026-08-15: it reads separate `access_token` and `refresh_token` values and
refreshes through the Connect endpoint
`https://auth.kimi.com/api/account.gateway.v1.AuthService/RefreshToken` with
the JSON field `refreshToken`. The gateway now performs that same refresh in
the isolated Kimi page when the imported access token is expired, stores the
rotated pair, and uses only the returned access token for chat.

## Authentication correction, 2026-08-16

The imported `kimi-auth` cookie must not be treated as authoritative merely
because its JWT `exp` is still in the future. A live request can reject that
profile snapshot with `auth.token.invalid` while the logged-in Kimi page can
still rotate credentials. The refresh path now runs after navigation before
accepting the imported cookie. This follows the separate access-token and
refresh-token model in the Kimi CLI OAuth implementation:
https://github.com/moonshotai/kimi-cli/blob/main/src/kimi_cli/auth/oauth.py
