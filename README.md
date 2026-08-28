# Mirage

![Rust](https://img.shields.io/badge/Rust-2021-orange)
![License](https://img.shields.io/badge/License-Apache%202.0-blue)
![Models](https://img.shields.io/badge/Models-75-green)
![Providers](https://img.shields.io/badge/Providers-6-brightgreen)

Open source AI gateway. Use the free internal APIs of ChatGPT, Gemini, DeepSeek, and 3 more providers through a single OpenAI-compatible endpoint. No API keys. No GPU. No vendor lock-in.

## What This Solves

LLM APIs are unreliable, expensive, and fragmenting.

OpenAI logged **166 incidents in 9 months** -- roughly 18 per month. Anthropic's commercial Claude uptime sits at **99.36%**, below the enterprise SLA threshold of 99.9%. Across ChatGPT, Claude, Gemini, and Copilot, high-signal disruption days rose from **6 in Q1 2025 to 51 in Q1 2026**. When a critical AI dependency goes down, a 50-person engineering team loses **$4,000 to $7,500 per hour**.

Meanwhile, **81% of enterprise CIOs** plan to use two or more LLM providers in 2026, and **37% are already running 5 or more models in production**. Switching between them is not free: documented migration costs include **$315,000 and 3 months** for a mid-size workload, and total exit cost on a fully loaded enterprise AI stack runs **$200K to $1M** in re-engineering.

Every existing gateway makes the same assumption: you have API keys and are willing to pay per token. They route your requests through another paid layer, add another vendor to trust, and introduce another point of failure.

Mirage does not do this. It uses the **free internal APIs that web browsers use** -- the same endpoints that power the chat UIs you already log into. Import your browser sessions, and every model is available through one OpenAI-compatible endpoint. No API keys. No per-token billing. No middleman.

## What We Need

The providers in this project are large companies with massive engineering teams and infrastructure budgets. They can change their web UIs and internal APIs at any time. When they do, integrations break. This is not a one-time build. It is an ongoing race.

We need a community around this project. People who use free-tier AI models, people who understand browser protocols, people who can spot when a provider has changed their wire format. When something breaks, we need people who can capture the new format and help fix the adapter.

If you are reverse-engineering a provider's internal API, use [Whelmer](https://github.com/Sumama-Jameel/Whelmer). It captures every browser protocol event -- CDP, BiDi, FDP -- unfiltered, so you can see exactly what the provider's web app sends and receives. It was built for this kind of work.

Star this repo. Open issues when something breaks. Submit captures. The more people running this against real providers, the faster we keep it working.

## Built With Obscura

Mirage is built on [Obscura](https://github.com/h4ckf0r0day/obscura), a headless
browser engine in Rust. It runs real JavaScript through V8, maintains a real DOM tree,
and speaks the Chrome DevTools Protocol -- the same interfaces as headless Chrome but
faster and lighter.

Obscura provides three things to Mirage:

- **Session import** -- reads browser cookies and localStorage from Firefox, Chrome,
  or Edge, so Mirage can authenticate to providers using your existing login sessions.
- **Browser fingerprinting** -- presents real TLS and HTTP fingerprints that match a
  genuine browser, so provider anti-bot systems do not flag the traffic.
- **Protocol engine** -- speaks Chrome DevTools Protocol, BiDi, and Firefox DevTools
  Protocol, giving Mirage direct access to browser internals when needed.

Obscura is ~12x faster and uses ~6x less memory than headless Chrome on framework pages.

## Quick Start

```bash
git clone https://github.com/Sumama-Jameel/Mirage.git
cd Mirage
cargo build --release
./target/release/obscura-gateway
```

The gateway starts on `127.0.0.1:3000`. Test it:

```bash
curl http://127.0.0.1:3000/v1/models \
  -H "Authorization: Bearer obscura-local"
```

Send a completion:

```bash
curl http://127.0.0.1:3000/v1/chat/completions \
  -H "Authorization: Bearer obscura-local" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-chat",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

## Features

- **6 providers, 75 models working now** -- ChatGPT, DeepSeek, Gemini, Qwen, Mistral, Meta AI. 5 more providers coming soon.
- **OpenAI-compatible API** -- drop-in replacement for any OpenAI client: LangChain, LlamaIndex, OpenWebUI, and more
- **Streaming and non-streaming** -- SSE streaming with `stream: true`, or single-response completions
- **Tool calling** -- MTP (Mirage Tool Protocol) provides structured tool calling across all providers. Models emit tool blocks in their text output, which the gateway parses and validates. Some providers also support native tool APIs as an optimization.
- **File upload** -- native file upload per provider
- **Thinking toggle** -- `thinking: true` enables chain-of-thought reasoning where supported
- **Web search** -- `search: true` enables web-grounded responses with citations
- **Session continuation** -- `session_url` lets clients continue conversations without resending history
- **Browser session import** -- reads cookies and localStorage from Firefox, Chrome, or Edge
- **Session pool** -- 10 warmed browser contexts in background, each on its own OS thread
- **Circuit breakers** -- per-provider health classification, exponential backoff, auto-healing
- **Rate limiting** -- per-provider token buckets prevent burst failures
- **Real browser fingerprints** -- TLS ClientHello, User-Agent, navigator properties match a real browser
- **Single binary** -- no Docker, no Node.js, no Python. Just the binary.

## Supported Providers

### Working Now

| Provider | Models | Endpoint |
|----------|--------|----------|
| ChatGPT | 21 | `chatgpt.com` |
| Qwen | 19 | `chat.qwen.ai` |
| Mistral | 11 | `chat.mistral.ai` |
| DeepSeek | 9 | `chat.deepseek.com` |
| Gemini | 6 | `gemini.google.com` |
| Meta AI | 3 | `meta.ai` |

### Partially Working

| Provider | Models | Status |
|----------|--------|--------|
| Kimi | 6 of 9 | `kimi-k3`, `kimi-k2.7-code-highspeed`, `kimi-research` broken |

### Coming Soon

Claude, Grok, GLM, MiniMax, and MiMo are actively being integrated. Each one has
unique challenges -- anti-bot systems, authentication flows, and evolving APIs --
that make them harder to crack. They are coming.

## Environment Variables

All variables use the prefix `OBSCURA_GATEWAY__` with `__` as the separator.

| Variable | Default | Description |
|----------|---------|-------------|
| `OBSCURA_GATEWAY__SERVER__HOST` | `127.0.0.1` | Bind address |
| `OBSCURA_GATEWAY__SERVER__PORT` | `3000` | HTTP port |
| `OBSCURA_GATEWAY__AUTH__API_KEY` | `obscura-local` | Bearer token for `/v1/*` routes |
| `OBSCURA_GATEWAY__BROWSER__SOURCE` | `auto` | Browser to import from: `auto`, `firefox`, `chrome`, `edge` |
| `OBSCURA_GATEWAY__BROWSER__PROFILE_PATH` | -- | Explicit browser profile directory |
| `OBSCURA_GATEWAY__BROWSER__COOKIES_JSON_PATH` | -- | Fallback JSON cookie file |
| `OBSCURA_GATEWAY__BROWSER__IDENTITY` | `firefox` | Fingerprint to present: `firefox`, `chrome`, `edge` |
| `OBSCURA_GATEWAY__BROWSER__USER_AGENT_OVERRIDE` | -- | Manual User-Agent override |
| `OBSCURA_GATEWAY__GLM__SIGN_SECRET` | `junjie` | HMAC-SHA256 signing secret for Z.AI |
| `OBSCURA_GATEWAY__GLM__UPSTREAM_URL` | `https://chat.z.ai/api/v2/chat/completions` | Z.AI chat endpoint |
| `OBSCURA_GATEWAY__DATA_DIR` | -- | Directory for persistent session storage |
| `RUST_LOG` | `info` | Tracing filter |

Or use `obscura-gateway.toml` in the working directory:

```toml
[server]
host = "127.0.0.1"
port = 8080

[auth]
api_key = "my-secret-key"

[browser]
source = "firefox"
identity = "firefox"
```

## Architecture

```
OpenAI-compatible client
        |
        v
  +-----------+
  | API Server|  axum HTTP on 127.0.0.1:3000
  | /v1/*     |  Bearer auth, CORS
  +-----------+
        |
        v
  +--------------+
  |   Provider   |  model ID -> provider adapter
  |   Registry   |  6 adapters, 75 models
  +--------------+
        |
        v
  +--------------+
  |   Provider   |  builds native request from OpenAI format
  |   Adapter    |  sends direct HTTP/SSE to provider API
  |              |  parses provider-native response
  |              |  normalizes to OpenAI response format
  +--------------+
        |
        v
  +--------------+
  |   Session    |  pool of 10 warmed browser contexts
  |   Manager    |  each = OS thread + V8 isolate
  |              |  cookies/localStorage from real browser
  +--------------+
        |
        v
  Provider's internal API
  (chat.deepseek.com, gemini.google.com, etc.)
```

**Request flow:** Client sends OpenAI-format request. Gateway resolves model ID to provider adapter. Adapter translates to provider's native wire format. Session manager provides authenticated browser context. Response is normalized back to OpenAI format and returned.

## Competitor Comparison

| | Mirage | OmniRoute | LiteLLM | Portkey | Bifrost |
|---|---|---|---|---|---|
| **Free tier access** | Yes | Yes | No | No | No |
| **API keys required** | No | Some | Yes | Yes | Yes |
| **Tool calling** | MTP (all providers) | No | Depends | Depends | No |
| **Browser session import** | Yes | No | No | No | No |
| **Providers** | 6 working | 350+ | 100+ | 250+ | 20+ |
| **Language** | Rust | TypeScript | Python + Rust | TypeScript | Go |
| **License** | Apache 2.0 | MIT | MIT | Apache 2.0 | Apache 2.0 |

OmniRoute aggregates documented free tiers using API keys. Mirage uses browser sessions directly -- no keys, no billing, no middleman.

## Security

| Layer | Mechanism |
|-------|-----------|
| API authentication | Bearer token on all `/v1/*` routes |
| Browser fingerprint | Real TLS ClientHello, User-Agent, navigator properties |
| SSRF protection | Loopback, RFC1918, and link-local fetches blocked by default |
| Rate limiting | Per-provider token buckets with exponential backoff |
| Circuit breakers | Health classification: HEALTHY, DEGRADED, RATE_LIMITED, AUTH_EXPIRED |
| Session encryption | On-disk session vault with encrypted persistence |
| Auth refresh | Automatic re-import of browser profile cookies on expiry |
| Bind address | `0.0.0.0` rejected at validation; localhost only by default |

## Project Structure

```
Mirage/
  crates/
    obscura-gateway/     AI gateway: API server, providers, session management
    obscura-browser/     Page type, navigation, JS evaluation
    obscura-js/          V8 runtime via deno_core
    obscura-dom/         DOM tree
    obscura-net/         HTTP client, stealth client, cookie jar
  docs/
    research/            Reverse-engineering notes and wire format captures
  GOAL.md                Project requirements and constraints
```

## Test

```bash
cargo test --workspace
```

## Build for Production

```bash
cargo build --release
```

Binary at `./target/release/obscura-gateway`.

## License

Apache 2.0. Build real things. No walled gardens.
