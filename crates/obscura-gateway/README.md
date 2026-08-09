# Obscura Gateway

A local, OpenAI-compatible API gateway that sits in front of free web UI AI models. It uses the [Obscura](https://github.com/h4ckf0r0day/obscura) Rust headless browser to reuse your own browser session cookies and expose provider chat interfaces as standard `/v1/chat/completions` endpoints.

**Supported providers:** DeepSeek, ChatGPT, Gemini, Kimi, GLM, Claude, and Meta AI. Every provider prefers its web app's internal background API (no provider API keys or PoW solvers) using native browser-session auth imported from your profile; Kimi, GLM, and Claude fall back to driving the authenticated public chat page only when the direct path is unavailable. Meta AI talks directly to Meta's Ecto-era DGW WebSocket with native browser-session auth (meta.ai cookies + the page-injected `ecto1:` token).

## What it does

1. Reads your existing DeepSeek cookies from your local Firefox profile on Linux.
2. Creates a small pool of isolated Obscura browser contexts and warms them by visiting `chat.deepseek.com`.
3. Exposes `http://127.0.0.1:3000/v1/chat/completions` and `/v1/models`.
4. Forwards your OpenAI-compatible requests to DeepSeek using the warmed session cookies.
5. Returns DeepSeek responses in OpenAI format, with SSE streaming support.

## Prerequisites

- Linux (Ubuntu/Debian tested).
- Firefox installed.
- You have signed in to [chat.deepseek.com](https://chat.deepseek.com) in Firefox.
- `libnss3.so` and `libnspr4.so` available on your system (usually installed with Firefox).
- Rust toolchain.

## Build

From the workspace root:

```bash
cd /home/sumama/Private/unnamed/obscura
cargo build -p obscura-gateway
```

The first build compiles V8, BoringSSL, and other native dependencies, so it takes several minutes. Subsequent builds are fast.

## Run

```bash
cargo run -p obscura-gateway
```

The gateway starts on `http://127.0.0.1:3000` by default.

### Configuration

Create `obscura-gateway.toml` in the directory where you run the binary:

```toml
[server]
host = "127.0.0.1"
port = 3000

[auth]
api_key = "obscura-local"

[firefox]
# Optional: explicit profile path. If omitted, the default profile is auto-detected.
profile_path = "/home/YOU/.mozilla/firefox/xxxx.default-release"

# Optional: fallback JSON cookie file if automatic SQLite/NSS extraction fails.
cookies_json_path = "/home/YOU/deepseek_cookies.json"

[glm]
# HMAC-SHA256 secret used to sign direct requests to chat.z.ai.
# Default: "junjie". Update if Z.AI rotates it.
sign_secret = "junjie"

# Direct internal endpoint. Default: https://chat.z.ai/api/v2/chat/completions.
upstream_url = "https://chat.z.ai/api/v2/chat/completions"

# Force the browser-UI path even when the direct API is available.
force_ui = false
```

GLM options can also be overridden via environment variables, e.g.
`OBSCURA_GATEWAY__GLM__SIGN_SECRET=...` or
`OBSCURA_GATEWAY__GLM__FORCE_UI=true`.

Environment overrides use the prefix `OBSCURA_GATEWAY__`, e.g.:

```bash
OBSCURA_GATEWAY__SERVER__PORT=8080 cargo run -p obscura-gateway
```

## Use with Cursor / Cline / any OpenAI client

Set the OpenAI base URL to:

```
http://localhost:3000/v1
```

Set the API key to match `auth.api_key` (default: `obscura-local`).

Select model:

- `deepseek-chat`
- `deepseek-reasoner`
- `chatgpt-auto`
- `gemini-3.5-flash`
- `gemini-3.1-pro`
- `gemini-3.1-flash-lite`
- `gemini-deep-research`
- `kimi-web`
- `glm-5.2` / `glm-5.1` / `glm-5` / `glm-5-turbo` / `glm-5v-turbo`
- `glm-4.7` / `glm-4.6v` / `glm-4.6` / `glm-4-plus` / `glm-4-zero` / `glm-4-think` / `glm-4-deepresearch`
- `glm-web` (alias for the account's currently selected web model)
- `claude-web`
- `muse-spark` (instant answers)
- `muse-spark-thinking` / `muse-spark-contemplating` (deep reasoning)

The `*-web` aliases deliberately select the account's current/default UI model.
They are stable gateway IDs, not promises that a named paid or regional model
is available. Use `/v1/models` to discover the aliases supported by the running
gateway.

## How sessions work

- The gateway imports provider cookies (DeepSeek, ChatGPT, Gemini, Kimi, GLM, Claude) from your browser profile using NSS decryption where needed.
- It keeps 2 browser contexts warm in a dedicated thread (`tokio::task::LocalSet`).
- Each API request borrows a warmed context, authenticates with the target provider, and makes the provider call.
- If a request fails, the session is marked dirty and re-warmed once before a single retry.
- The correct provider is selected based on the requested model ID.

### Provider-specific auth

| Provider | Auth method | Proof-of-Work |
|----------|-------------|---------------|
| **DeepSeek** | Bearer token (`userToken` from localStorage) + CSRF token | WASM-based SHA3-256 via `wasmtime` |
| **ChatGPT** | Bearer JWT (`/api/auth/session`) + session cookies | Pure Rust SHA3-512 |
| **Gemini** | Google session cookies + `SNlM0e` CSRF token | None (no PoW required) |
| **Kimi** | Existing Kimi browser session (`kimi-auth` cookie or localStorage token) | Bearer token for `kimi.moonshot.cn` internal API |
| **GLM** | Imported Z.AI JWT + cookies | Dual-layer HMAC-SHA256 (`X-Signature`) for `chat.z.ai/api/v2/chat/completions` |
| **Claude** | Existing Claude browser session (`sessionKey` + `lastActiveOrg` + `cf_clearance` cookies) | Cookie-authed `claude.ai/api` internal API |
| **Meta AI** | meta.ai session cookies (`ecto_1_sess` + `datr`) + `ecto1:` WS token (page RSC, or `META_AI_ECTO1_TOKEN` env override) | None |

## Security

- Binds to `127.0.0.1` by default; `0.0.0.0` is rejected.
- Cookies are loaded into memory only and never logged.
- Each context uses a temporary profile directory that is removed on process exit.
- Bearer token authentication is required for `/v1/*` routes.

## Known limitations

- **Linux only** for automatic Firefox cookie import. macOS/Windows users can use the JSON cookie fallback.
- **Provider internal API endpoints** are best-effort and may need adjustment if providers change them.
- **GLM** prefers the internal `chat.z.ai/api/v2/chat/completions` endpoint with dual-layer HMAC-SHA256 signing (`X-Signature`) using the imported Z.AI session token. When the direct path fails (missing token, signature/captcha challenge, or attachment upload error) the gateway automatically falls back to driving the authenticated web UI. Named model IDs (`glm-5.2`, `glm-5.1`, `glm-4.7`, etc.) carry per-model capability metadata, support multi-turn via `session_url`, reasoning/thinking toggle, web-search toggle, streaming, prompt-injected tool calling, image/file attachments (uploaded via `/api/v1/files/` on the direct path; on the UI fallback attachments are downloaded server-side and pushed into the page via `DataTransfer` so CORS is not required) and session continuation. There is no anonymous guest mode: the provider requires an imported logged-in chat.z.ai session (`token` localStorage entry or `token` cookie) and fails closed with an `Auth` error when none is present. Set `glm.force_ui = true` to skip the direct path, and `glm.sign_secret` / `glm.upstream_url` if Z.AI rotates them.
- **Kimi and Claude** use their internal HTTP APIs with native session auth (imported cookies/localStorage bearer tokens) and support streaming, prompt-injected tool calling, file/image upload, and multi-turn continuation via `session_url`. Feature support is gated per model (e.g. `kimi-search` for web search, vision-capable models for images); unsupported combinations are rejected with a clear `BadRequest` error, never silently dropped.
- **Meta AI** supports text chat, SSE streaming, prompt-injected tool calling, and multi-turn continuation via `session_url` (`https://www.meta.ai/c/<conversationId>`). It requires an authenticated meta.ai session: the gateway imports `ecto_1_sess` + `datr` cookies from the warmed browser and needs the `ecto1:` WebSocket token (extracted from the logged-in page's RSC payload, or provided via the `META_AI_ECTO1_TOKEN` environment variable). It has no file/image upload and no web-search channel on the DGW endpoint, so attachments and `"search": true` are rejected (fail closed). Missing auth fails with a clear `Auth`/`Provider` error — there is no anonymous mode. Meta geo-blocks some regions; the provider returns a clear `Provider` error for that case.
- Cookie decryption requires an unlocked Firefox profile (no master password).
- **ChatGPT free tier** only supports `chatgpt-auto` (GPT-4o-mini). Paid models require a ChatGPT Plus/Pro subscription.
- **Multi-turn conversations** require sending the `session_url` from a prior response back in the next request.
- **Gemini deep-research** model may require additional authentication scopes.

## Architecture

```
Cursor/Cline/Client
        │ OpenAI API
        ▼
obscura-gateway (axum)
        │
        ├── OpenAI request/response types
        ├── Provider registry
        │   ├── DeepSeek (direct API + WASM PoW)
        │   ├── ChatGPT (SSE streaming + SHA3-512 PoW)
        │   ├── Gemini (framed streaming + file upload)
        │   ├── GLM (direct v2 API + dual-layer HMAC + UI fallback)
        │   └── Meta AI (Ecto DGW WebSocket: GraphQL warmup + protobuf frames)
        ├── Firefox cookie importer (SQLite + NSS)
        ├── Session manager (Obscura context pool + on_response capture)
        └── Shared infrastructure
            ├── SessionStore (multi-turn conversation)
            ├── ToolCall formatting (XML + native)
            └── FileUpload (SSRF-safe)
        │
        ▼
   Obscura browser (cookie warm-up + auth extraction + UI driving)
        │
        ▼
   Provider backend (direct RPC for DeepSeek/ChatGPT/Gemini/GLM,
                    browser-mediated UI fallback for GLM)
```

## License

Apache-2.0, matching the upstream Obscura project.
