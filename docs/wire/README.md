# Wire protocol docs — index

Evidence-first references for each provider's internal API. Filenames follow
`docs/InitialPlan.md` §9.3 where they exist; older docs keep their original
names and are linked here so nothing is duplicated.

| Plan name | Actual doc | Coverage |
|---|---|---|
| `glm-v2.md` | [../GLM_FREE_WEB_PROTOCOL.md](../GLM_FREE_WEB_PROTOCOL.md) | chat.z.ai v2 completions, signature, SSE phases, chats/new, uploads, captcha classes |
| `chatgpt-backend-api.md` | [chatgpt-free-web.md](chatgpt-free-web.md) + [../KIMI_K3_NATIVE_RESPONSE.md] sibling notes in code (`providers/chatgpt/rpc.rs`) | `/backend-api/f/conversation` prepare + sentinel PoW (implemented); free-web `/backend-anon/conversation` research record |
| `kimi-api.md` | [../KIMI_K3_NATIVE_RESPONSE.md](../KIMI_K3_NATIVE_RESPONSE.md) + [kimi-connectrpc.md](kimi-connectrpc.md) | legacy moonshot SSE, K3 mavis session API, kimi.ai ConnectRPC frames |
| `gemini-internal.md` | [gemini-internal.md](gemini-internal.md) | StreamGenerate f.req envelope, request[9], candidate[28] tool slots, citations |
| `grok-rest.md` | [grok-rest.md](grok-rest.md) | grok.com REST flow, dynamic x-statsig-id, upload URIs |
| `mistral-lechat.md` | [mistral-lechat.md](mistral-lechat.md) | Le Chat tRPC newChat/append, `<code>:<json>` stream codes, patch dedupe |
| `docs/research/provider_rate_limits.md` | [../research/provider_rate_limits.md](../research/provider_rate_limits.md) | token-bucket configs, classifier cooldowns |

Cross-cutting:

- **Tool calling**: MTP/1 is the universal dialect —
  [streaming_tool_calls.md](streaming_tool_calls.md),
  [tool-calling-mirage/system-prompt.txt](tool-calling-mirage/system-prompt.txt),
  live native-tool evidence in [tool-calling/](tool-calling/COLLECTION-2026-08-21.md),
  audit status in [../research/mtp-provider-audit.md](../research/mtp-provider-audit.md).
- **Drift healing runbook**: [drift-healing.md](drift-healing.md).
