# Meta AI (muse-spark) status — as of 2026-08-09 14:34 UTC

Session scope: get `muse-spark` working end-to-end on the live Obscura gateway,
pure-HTTP token path (no browser), response time acceptable (no 30s+ hangs).

## Verified working
- rd_verify challenge solve over pure HTTP: steps a/b/c in ~1s.
  - `extract_ecto_token(cookie_header)` does: GET home (403 challenge),
    POST `/__rd_verify_...challenge=3` (200, sets rd_challenge), GET home again.
- ecto1 token regex extraction (`TOKEN_RE` = `ecto1:[^"]+`, `TOKEN_RE_BARE` = bare base64url).
  - 26 auth tests pass; full suite 467 tests pass.
- Browser path proven unusable for meta.ai: 127+ script SPA trips the V8
  watchdog (synchronous overrun) → isolate left hung; pure HTTP is the way.
- DGW WebSocket handshake auth:
  - WRONG: `x-dgw-authtype=ecto` + appid `278093554249291` → 4003 Unauthorized.
  - CORRECT: `x-dgw-authtype=15:0` (ABRA) + appid `1522763855472543` + version 5
    → server ACCEPTS the connection and replies with binary frame `\n`.
    (These constants are already in `direct.rs` WS_* consts.)
- Intro frame (6-byte header type 0x0f + JSON conversation-id payload) →
  server returns `{"code":200}`.

## Blocked / still broken
- `x-dgw-establish-stream-frame-base64` NOT sent in the connect URL.
  - The web app's `constructConnectUrl` requires it
    (`if(!r.includes(R))return;` aborts stream establishment without it).
  - The frame is protobuf-encoded by a WASM codec
    (`__DgwCodecEncodeStreamGroup_EstabStream`); it wraps
    (streamId, JSON of the prefixed grouped-stream headers).
  - The wasm binary `dgwcppbridge.wasm` is lazily loaded by chunk 9658 and was
    NOT retrievable (404 on guessed `_next/static/chunks/dgwcppbridge.wasm`);
    it is injected by turbopack via `Module['wasm']`, not a static asset path.
  - Status: NEED the wasm binary (or a captured server frame) to reverse the
    DGW v5 (DGWVER_BIG_IDS) EstablishStream frame wire format.
    Without it, no Rust implementation is possible.
  - Even if the establish frame is missing the connection is still ACCEPTED
    (server sends Pong), so auth is not the blocker; the blocker is whether
    the server produces assistant output without an established stream group.
- Prompt frame → response cycle unverified and currently hangs.
  - After intro (`{"code":200}`) the gateway sends the mutated proto prompt
    frame, then the 300s `WS_TIMEOUT` elapses with no assistant content.
    `curl -m 120` returns HTTP 000 (deadline). This is the "30s+ wait" pain.
  - Unknown whether the prompt proto template / field mutation paths
    ([1,1,5],[2,1,1],[2,1,2],[2,2],[1,5],[1,6],[1,10,4]) still match the
    server's current expected payload, or whether a StreamGroup binary
    establish exchange is now mandatory before a Data frame is accepted.
- Token extraction is FLAKY.
  - `extract_ecto_token` step-c sometimes returns 403 body_len=0
    (`token_found=false`), even though step-b reports `has_rd_cookie=true`.
  - `solve_resp.headers().get_all("set-cookie").iter().next()` captures only
    the FIRST set-cookie. Meta's rd_verify POST may set multiple cookies
    (rd_challenge + a refreshed nonce / ecto refresh) that must ALL be
    forwarded on the step-c GET. This staleness made the WS path unreachable
    in the later probes.

## Not re-tested this session (stale docs assumed authoritative: NO)
- Claude: no `sessionKey` in profile (needs user login — not a code bug).
- MiMo: missing `userId` cookie on aistudio.xiaomimimo.com.
- minimax: account Token-Plan quota 2056 (account-side).
- DeepSeek: PoW warm log "navigation exceeded 30000ms deadline" (non-fatal).
- grok-auto, glm-5: 08-08 baseline not re-run; docs stale.

## Concrete next steps
1. `extract_ecto_token`: capture ALL `set-cookie` values from the solve POST
   and forward them on step c (fixes flakiness / 403).
2. Retrieve `dgwcppbridge.wasm` (trace the turbopack wasm asset URL from the
   chunk manifest) and reverse the EstablishStream frame format, then add
   `x-dgw-establish-stream-frame-base64` to `build_ws_url`.
3. Confirm whether prompt frames (old text/6-byte format) or new
   StreamGroup Data (protobuf, type 13) frames are expected on the v5 connection.
4. Lower `WS_TIMEOUT` (300s) to ~15–20s for non-thinking replies.
5. Re-run the live baseline across glm-5, grok-auto, minimax-m3, MiMo,
   DeepSeek; refresh stale docs.
