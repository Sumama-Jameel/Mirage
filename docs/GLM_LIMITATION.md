# GLM / Z.AI limitation

GLM is the only provider in this gateway whose internal API is gated by an
in-browser captcha that Obscura's self-contained headless engine cannot pass.

## What happens

Z.AI's `POST /api/v2/chat/completions` returns
`code: FRONTEND_CAPTCHA_REQUIRED` and demands a `captcha_verify_param` in the
body. The param is produced by the Aliyun NVC captcha widget that the chat.z.ai
SPA loads in-browser. In Obscura's headless engine the widget runs but its
traceless integrity check rejects the runtime and the `success` callback fires
with `{}` — no certifyId. The retry with an empty param is then rejected with
`captcha_error_type: missing_param`. The gateway surfaces this as a 502.

This is not a code bug. The gateway correctly warms the page, reads the live
cookies (including `ssxmod_itna`), extracts the real fingerprint, signs the
request, and runs the Aliyun widget. The widget itself refuses to certify the
headless browser, because Aliyun's integrity verdict depends on runtime
signals (canvas, WebGL, audio) that a stock headless engine cannot legitimately
reproduce. `captcha.rs` in this repo documents that the server rejects
server-computed proofs (`F002` / `F019`).

DeepSeek, ChatGPT, Gemini, Kimi, and Claude do not gate their internal API
this way and work fully self-contained.

## Operator options

1. **Attach the gateway to a real browser via CDP.**
   The real browser passes the integrity check, the widget produces a real
   certifyId, and the internal API + captcha-retry pipeline returns real GLM
   completions.
   Trade-off: per-command IPC latency, and it requires the user to keep a
   real browser running and a CDP port open. It also breaks Obscura's
   self-contained promise for that provider.

2. **Wait for a real Aliyun bypass.** None is currently known. A full
   reverse-engineer of the in-browser integrity algorithm is a deep,
   uncertain effort and may still be server-rejected.

The gateway will not ship a fragile spoof that breaks the next time Aliyun
updates its check.
