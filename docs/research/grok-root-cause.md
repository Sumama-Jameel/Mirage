# Grok: exact root cause and path forward

## The problem

Grok returns `{"code":7,"message":"This page is out of date"}` on ALL API
requests from the gateway. This is NOT a Cloudflare block (we get HTTP 200)
and NOT an auth failure (cookies are accepted). It is a **client-version
validation failure**.

## Root cause chain

```
grok.com web app JS → botoxSign(path, method) → x-statsig-id header
                                                     ↓
grok.com API server ← validates token against CURRENT deploy release hash
                                                     ↓
                                              MISMATCH = code 7
```

The `botoxSign` function is inside chunk `05-qn-i6ali3h.js` (module 1645000).
It is protected by:
1. String-array obfuscation with RC4 decoding and array rotation
2. Anti-tampering checksum (rotation count validated against constant 589327)
3. Browser API dependencies (`document.currentScript`, possibly `crypto.subtle`)
4. Per-deploy re-obfuscation (chunk hash changes on every deploy)

## What we tried

| Approach | Result | Why it failed |
|---|---|---|
| Synthetic error markers | ❌ code 7 | Worked until ~Aug 20; xAI tightened validation |
| Dynamic release scraping + Baggage | ✅ works | Correctly tracks deploy changes |
| Firefox TLS emulation + all cookies | ✅ passes CF/auth | Eliminated cookie/UA mismatch |
| Signer chunk extraction from CDN | ✅ found it | Self-contained, only btoa/atob/Math |
| V8 evaluation of signer chunk | ⚠️ timeout | Module registers but signing Promise never settles — likely needs browser APIs (crypto.subtle, DOM events) not in bare Deno |
| Captured token replay | ❌ stale tokens | Tokens bound to old deploy release |
| 38 captured tokens rotation | ❌ same | All captured pre-deploy; invalid for current deploy |

## Why browser harvesting failed

The SPA loads fully under SessionManager automation (234 assets). But:
- No `/rest/*` calls fire during the observation window (SPA boot incomplete)
- XHR interception doesn't capture the statsig header (added below XHR layer,
  likely in a service worker or compiled fetch wrapper that keeps a private
  reference to the native fetch)

## Path forward: browser-as-transport

The ONLY reliable approach is to make the chat request FROM WITHIN the
logged-in grok.com page, letting the app's own interceptors add valid auth
headers automatically:

1. Navigate session page to grok.com
2. Wait for full SPA load (~30s on low-end hardware)
3. Execute JavaScript inside the page that makes the actual API call:
   ```js
   const resp = await fetch('/rest/app-chat/conversations/new', {
       method: 'POST',
       headers: {'Content-Type': 'application/json'},
       credentials: 'include',
       body: JSON.stringify(payload)
   });
   return await resp.text();
   ```
4. Parse the response text using existing NDJSON parsers

This eliminates ALL anti-bot concerns because we use the exact same code
path as a human user's browser.

## Implementation notes

- Non-streaming first (proof of concept), then streaming via ReadableStream
- Session must stay warm (navigate at gateway startup, keep alive)
- Rate limits still apply per account
- No reverse engineering needed — completely future-proof
