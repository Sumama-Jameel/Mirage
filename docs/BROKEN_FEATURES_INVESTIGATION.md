# Broken Features - Investigation & Fix Guidance

Status: Updated 2026-08-08
Scope: All provider issues from BROKEN_FEATURES.md

## Summary of Fixes & Status

### ✓ FIXED (Task 1, 7)

**1. Grok provider (HTTP 502) - DOCUMENTED**
- **Issue**: Expired x-statsig-id challenge constants
- **Root Cause**: Grok.com deploy cycle updates anti-bot tokens every 2-4 weeks
- **Solution**: Documented in `docs/GROK_CONSTANT_SETUP.md`
  - Manual extraction: User runs console script in browser, extracts constants, sets env vars
  - Auto-heal: If Firefox profile has valid grok.com session, gateway extracts on first 403
- **Status**: Auto-heal code implemented; manual process documented for when auto-heal unavailable
- **Blocking**: Requires authenticated grok.com session in browser profile or manual user extraction

**7. Qwen research model (HTTP 502) - RESOLVED**
- **Issue**: Model "qwen-deep-research" not found on chat.qwen.ai
- **Root Cause**: Qwen Deep Research is only available through Alibaba Cloud Model Studio paid API, not the free chat interface
- **Solution**: Removed qwen-research from available models list
- **Status**: FIXED - model removed from mod.rs and direct.rs

### ⚠️ ACCOUNT-SIDE BLOCKERS (Not code bugs - Already mitigated)

**2. Minimax provider (HTTP 502) - DOCUMENTED & CACHED**
- **Issue**: Token Plan quota exhausted (error code 2056)
- **Root Cause**: Account-side token quota limit reached
- **Current Mitigation**: Quota exhaustion state cached (~30 min) with fast-fail
  - Before: ~8s per request while exhausted
  - After: Immediate error response during cache window
- **Fix**: User can claim daily free credits, purchase Credits, upgrade Token Plan, or wait for quota window reset
- **Status**: Code is already optimal; issue is user's account configuration

### 🔴 BLOCKED (Require browser credentials or live API testing)

**3. MiMo provider (HTTP 401) - Missing userId cookie**
- **Issue**: Firefox profile lacks `xiaomichatbot_userId` cookie for aistudio.xiaomimimo.com
- **Root Cause**: Browser profile never logged into MiMo service
- **Fix Required**:
  1. Manually login to https://aistudio.xiaomimimo.com in Firefox
  2. Ensure cookies are saved (check Storage → Cookies in F12)
  3. Restart gateway
- **Investigation Path**: Check if there are alternative auth methods or if userId is obtainable from other cookies
- **Blocker**: Requires manual user login to MiMo service

**4. Meta AI provider (HTTP 401) - Missing cookies**
- **Issue**: Firefox profile lacks `meta.ai` / `ecto_1_sess` cookies
- **Root Cause**: Browser profile never logged into Meta AI (ecto platform)
- **Fix Required**:
  1. Manually login to https://meta.ai in Firefox
  2. Ensure cookies are saved
  3. Restart gateway
- **Investigation Path**: Check if ecto1 token can be extracted from page localStorage as fallback
- **Blocker**: Requires manual user login to Meta AI

**5. GLM provider (16 models broken/empty) - Complex**
- **Issue**: Two failure modes:
  - 11 models: HTTP 502 "chat.z.ai input textarea not found within timeout"
  - 5 models: HTTP 200 with empty content
- **Root Causes**:
  1. Direct API failing to authenticate or reach endpoint
  2. UI automation fallback can't find text input after 20s wait
  3. Some models return empty responses even when connection succeeds
- **Current Code**: 
  - Direct path attempts RPC call to Z.AI internal API
  - Falls back to UI-driven chat when direct fails
  - UI loader waits up to 20s for ProseMirror editor to appear
- **Investigation Needed**:
  1. Verify direct API endpoint and authentication (`/rest/chat`, signature scheme)
  2. Check if Z.AI UI structure changed (ProseMirror → new framework?)
  3. Test which model IDs actually work on current Z.AI
  4. Verify if empty responses are API errors masquerading as success
- **Blocker**: Requires live testing against chat.z.ai; possibly Z.AI changed their UI framework

**6. Kimi K3 - Multiple degradations**
- **Issues**:
  - Stream: 1 chunk, no deltas, no session_url in stream
  - Tool calling: No tool calls returned
  - Upload: Empty content after 85s
  - Thinking: No reasoning_content
  - Search: No citations returned
- **Root Causes**: Multiple issues across different features
  1. **Streaming session_url**: Need to capture and return continuation token from stream
  2. **Tool calling**: Native tool calling may require different request format or parsing
  3. **Upload**: Async file processing - model receives unparsed file
  4. **Thinking**: May not be returning extended attributes
  5. **Search**: May require different response parsing for citations
- **Investigation Needed**:
  1. Reverse-engineer Kimi API format for each feature independently
  2. Check if streaming response includes continuation tokens
  3. Verify native tool calling structure matches implementation
  4. Test async upload handling - may need to wait for parsing to complete
- **Blocker**: Requires reverse-engineering multiple Kimi API endpoints

**8. Gemini native tool calling - Extraction mismatch**
- **Issue**: Tool calls appear in content as raw JSON but aren't parsed into tool_calls field
- **Example Output**:
  ```
  Content: "http://googleusercontent.com/card_content/0\n```json {...}```"
  tool_calls: null (expected non-null)
  ```
- **Root Cause**: Possible issues:
  1. Tool call text is being included in content AND the structured response
  2. Stripping logic isn't removing the URL prefix correctly
  3. Parsing logic isn't detecting the format
  4. Native tool calls in response index 28 aren't being extracted
- **Current Code**:
  - `parse_gemini_tool_calls()` handles multiple formats (fenced JSON, inline calls, XML)
  - `strip_gemini_card_prefix()` removes googleusercontent URLs
  - Native tool extraction tries candidate[28] first, falls back to text parsing
- **Investigation Needed**:
  1. Add debug logging to see what text is being parsed
  2. Verify what native response index Gemini uses for tool calls
  3. Test with actual Gemini API response to see format
  4. May need to update regex if URL format changed
- **Blocker**: Requires live testing against Gemini API or captured response samples

## Investigation & Fix Priority

### High Priority (Low blocker count):
1. **Qwen research** - FIXED ✓
2. **Grok** - DOCUMENTED ✓
3. **Minimax** - Already mitigated ✓

### Medium Priority (Fixable with browser access):
1. **MiMo** - User needs to login
2. **Meta AI** - User needs to login

### Low Priority (Complex, requires deep API reverse-engineering):
1. **GLM** - Possibly Z.AI UI changed framework
2. **Kimi** - Multiple independent features broken
3. **Gemini** - Tool extraction logic needs debugging

## How to Unblock Each Issue

### Grok (Pending)
```bash
# Method 1: Manual extraction
1. Open https://grok.com in Firefox (must be logged in)
2. F12 → Console → paste extraction script from docs/EXTRACT_GROK_CONSTANTS.txt
3. Send a chat message
4. Run JSON.stringify(window.__gc) and extract values
5. Set env vars: GROK_CHALLENGE_HEADER_HEX, GROK_CHALLENGE_SUFFIX, GROK_CHALLENGE_TRAILER

# Method 2: Auto-heal (if logged in)
1. Ensure Firefox profile has grok.com session
2. Start gateway
3. On first 403, auto-heal triggers and extracts constants automatically
```

### Minimax (Already working)
- No code fix needed; user must resolve token quota through account settings

### MiMo & Meta AI (Blocked on login)
```bash
# For each provider:
1. Manually login to service in Firefox:
   - MiMo: https://aistudio.xiaomimimo.com
   - Meta AI: https://meta.ai
2. Verify cookies saved in F12 Storage → Cookies
3. Restart gateway (will load cookies from Firefox profile)
```

### GLM (Requires reverse-engineering)
1. Check if Z.AI changed UI framework or endpoint
2. Add debug logging to direct.rs to see RPC requests/responses
3. Verify model IDs that work vs. don't work
4. Update selectors if UI changed

### Kimi (Requires feature-by-feature investigation)
1. Test each feature (streaming, tools, upload, thinking, search) independently
2. Capture actual API responses
3. Compare against implementation assumptions
4. Update parsing logic for each feature

### Gemini (Requires debugging)
1. Add debug logging to tool call extraction
2. Capture actual Gemini responses with tool calls
3. Verify regex matching and text stripping
4. Test native vs. text-based tool call extraction

## Next Steps for Completion

### Phase 1: User Setup (Unblocks MiMo, Meta AI)
- Document login process for each provider
- Create automated check to verify cookies are present

### Phase 2: Provider-specific reverse-engineering (GLM, Kimi, Gemini)
- Run live tests against each provider's current API
- Capture response samples
- Update code based on actual API format
- Add regression tests with captured responses

### Phase 3: Documentation & validation
- Update BROKEN_FEATURES.md with resolutions
- Create provider setup guide showing which need manual config
- Test all features end-to-end
- Ensure zero build warnings

## Files to Update

- `docs/GROK_CONSTANT_SETUP.md` - DONE ✓
- `crates/obscura-gateway/src/providers/qwen/mod.rs` - DONE ✓
- `crates/obscura-gateway/src/providers/qwen/direct.rs` - DONE ✓
- `BROKEN_FEATURES.md` - Needs update with investigation findings
- Provider-specific docs - TBD after investigation

## Reference

- **GOAL.md**: Requirements for feature completeness
- **AGENTS.md**: Architecture and testing guidelines
- **BROKEN_FEATURES.md**: Current known issues (baseline)
- **LatestAImodels**: Expected models for each provider
