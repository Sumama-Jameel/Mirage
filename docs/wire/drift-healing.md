# Drift healing runbook

What happens automatically when a provider response can no longer be parsed,
and what a human does to close the loop. Implements InitialPlan §4.2
"protocol drift" with human-in-loop healing — the gateway never
auto-regenerates parsers (§16.7: no guess-driven implementation).

## Automatic (gateway)

1. The stream/non-stream path fails; `classify_error` labels it
   `response_parse` or `protocol_drift`.
2. The circuit breaker records the failure. Repeated parse failures degrade
   the provider early; drift opens the circuit immediately.
3. Where raw bytes are in hand, a snapshot is written:
   `<data_dir>/drift/<provider>/<ts>-<kind>.txt` (64 KB cap, newest 20 per
   provider). Current hooks:
   - GLM: non-SSE payload / HTTP error body (`parse`, `http-error`)
   - Kimi K3: empty-200 with non-empty body (`empty-200`)
   - ChatGPT: empty-200 with non-empty body (`empty-200`)
4. `/health` shows the provider state + `locked_out_secs`; gate errors carry
   a remediation hint.

## Human (healing)

1. Read the newest snapshot under `drift/<provider>/`. Compare against the
   provider's wire doc in this directory.
2. Recapture live traffic (F12 or proxy) for one normal turn.
3. Diff: renamed fields? new envelope? moved tool/citation slots?
4. Patch the parser + its unit tests (captured shapes become fixtures).
5. Restart the gateway; the half-open canary validates the fix and closes
   the circuit on success.
6. Record the finding in the provider's wire doc so the next drift is a
   diff, not research.
