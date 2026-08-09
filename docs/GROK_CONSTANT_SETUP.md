# Grok Provider Setup: x-statsig-id Constants

## Problem

Grok.com uses an anti-bot mechanism based on Statsig's `x-statsig-id` challenge tokens. The gateway ships with hardcoded defaults that become stale when Grok deploys frontend updates (every few weeks).

When constants are stale, all Grok requests return `HTTP 403 Forbidden: "Request rejected by anti-bot rules"`.

## Solution: Extract Fresh Constants

### Method 1: Manual Browser Console Extraction (Recommended)

This is the most reliable method when constants have gone stale.

**Prerequisites:**
- Firefox or Chrome browser
- Active login session on https://grok.com (requires X Premium)
- Access to F12 Developer Console

**Steps:**

1. **Open grok.com in your browser:**
   ```
   https://grok.com
   ```
   Ensure you're logged in with an X Premium account.

2. **Open F12 → Console tab**

3. **Paste this extraction script:**
   ```javascript
   window.__gc={hi:null,tok:null};var _d=crypto.subtle.digest.bind(crypto.subtle);crypto.subtle.digest=async function(a,d){var s=new TextDecoder().decode(d);if(s.includes('!')&&(s.startsWith('GET!')||s.startsWith('POST!')))window.__gc.hi=s;return _d(a,d)};var _f=window.fetch.bind(window);window.fetch=function(u,i){var url=(typeof u=='string'?u:u?.url)||'';if(url.includes('/rest/app-chat/')&&i?.headers){var t=i.headers['x-statsig-id']||i.headers['X-Statsig-Id'];if(t)window.__gc.tok=t}return _f(u,i)};console.log('ready')
   ```

4. **Send a chat message** in Grok and wait for the response

5. **In the console, run:**
   ```javascript
   JSON.stringify(window.__gc)
   ```
   You'll see output like:
   ```json
   {"hi":"POST!/rest/app-chat/conversations/new!1234567890abcdef...","tok":"BASE64_TOKEN..."}
   ```

6. **Extract the values** and set environment variables before starting the gateway:
   
   In the browser console, run this conversion script:
   ```javascript
   (function(){
     var tok = window.__gc.tok;
     var raw = new Uint8Array([...atob(tok)].map(c=>c.charCodeAt(0)));
     var now = Math.floor(Date.now()/1000) - 1682924400;
     for (var k = 0; k < 256; k++) {
       var dec = new Uint8Array(raw);
       for (var i = 0; i < dec.length; i++) dec[i] ^= k;
       var counter = dec[49] | (dec[50]<<8) | (dec[51]<<16) | (dec[52]<<24);
       if (Math.abs(counter - now) < 10) {
         var headerHex = [...dec.slice(0,49)].map(b=>b.toString(16).padStart(2,'0')).join('');
         var trailer = dec[69];
         var suffix = window.__gc.hi.split('!').slice(2).join('!');
         suffix = suffix.replace(/^-?\d+/, '');
         console.log('GROK_CHALLENGE_HEADER_HEX=' + headerHex);
         console.log('GROK_CHALLENGE_SUFFIX=' + suffix);
         console.log('GROK_CHALLENGE_TRAILER=' + trailer);
         break;
       }
     }
   })();
   ```

7. **Set environment variables** with the extracted values:
   ```bash
   export GROK_CHALLENGE_HEADER_HEX="<hex_from_step_6>"
   export GROK_CHALLENGE_SUFFIX="<suffix_from_step_6>"
   export GROK_CHALLENGE_TRAILER="<trailer_from_step_6>"
   ```

8. **Start the gateway:**
   ```bash
   # With env vars set above
   cargo build --release
   ./target/release/obscura-gateway
   ```

### Method 2: Auto-Heal on First Request

If the browser session has valid grok.com cookies loaded from Firefox:

1. **Ensure Firefox profile is logged into grok.com:**
   - The gateway loads cookies from `~/.mozilla/firefox-esr/*/cookies.sqlite`
   - Make sure your Firefox profile has an active grok.com session

2. **Start the gateway normally:**
   ```bash
   cargo build --release
   ./target/release/obscura-gateway
   ```

3. **Make a request to any Grok model:**
   ```bash
   curl -s http://127.0.0.1:8080/v1/chat/completions \
     -H "Authorization: Bearer test-key-123" \
     -H "Content-Type: application/json" \
     -d '{
       "model": "grok-auto",
       "messages": [{"role": "user", "content": "hi"}]
     }' | jq .
   ```

4. **On first 403, auto-heal triggers:**
   - The gateway navigates the browser to grok.com
   - Injects interception scripts
   - Captures the `x-statsig-id` token from the page's own requests
   - Unscrambles and extracts the constants
   - Saves them to disk (if configured)
   - Retries the request automatically

**Note:** This requires an active grok.com login session in the Firefox profile. If it's not logged in, extraction will timeout and fail.

## Troubleshooting

### "Request rejected by anti-bot rules" (403)

**Symptom:** All Grok requests return HTTP 403 with anti-bot error

**Causes:**
- Constants are stale (most common)
- Browser profile not logged into grok.com
- grok.com deploy cycle updated the Statsig SDK

**Fixes:**
1. Check if env vars are set:
   ```bash
   echo $GROK_CHALLENGE_HEADER_HEX
   ```
2. If not set or empty, run Method 1 (manual extraction) above
3. If extraction auto-heal failed, check the gateway logs for "Grok challenge"

### "Timed out waiting for grok.com to generate x-statsig-id"

**Symptom:** Auto-heal extraction times out

**Causes:**
- Firefox profile not logged into grok.com
- Statsig SDK failed to load on the page
- Browser can't reach grok.com

**Fixes:**
1. Login to grok.com in your Firefox browser before starting the gateway
2. Or use Method 1 (manual extraction)
3. Check network connectivity

### "Your grok.com login session may have expired"

**Symptom:** 403 after auto-heal retry

**Fixes:**
1. Logout and re-login to grok.com in Firefox
2. Wait a few minutes for the session to establish
3. Re-run the gateway

## When Constants Go Stale

Grok updates their frontend periodically. When this happens:

1. You'll start seeing `HTTP 403` responses
2. The auto-heal mechanism will try to extract new constants
3. If auto-heal succeeds, everything continues normally
4. If auto-heal fails (not logged in), use Method 1 to manually extract

**Estimation:** Constants typically last 2-4 weeks between Grok frontend updates.

## Configuration

### Environment Variables

```bash
export GROK_CHALLENGE_HEADER_HEX="<98 hex chars>"
export GROK_CHALLENGE_SUFFIX="<suffix string>"
export GROK_CHALLENGE_TRAILER="<single digit>"
```

These take precedence over all other sources (disk, defaults, extraction).

### Data Directory Persistence

If configured with a data directory, extracted constants are automatically saved:
```bash
./target/release/obscura-gateway --data-dir /tmp/obscura-data
```

Constants are persisted to `/tmp/obscura-data/grok_challenge.json`.

## Reference

- Full technical details: See `docs/EXTRACT_GROK_CONSTANTS.txt`
- Implementation: See `crates/obscura-gateway/src/providers/grok/`
- Architecture notes: See `AGENTS.md` "Grok provider challenge constants" section
