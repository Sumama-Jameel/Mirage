# Obscura Gateway — Infrastructure Reference (implemented)

Companion to `docs/InitialPlan.md` (the original design). This document
records the infrastructure that is **implemented today** in
`crates/obscura-gateway`, mapping each plan phase to code. Sections marked
"[gap]" are still open items.

## Request guard pipeline

Every `POST /v1/chat/completions` request passes through guards in order
(`api/mod.rs`, `chat_completions`):

1. **Model + request validation** — `req.validate()` (missing model, empty
   messages) then `provider.validate_request(&req)` (provider-specific
   impossibilities such as attachments without upload support).
2. **Circuit breaker** — `state.health.gate_new_request(provider_name)`
   fails fast while a lockout is armed.
3. **Credential pre-flight** — if an imported profile snapshot exists,
   `state.auth_checker.check(...)` probes required cookies /
   localStorage / env var. Missing credentials return an `Auth` error that
   lists the exact missing names.
4. **Rate limit + concurrency** — `state.rate_limiter.acquire()` waits
   (bounded 30 s) for a token bucket slot and queues on the per-provider
   concurrency semaphore.
5. **Run + classify** — non-stream calls record success/error on the
   circuit. Streams are wrapped in `HealthTrackingStream`, which settles
   health on clean stream end (success) or first error (classified).
   A classified `RateLimit` also arms the limiter's 429 cooldown.

`GET /health` returns an unauthenticated snapshot:

```json
{
  "status": "ok",
  "providers": [
    { "provider": "grok", "state": "healthy", "consecutive_failures": 0, "locked_out_secs": 0 }
  ]
}
```

## Circuit breaker (`providers/health.rs`)

Per-provider `Status { state, consecutive_failures, lockout_until }`.

- Healthy: requests pass.
- Failure with a circuit-breaking class opens the circuit and arms a
  lockout for the class cooldown; gate rejects with a remediation hint while
  locked out.
- Lockout expiry transitions to `Degraded` (half open); the next request is
  a canary probe. Success heals; a repeat failure re-arms with an escalated
  cooldown (`cooldown * consecutive_failures`, max 6x).

`Parse` and `Other` carry no circuit-opening pressure (transient blips).

### Error classes and cooldowns

| Class                | Cooldown |
|----------------------|----------|
| `auth_missing`       | 10 min   |
| `auth_expired`       | 12 h     |
| `challenge_required` | 20 min   |
| `rate_limited`       | 60 s     |
| `quota_exhausted`    | 6 h      |
| `protocol_drift`     | 30 min   |
| `network`            | 5 min    |
| `model_unsupported`  | 60 s     |
| `response_parse`     | 30 s     |
| `other`              | 30 s     |

`HealthState` serializes directly into `/health`; `challenge_required`,
`auth_*`, `quota_exhausted`, `protocol_drift` carry human `remediation()`
hints surfaced in gate errors (plan §4 `HEAL_HUMAN_REQUIRED` style:
"re-import a logged-in profile" / "solve captcha in source browser" etc.).

### Classifier priority (`classify_error`)

1. Missing-credential markers on profile-gated providers => `auth_missing`
2. HTTP 401 => `auth_expired`
3. Grok 403 + challenge markers (`captcha`, `人机验证`, `anti-bot`,
   `IllegalUserTag`, `verify you are human`, `page was deleted`) =>
   `challenge_required`
4. HTTP 429 + rate markers (`6200`, `rate limit`, `too many requests`) =>
   `rate_limited`
5. Quota markers (`42212`, `2056`, `token plan`, `quota`, `credit`,
   `insufficient balance`, `usage cap`, `account locked`) => `quota_exhausted`
6. `token_expired` / `session expired` => `auth_expired`
7. Decode markers (`decode failed`, `empty response body`, `unexpected sse`,
   `invalid json`, `unknown phase`) => `response_parse`
8. Transport markers (`timed out`, `connection reset`, `dns`) => `network`
9. `model ... not found` / `unknown model` => `model_unsupported`
10. Protocol markers (`protocol`, `drift`, `marker`, `missing field`) =>
    `protocol_drift`
11. else `other`

The same classifier drives stream and non-stream paths.

## Rate limiting (`ProviderRateLimiter`)

Token bucket + concurrency semaphore per provider. `acquire` waits up to
`MAX_RATE_WAIT` (30 s) for a token; `RateLimitPermit` releases the
concurrency slot on drop. `record_ratelimited` halts token grants for
`cooldown_after_429`.

`rate_config_for` (plan §8, phase-0):

| Provider | rpm | concurrency | burst | 429 cooldown |
|----------|-----|-------------|-------|--------------|
| default  | 60  | 4           | 4     | 60 s         |
| mistral  | 10  | 2           | 2     | 120 s        |

[gap] plan §8.2 priority queue: not implemented (single queue).

## Credential pre-flight (`providers/authcheck.rs`)

`check(provider, cookie_jar, local_storage) -> AuthVerdict { ok, missing }`.
`CachedAuthChecker` wraps with a 5-minute TTL.

| Provider | Proof of auth | Type |
|----------|---------------|------|
| chatgpt  | `__Secure-next-auth.session-token` or `__Host-next-auth.csrf-token` @ chatgpt.com | cookie any-of |
| claude   | `sessionKey` + `lastActiveOrg` @ claude.ai | cookie all |
| deepseek | `userToken` in localStorage `https://chat.deepseek.com` | localStorage |
| gemini   | `__Secure-1PSID` / `SID` / `HSID` @ gemini.google.com, .google.com | cookie any-of |
| glm      | `token` cookie @ chat.z.ai, or `token`/`access_token` localStorage | cookie or localStorage |
| grok     | `sso` + `sso-rw` @ grok.com | cookie all |
| kimi     | `kimi-auth` cookie, or `access_token` localStorage @ www.kimi.com | cookie or localStorage |
| metaai   | `c_user` / `xs` / `dm_user_id` @ meta.ai | cookie any-of |
| mimo     | `xiaomichatbot_serviceToken` + `userId` + `xiaomichatbot_ph` | cookie all |
| minimax  | `MINIMAX_JWT` env var, or `_token`/`mavis:token` localStorage @ agent.minimax.io | env or localStorage |
| mistral  | `mistral-chat-session` / `anonymous-id` @ chat.mistral.ai, mistral.ai | cookie any-of |
| qwen     | `token` / `cna` @ chat.qwen.ai, .qwen.ai | cookie any-of |

Snapshot source: `SessionManager::imported_snapshot()` rebuilds a
`CookieJar` + localStorage entries from the imported profile. If no profile
was imported, pre-flight is skipped (provider validation still applies).
Providers without a declared probe are not gated.

## What was removed (plan §16.3: no UI automation as primary transport)

- `src/chat.rs`, `providers/glm/{ui,humanize,captcha,response}.rs` deleted.
- `Provider` trait slimmed to `name`, `url`, `models`, `chat`,
  `chat_stream`, `supports_attachments`, `validate_request`. No
  `DoneSignal` / `ChatMode::Ui` / selectors / done prompts.
- GLM is direct-only.
- `session.rs` capture machinery removed (`ExtractedTexts`,
  `CapturedResponse`, `ActiveCapture`, capture commands).
- Grok no longer has a done-prompt monitor; continuation state lives in
  `GrokSessionStore` (persisted to `<data_dir>/grok.json`) and 403s are
  quarantined by the circuit breaker.

## Timeouts (plan §6.5.D)

`StealthHttpClient` builds with `Timeouts`:
- default: connect 5 s, total request 30 s.
- `Timeouts::streaming()`: connect 5 s, request 300 s (used by grok, whose
  SSE streams run for the whole turn).

## Ops runbook

- Provider turns red => `GET /health` shows class + `locked_out_secs`;
  gate errors carry remediation hints (re-import profile, solve captcha,
  top up quota, provider update for drift).
- Circuits self-heal via canary after lockout expiry; no manual reset, but
  state is in-memory, so a gateway restart resets all lockouts (safe: one
  canary per provider re-learns).
- Persistence: grok conversation state only (`--data-dir`). Health/limiter
  state is intentionally in-memory.
- Maintain: re-extract grok x-statsig-id constants per
  `docs/EXTRACT_GROK_CONSTANTS.txt`; keep `X_FE_VERSION` GLM header current.

## Definition of Done status (plan §12)

| Criterion                                             | Status |
|-------------------------------------------------------|--------|
| Release build clean                                   | yes (0 warnings) |
| Test build clean                                      | yes (0 warnings) |
| Tests pass                                            | 483 pass, 0 fail (`cargo nextest run -p obscura-gateway`) |
| Phase 0 (health/breaker/limiter/classifier)           | done |
| Phase 1 GLM direct-only                               | done (fallback removed) |
| Phase 5 Grok constants/vault                          | partial: randomized statsig + retry-once on 403 + streaming timeouts; dynamic vault still open |
| Phase 3 Kimi, Phase 4 Gemini, Phase 2 ChatGPT          | live verification pending |
| Phase 6 auth-gated providers                          | done at the pre-flight level (Claude/MiMo/Minimax report clean auth status) |
| No manual runtime labour                              | yes for the implemented paths |