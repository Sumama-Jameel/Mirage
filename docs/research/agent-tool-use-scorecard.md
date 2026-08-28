# Agent-loop tool-use scorecard — 2026-08-27

Real-world MTP/1 verification: direct curl tests against the gateway's
OpenAI-compatible surface with a single `glob` tool. Task = "List all files
in current directory". Retested after MTP prompt overhaul (repair prompt,
tool ordering, per-model prompt styles).

Config: `~/obscura-agent-test/opencode.json` → `http://127.0.0.1:8080/v1`.

## Results (2026-08-27)

| Model | Chat | Tool Calling | Status |
|---|---|---|---|
| deepseek-chat | ✅ | ✅ glob called | **WORKING** |
| qwen-max | ✅ | ✅ glob called | **WORKING** |
| kimi-k2.6 | ✅ | ✅ glob called | **WORKING** |
| gemini-3.6-flash | ✅ | ✅ glob called | **WORKING** (improved!) |
| gpt-5.6 | ✅ | ❌ empty content | **BROKEN** — MTP not working |
| muse-spark | ✅ | ❌ refuses MTP | **BROKEN** — explicitly rejects format |
| minimax-m1 | 🔒 | 🔒 | **AUTH EXPIRED** — needs re-login |
| mistral-large | 🔒 | 🔒 | **AUTH EXPIRED** — needs re-login |
| claude-sonnet-5 | 🔒 | 🔒 | **NOT LOGGED IN** — needs sessionKey |
| mimo | 🔒 | 🔒 | **AUTH EXPIRED** — needs re-login |
| glm-5.2 | 🔒 | 🔒 | **CAPTCHA** — needs manual solve |

### Working: 4/12
- deepseek-chat, qwen-max, kimi-k2.6, gemini-3.6-flash

### Blocked by auth (5/12)
- minimax-m1, mistral-large, mimo: cookies expired, need Firefox re-login
- claude-sonnet-5: missing sessionKey/lastActiveOrg
- glm-5.2: anti-bot captcha active

### Broken (2/12)
- gpt-5.6: returns empty content when tools present, ignores MTP format
- muse-spark: explicitly says "I can't use the Mirage tool format"

## Previous results (2026-08-23)

| Model | File created | Tool loop | Notes |
|---|---|---|---|
| deepseek-chat | ✅ exact | ✅ | Full pass; also continuation + read-back |
| qwen-max | ✅ exact | ✅ | Multi-turn list→write now passes after flat-prompt ordering fix |
| muse-spark | ✅ content ok | ✅ | Wrote a stray second filename (model sloppiness) |
| mimo-v2.5 | ✅ (curl-verified) | ✅ | Auth fixed (userId lives on account.xiaomi.com, Xiaomi SSO) |
| kimi-k2.6 | ❌ via harness / ✅ forced + few-tools | ⚠️ | 14-tool auto prompts overwhelm it; forced tool_choice works |
| gemini-3.5-flash / 3.6-flash / 3.1-pro | ❌ | ⚠️ loop runs, drifts off-task | Reads project files, then chats about them |
| gpt-5.6 | ❌ | ⚠️ | Conflates MTP markers with apply-patch format |

## Changes since 2026-08-23

### MTP prompt overhaul (2026-08-27)
- `build_repair_prompt`: now shows MTP format structure, error, available tools, example
- `build_mtp_system_prompt`: per-model prompt styles (Minimal/Standard/Verbose)
- `compile_tools_for_prompt`: priority ordering (write/read/edit/bash/grep/glob first)
- `normalize_history`: clearer completion prompt
- Per-model `PromptStyle` in `ProviderProfile`:
  - DeepSeek/Qwen: Minimal (strong models)
  - GLM/Mistral/Gemini: Verbose (weak models, extra reminders)
  - Others: Standard

### Flat-prompt ordering fix (2026-08-24)
- qwen-max failed agent loop: gateway re-prepended MTP policy every turn
- Fixed via `mtp::compose_flat_prompt` — stable order with newest instruction last
- Applied to qwen, mimo, minimax, grok

### Gateway fixes (2026-08-23)
1. Gemini no longer rejects temperature/max_tokens/top_p/stop/penalties
2. MTP prompt gained anti-patch-format and exact-marker rules
3. `--data-dir` wiring, Kimi kimi.ai auth, ChatGPT split-cookie probe
4. Gemini cumulative-stream dedupe, BardErrorInfo classification

Note: opencode keeps a background server that caches provider config;
`OPENCODE_CONFIG` forces a fresh read per invocation.
