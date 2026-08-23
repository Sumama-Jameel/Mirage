# MTP/1 migration - provider audit notes (2026-08)

Post-migration audit of every provider after the universal MTP/1 tool
dialect landed. Live verification checklist at the bottom.

## Tool dialect status

| Provider | Dialect path | Native tools forwarded? | Notes |
|---|---|---|---|
| deepseek | mtp (MtpStreamState) | no (never was) | reference implementation |
| glm | mtp | no (tools/tool_stream removed from body) | chats/new now real; upload prefers meta.cdn_url |
| qwen | mtp | no | native SSE delta.tool_calls take precedence when present |
| mistral | mtp | no (never sent) | prompt compiled into latest-user text |
| mimo | mtp | n/a (flat prompt) | transcript renders assistant calls as MTP blocks |
| metaai | mtp | n/a | native DGW search-results blocks still parsed first |
| minimax | mtp | no (payload tools removed) | native agent_message.tool_calls remain priority 1 |
| claude | mtp | no | native content_block_start tool_use remains priority 1 |
| grok | mtp | n/a (message string) | native tool_usage_card remains priority 1 |
| chatgpt | mtp | no (body tools removed) | /f/conversation prepare+sentinel unchanged |
| gemini | mtp | no (request[9] emptied) | native candidate[28] + gemini fences still parsed first |
| kimi | mtp over ConnectRPC (opt-in) | n/a | see docs/wire/kimi-connectrpc.md |

## Invariants verified by tests

- `strip_upstream_tools`: chatgpt rpc test asserts payload carries no tools;
  glm/gemini tests assert empty tool slots.
- Conformance suite (`providers/conformance.rs`): plain chat, echo block,
  forced choice, invalid-block repair, char-split markers, built-in name
  suppression.
- `finish_reason: "tool_calls"` emitted only with validated calls.

## Fail-fast auth status

`providers/authcheck.rs` probes gate all cookie/local-storage providers and
return per-provider verdicts before any network call. Minimax/Claude/MiMo
remain fail-fast with explicit auth errors (unchanged).

## Rate limiting

- mistral: 429/5xx retried via shared `send_with_retry`; continuation state
  (chatId/messageId/version) persisted across cooldowns in the session
  store, so an append after a cooldown resumes the same thread.
- minimax/qwen/kimi: session stores TTL-bounded; expired sessions return a
  clear error rather than forking silently.

## Live verification checklist (requires real accounts)

1. Per provider: non-tool chat, tool call round-trip (stream + non-stream),
   thinking toggle, upload, search/citations where supported.
2. Kimi ConnectRPC: `OBSCURA_KIMI_CONNECT_RPC=1`, new chat, verify
   chat.id/assistant message id persistence, then flip default.
3. ChatGPT: capture anonymous conversation HAR if the anon fallback is wanted.
4. GLM: confirm server-created chat ids appear in web history.
