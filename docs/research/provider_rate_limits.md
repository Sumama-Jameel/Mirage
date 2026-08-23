# Provider rate limits and cooldowns

Implementation: `providers/health.rs` (`rate_config_for`,
`ProviderRateLimiter`, `ErrorClass::cooldown`). InitialPlan §8.

## Token bucket + concurrency per provider

| Provider | rpm | concurrency | burst | 429 cooldown |
|----------|-----|-------------|-------|--------------|
| default  | 60  | 4           | 4     | 60 s         |
| mistral  | 10  | 2           | 2     | 120 s        |

- `acquire(provider, priority)`: waits up to 30 s for a rate token, then
  queues on the per-provider concurrency semaphore.
- Priority lanes (`ConcurrencyGate`, 4 FIFO queues): `UserLive` >
  `Continuation` > `Canary` > `ResearchProbe`. Normal requests enter as
  `UserLive`; a half-open circuit's first request enters as `Canary`.
- A classified `RateLimit` halts token grants for the cooldown window
  (`record_ratelimited`).

## Circuit-breaker cooldowns by error class

| Class | Cooldown | Escalation |
|---|---|---|
| auth_missing | 10 min | x consecutive failures (max 6x) |
| auth_expired | 12 h | " |
| challenge_required | 20 min | " + persisted across restarts for grok (`grok.json` anti_bot section) |
| rate_limited | 60 s | " |
| quota_exhausted | 6 h | " |
| protocol_drift | 30 min | " |
| network | 5 min | " |
| model_unsupported | 60 s | " |
| response_parse | 30 s | opens only on the 3rd consecutive failure |
| other | no circuit pressure | - |

## Provider-specific notes

- **Minimax**: account-level Token Plan quota is additionally cached in the
  provider (`QuotaBlock`, 30 min TTL) so known-dead credentials fail in ~0 ms
  instead of the ~8 s upstream round-trip.
- **Mistral**: continuation state persists across 429 cooldowns; the next
  append resumes the same chat thread.
