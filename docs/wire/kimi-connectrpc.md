# Kimi ConnectRPC wire protocol (kimi.ai)

Evidence: `captures/kimi_chat_wire.txt` (live capture, 2026-08).

## Endpoint

```
POST https://www.kimi.ai/apiv2/kimi.gateway.chat.v1.ChatService/Chat
content-type: application/connect+json
connect-protocol-version: 1
authorization: Bearer <JWT aud=kimi.ai>
x-msh-device-id / x-msh-session-id / x-msh-platform=web / x-msh-version=2.0.0
x-language: en-US
r-timezone: <local>
x-traffic-id: <JWT sub>
```

## Request body (JSON, single ConnectRPC frame)

```json
{
  "scenario": "SCENARIO_K2D5",
  "tools": [{"type": "TOOL_TYPE_SEARCH", "search": {}}],
  "message": {
    "role": "user",
    "blocks": [{"message_id": "", "text": {"content": "<prompt>"}}],
    "scenario": "SCENARIO_K2D5",
    "is_goal": false
  },
  "options": {
    "thinking": true,
    "enable_plugin": true,
    "reasoning_effort": "REASONING_EFFORT_LOW"
  },
  "project_id": ""
}
```

Built-in search is engaged ONLY via `TOOL_TYPE_SEARCH`; thinking via
`options.thinking`. Continuation fields are not present in the captured
(new-chat) request; until a multi-turn capture exists, continuation turns
use the legacy kimi.moonshot.cn path.

## Response framing

ConnectRPC streaming: each frame is `[flags:u8][len:u32 BE][json]`.
Flags byte is `0x00` for data frames. The capture renders the high zero
bytes of the length prefix as stray leading characters (`T{...}` = len 84).

## Event masks

| mask | meaning |
|---|---|
| (none) `{"heartbeat":{}}` | keepalive |
| `chat`, `chat.lastRequest` | conversation id (`chat.id`) |
| `message` role=user/assistant/system | message tree nodes; assistant `id` = continuation handle |
| `block.multiStage`, `block.stage` | STAGE_NAME_THINKING lifecycle |
| `block.think.content` (op append) | reasoning delta in `block.think.content` |
| `block.text.content` (op append) | answer delta in `block.text.content` |
| `done` / status finished | terminal |

## Implementation

- Codec + event classifier + request builder:
  `crates/obscura-gateway/src/providers/kimi/connectrpc.rs`
- Transport gate: env `OBSCURA_KIMI_CONNECT_RPC=1`, NEW chats only, with
  automatic fallback to the legacy SSE path on any failure.
- Tool calling rides the gateway-wide MTP/1 dialect on top of the text
  deltas; built-in search is never triggered by client tool names.
- Auth refresh: `POST https://auth.kimi.com/api/account.gateway.v1.AuthService/RefreshToken`
  with `{"refreshToken": ...}` -> `{data:{accessToken, refreshToken}}`
  (see docs/KIMI_K3_NATIVE_RESPONSE.md and providers/kimi/auth.rs).
