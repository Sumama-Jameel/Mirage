# Browgent Browser Import Architecture

## Overview

Browgent imports **authentication data** (cookies and localStorage) and **conversation history** (from LLM provider APIs) from the user's local browser. It does **not** import bookmarks, browsing history, saved passwords, credit cards, autofill data, extensions, themes, or browser settings.

---

## 1. What Does Browgent Import?

### A. Cookies (for authentication)

Imports cookies from the user's local Firefox/Chrome browser profiles to authenticate against LLM provider websites (DeepSeek, ChatGPT, Claude, Gemini, etc.).

**Key file:** `browgent/apps/browser_runtime/auth/system_browser_extractor.py`

- **Firefox cookies** — Read from `~/.mozilla/firefox-{profile}/cookies.sqlite` (line 258-278)
- **Chrome/Chromium cookies** — Read from `~/.config/google-chrome/Default/Cookies` or `~/.config/chromium/Default/Cookies` (line 331-369)
- Chrome cookies may be **encrypted** (v10/v11 format) and are decrypted using PBKDF2 with the `peanuts` passphrase (line 384-441) or Windows DPAPI / macOS Keychain (line 443-496)

### B. localStorage (for authentication persistence)

Imports localStorage entries from the Firefox profile to restore provider login sessions.

**Key file:** `browgent/apps/browser_runtime/auth/system_browser_extractor.py`

- **Firefox localStorage** — Read from `storage/default/https+++{host}/ls/data.sqlite` within the Firefox profile directory (lines 280-307)
- Chrome localStorage extraction is not supported (line 186 comments)

### C. Conversation History (from LLM provider APIs)

Fetches prior conversation history from provider API endpoints using the browser's session cookies for authentication.

**Key file:** `browgent/apps/browser_runtime/history_fetcher.py`

- DeepSeek conversation history from `https://chat.deepseek.com/api/v0/chat/history_messages` (line 119-121)
- ChatGPT conversation history from `https://chatgpt.com/backend-api/conversation/{id}` (line 123-126)
- Claude conversation history from `https://claude.ai/api/organizations/{org_id}/chat_conversations/{id}/completion` (line 127-130)
- Provider-specific response parsers for DeepSeek (lines 55-113), OpenAI-compatible (lines 129-149), and flat-format (lines 152-162)

### D. Conversation Title (metadata)

Extracts conversation titles from API responses for session naming:
- DeepSeek: `data.biz_data.chat_session.title` (line 145)
- ChatGPT: `title` (line 147)
- Claude: `name` (line 149)

---

## 2. From Which Browsers Does It Import?

### Firefox / Firefox ESR (primary)

| What | File | Line(s) |
|------|------|---------|
| Profile detection | `system_browser_extractor.py` | 229-256 |
| Cookie reading | `system_browser_extractor.py` | 258-278 |
| localStorage reading | `system_browser_extractor.py` | 280-307 |
| Firefox binary detection | `config.py` | 13-32 |
| Bundled Firefox binary | `.browsers/firefox-1522/firefox/` | (embedded browser for headless use) |

Firefox profile paths scanned:
- `~/.mozilla/firefox-esr/` (any directory ending in `default`)
- `~/.mozilla/firefox/` (any directory ending in `default`)

### Chrome / Chromium (secondary)

| What | File | Line(s) |
|------|------|---------|
| Profile detection | `system_browser_extractor.py` | 309-329 |
| Cookie reading | `system_browser_extractor.py` | 331-369 |
| Cookie decryption | `system_browser_extractor.py` | 371-496 |

Chrome profile paths scanned (Linux):
- `~/.config/google-chrome/Default/Cookies`
- `~/.config/chromium/Default/Cookies`

Also supports macOS and Windows paths (lines 311-318).

### NOT Imported From

- **Brave** — No Brave-specific paths or logic
- **Microsoft Edge** — No Edge-specific paths or logic (except detecting `Edg/` in user agent to exclude from Chromium detection, line 93 of `capability_detector.py`)
- **Safari** — No Safari support
- **Opera / Vivaldi** — No support

---

## 3. How Does It Import? (Tools/Libraries/APIs)

### Core Technologies

| Technology | Purpose | File Evidence |
|------------|---------|---------------|
| **Playwright** (`playwright>=1.49.0`) | Browser automation engine | `setup.py` line 10, `browser_lifecycle.py` line 68 |
| **aiosqlite** | Async SQLite access for reading browser cookie DBs | `system_browser_extractor.py` line 23 |
| **httpx** (`httpx>=0.28.0`) | Async HTTP client for provider API calls | `history_fetcher.py` line 304, `setup.py` line 11 |
| **cryptography** (`cryptography>=44.0.0`) | AES-CBC decryption of Chrome cookies | `system_browser_extractor.py` line 424-427, `setup.py` line 16 |
| **websockets** | CDP protocol WebSocket communication | `cdp_client.py` line 11 |
| **orjson** (`orjson>=3.10.0`) | Fast JSON parsing | `setup.py` line 15 |
| **pydantic** / **pydantic-settings** | Configuration management | `setup.py` lines 8-9 |
| **Chrome DevTools Protocol (CDP)** | Low-latency stream capture via Playwright's CDP sessions | `cdp_client.py`, `network_capture.py` |

### Import Flow Architecture

**Step 1: Browser Lifecycle Management** (`browser_lifecycle.py`)

```python
# Line 71-73: Uses Playwright's async_playwright
self._playwright = await async_playwright().start()
browser_type = browser_config.browser_type or 'chromium'
browser_launcher = getattr(self._playwright, browser_type)
```

Configurable via env vars:
- `BROWGENT_BROWSER_TYPE=chromium|firefox`
- `BROWGENT_BROWSER_HEADLESS=true|false`
- `BROWGENT_FIREFOX_EXECUTABLE_PATH=/path/to/firefox`

**Step 2: Session Creation** (`session.py` -> `_SessionHandle.start()`)

The auth flow is triggered automatically on session creation:
1. Navigate to provider URL (e.g., `https://chat.deepseek.com/`)
2. Run auth chain: pre-navigate cookies -> localStorage -> detect redirects -> interactive login fallback
3. Attach stream capture subsystems (DOM polling or CDP network capture)

**Step 3: Cookie/State Extraction** (`system_browser_extractor.py`)

```python
# Lines 140-177: copy_cookies_from_system_browser()
# 1. Try Firefox first (uses safe_copy + aiosqlite)
# 2. Fall back to Chrome (with encrypted cookie decryption)

async def copy_cookies_from_system_browser(self, context, provider):
    host = PROVIDER_HOSTS.get(provider, '')
    db_path = await self._find_firefox_profile()
    if db_path:
        cookies = await self._read_firefox_cookies(db_path, host)
        validated = self._validate_cookies(cookies, host)
        await context.add_cookies(validated)  # <-- Inject into Playwright context
```

The cookies are read from the system browser SQLite databases, filtered for validity, and injected into the Playwright BrowserContext — **no API keys are needed**.

**Step 4: Chrome Cookie Decryption** (`system_browser_extractor.py`)

```python
# Lines 384-441: decrypt_chrome_v10()
# Uses PBKDF2-HMAC-SHA1 with salt=b'saltysalt', 1 iteration, passphrase=b'peanuts'
# to derive a 16-byte AES key, then AES-CBC decrypts the cookie value
kdf = PBKDF2HMAC(algorithm=hashes.SHA1(), length=16, salt=b'saltysalt', iterations=1)
key = kdf.derive(b'peanuts')
cipher = Cipher(algorithms.AES(key), modes.CBC(iv))
```

**Step 5: State Persistence** (`cookie_store.py`)

```python
# Lines 84-89: capture_state() - captures from live Playwright session
async def capture_state(self, context, page, provider):
    cookies = await context.cookies()
    ls_raw = await page.evaluate('() => JSON.stringify(window.localStorage)')
    return {'cookies': cookies, 'local_storage': ls_dict, ...}
```

This state is saved as an **encrypted blob** to `storage/auth/{provider}.enc` using `cryptography`-based encryption (lines 127-158).

**Step 6: Conversation History Fetching** (`history_fetcher.py`)

```python
# Lines 253-279: Uses browser cookies to authenticate to provider APIs
async def _extract_cookies(self, browser_context, provider):
    cookies = await browser_context.cookies()
    return "; ".join([f"{c['name']}={c['value']}" for c in cookies])

# Lines 285-324: HTTPS request to provider's history API with those cookies
async def _fetch_and_parse(self, provider, endpoint_config, conversation_id, cookie_header):
    async with httpx.AsyncClient(timeout=30.0) as client:
        response = await client.get(url, headers={"Cookie": cookie_header, ...})
        data = response.json()
        return self._parse_response(data, provider)
```

---

## 4. How Does It Use the Imported Data?

### A. Authentication (primary use)

Imported cookies and localStorage are used to **establish and maintain authenticated sessions** with LLM providers (DeepSeek, ChatGPT, Claude, Gemini, Qwen, GLM, Kimi, Grok) via `AuthManager` in `browgent/apps/browser_runtime/auth/manager.py`.

**Resolution order** (lines 174-256 of `manager.py`):
1. Restore saved encrypted auth blob (cookies + localStorage from previous run)
2. Check if page already has an active session
3. **Extract cookies from system Firefox/Chrome profiles** (the browser import step)
4. Interactive login prompt (fallback)

Once injected, the Playwright-controlled browser has authenticated access to the provider, enabling Browgent to:
- Navigate to the provider's chat page
- Inject prompts into the textarea
- Capture streaming responses
- Fetch conversation history

### B. Prompt Enrichment with Conversation History

When resuming a session with a `conversation_id`, Browgent fetches the prior conversation history from the provider's API (using browser cookies for auth) and **injects it as context** for the LLM prompt.

**Key code in** `engine.py` (lines 733-747):
```python
if session.metadata.get('conversation_id'):
    fetch_result = await self._history_fetcher.fetch(browser_ctx, effective_provider,
                                                      session.metadata['conversation_id'])
    if fetch_result and fetch_result.messages:
        api_context = self._history_fetcher.format_as_context(fetch_result.messages)
        enriched = f'[Previous Conversation]\n{api_context}\n\n{enriched}'
```

### C. Session Title Resolution

The conversation title extracted from the provider API is used to **name the Browgent session** for the status dashboard and storage (engine.py lines 750-758).

### D. Stream Capture Configuration

The provider-specific `ProviderCaptureConfig` map at `browgent/apps/browser_runtime/capture_config.py` defines how to **capture streaming responses** from each provider (lines 28-120), including:
- URL patterns to intercept
- Content JSON paths for extraction
- CSS selectors for the textarea, submit button, output areas
- Noise patterns to filter

These configs are used by `js_capture.py` (DOM polling) and `network_capture.py` (CDP intercept) to capture LLM response streams.

### E. Auth State Persistence Across Restarts

The `CookieStore` (`cookie_store.py`) saves the authenticated browser state (cookies + localStorage) as an **encrypted blob** to disk. On subsequent runs, Browgent restores this state to avoid requiring re-login.

---

## Summary Table

| Aspect | Details |
|--------|---------|
| **Imported data** | Cookies, localStorage, conversation history (messages + titles) |
| **Source browsers** | Firefox/FF ESR (primary), Chrome/Chromium (secondary) |
| **Import method** | Read SQLite databases directly (`cookies.sqlite`, `data.sqlite`) using `aiosqlite`; Chrome cookies decrypted via PBKDF2+AES-CBC |
| **Tooling** | Playwright (browser automation), aiosqlite (DB reading), httpx (API calls), cryptography (cookie decryption), CDP (response capture) |
| **What is NOT imported** | Bookmarks, browsing history, saved passwords, credit cards, autofill data, extensions, themes, browser settings |
| **Data usage** | Authentication for LLM providers, prompt context enrichment, session naming |
| **Storage** | Encrypted auth blobs at `storage/auth/{provider}.enc`, session metadata at `storage/sessions/`, SQLite state at `storage/browgent.db` |
| **Browsers NOT supported** | Brave, Edge, Safari, Opera, Vivaldi |
| **Processes exclusively read** | Browser profiles are read-only (SQLite databases are copied to temp dirs before reading per `safe_copy()` in `profile_utils.py` line 46-51) |
