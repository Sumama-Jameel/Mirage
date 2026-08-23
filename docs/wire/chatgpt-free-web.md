# ChatGPT free web app protocol (backend-api/conversation) - research record

Status: RESEARCHED, NOT SHIPPED as a fallback path. Decision rationale at
the bottom. Primary implementation remains `/backend-api/f/conversation`
with the prepare/sentinel pipeline (`providers/chatgpt/direct.rs`).

## Researched free-web flow (community-verified 2024-2026)

1. **Auth**: Bearer JWT from `GET https://chatgpt.com/api/auth/session`
   (cookie-authenticated). Anonymouse variant uses `backend-anon` host
   prefix with no bearer.
2. **Requirements**: `POST /backend-api/sentinel/chat-requirements` returns
   `{token, proofofwork:{required, seed, difficulty}, arkose:{required}}`.
3. **Proof token**: SHA3-512 proof-of-work over
   `seed + base64(json_config)`, nonce scan to ~100k, answer prefixed
   `gAAAAAB`. Already implemented: `providers/chatgpt/rpc.rs::generate_proof_token`.
4. **Conversation POST** (either host prefix):
   ```json
   {
     "action": "next",
     "messages": [{"id": "<uuid>", "author": {"role": "user"},
                   "content": {"content_type": "text", "parts": ["..."]}}],
     "parent_message_id": "<uuid>",
     "model": "<slug>",
     "conversation_mode": {"kind": "primary_assistant"},
     "force_use_sse": true,
     "websocket_request_id": "<uuid>",
     "history_and_training_disabled": false
   }
   ```
   Headers: `openai-sentinel-chat-requirements-token`,
   `openai-sentinel-proof-token`, optional
   `openai-sentinel-arkose-token`, cookies incl. `oai-did`.
5. **Stream**: SSE; events frequently carry the FULL accumulated message in
   `message.content.parts[0]` (not deltas). Consumers must dedupe by prefix.
   Moderation events: `type:"moderation"` with `moderation_response.blocked`.

## Why not shipped now

- The gateway's primary path (`f/conversation` prepare + conduit token +
  sentinel PoW) is already capture-aligned and regression-tested.
- The `/backend-anon` variant requires a different session bootstrap
  (anonymous csrf/device-id dance) that no local capture covers; shipping
  it blind would risk breaking the working authenticated path.
- Wire evidence required before implementing: F12 HAR of an anonymous
  chatgpt.com conversation including `/backend-anon/conversation` request +
  response headers.

## When evidence arrives

Implement `send_anon_conversation_request` beside
`send_conversation_request`, selected on auth-absence or primary 401/403,
reusing `generate_proof_token`. Dedupe cumulative SSE parts before feeding
the MTP pipeline (`MtpPipeline` / `mtp::MtpStreamState` handle tool blocks;
prefix-dedupe mirrors MiMo's `prev_content` approach).
