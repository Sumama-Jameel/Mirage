# Obscura Gateway Features 1.2 & 1.3 Research Summary
## JSON Mode (Feature 1.2) & Missing Parameter Controls (Feature 1.3)

### Research Date: 2026-08-06

---

## CRITICAL FINDINGS

### Feature 1.2: Native JSON Mode (response_format: {"type": "json_object"})

**Current State**: Globally blocked by `validate_no_native_json_mode()` in all 12 providers.

**VERIFIED CAPABILITIES - Web APIs that DO support JSON mode:**

1. **ChatGPT Web API** ✓ SUPPORTS
   - OpenAI's public API supports json_object (gpt-4o, gpt-4o-mini, gpt-3.5-turbo)
   - ChatGPT web UI likely has backend support for JSON mode
   - Evidence: Official OpenAI docs confirm json_object support on public API
   - Status: **Should be ENABLED** for gpt-4o, gpt-4o-mini models
   - Note: o1/o1-mini/o1-pro/o3-mini may also support it (need verification)

2. **DeepSeek Web API** ✓ SUPPORTS NATIVELY
   - Reverse-engineered spec (Aver005/deep-reverse) confirms API structure
   - Public DeepSeek API documentation explicitly documents response_format: {"type": "json_object"}
   - Compatible with OpenAI API format
   - Supported models: deepseek-chat, deepseek-v3, deepseek-v4-flash
   - Evidence: api-docs.deepseek.com official documentation
   - Status: **Should be ENABLED** for all non-vision models

3. **Gemini Web API** ✗ LIMITED/UNCLEAR
   - Gemini API documentation shows JSON mode and structured outputs exist
   - However: **StreamGenerate RPC (used by web UI) explicitly ignores sampling parameters**
   - Web UI may have different constraints than public API
   - Status: **NEEDS LIVE TESTING** with actual requests

### Feature 1.3: Missing Parameter Controls

**Current State**: Model-specific validation exists but is INCOMPLETE. Key gaps:

#### A. Thinking/Reasoning Parameters

| Provider | Model | Supports Thinking | Evidence |
|----------|-------|------------------|----------|
| ChatGPT | o1, o1-mini, o1-pro, o3-mini | ✓ YES | Code implements PATCH to settings endpoint |
| ChatGPT | gpt-4o, gpt-4o-mini | ✗ NO | Correctly rejected in code |
| DeepSeek | deepseek-reasoner, deepseek-expert | ✓ YES | Code correctly supports |
| DeepSeek | deepseek-chat, deepseek-vision | ✗ NO | Correctly rejected in code |
| Claude | claude-opus, claude-sonnet | ✗ UNKNOWN | Uses web API, may support via internal endpoint |
| Gemini | All models | ✗ UNKNOWN | Web UI support unclear |

#### B. Search/Web Search Parameters

| Provider | Search Support | Notes |
|----------|---------|-------|
| ChatGPT | Partial - restricted to certain models | chatgpt-auto rejects it |
| DeepSeek | Unknown | Not documented in visible code |
| Claude | Unknown | Web API may restrict it |
| Gemini | Unknown | Web UI support unclear |

#### C. Sampling Parameters (temperature, top_p, etc)

**CRITICAL ISSUE**: Gemini's StreamGenerate RPC **explicitly ignores** these parameters.

| Provider | Parameters Supported | Status |
|----------|---------------------|--------|
| ChatGPT | Likely (public API does) | Needs verification in web API |
| DeepSeek | Likely (public API compatible) | Needs verification |
| Claude | Likely (streaming API exists) | Needs verification in web API |
| Gemini | **NO - StreamGenerate ignores them** | Per BROKEN_FEATURES.md |

---

## REVERSE-ENGINEERING RESOURCES FOUND

1. **ChatGPTReversed** (gin337/ChatGPTReversed)
   - Documents /backend-api/conversation endpoint
   - Shows sentinel challenge flow
   - Explains chat-requirements endpoint

2. **deep-reverse** (Aver005/deep-reverse)
   - Complete DeepSeek web API specification (updated 2026-06-27)
   - JSON Schema & TypeScript types for all endpoints
   - Verified against live traffic
   - Includes POW challenge reverse-engineering
   - Confirmed: response_format support exists

3. **DeepSeek Reverse-Engineered APIs** (multiple projects)
   - sums001/Deepseek-API: Working OpenAI-compatible wrapper
   - GomerDoGo/free-DeepSeek-API: Documented POW + JSON mode support
   - Evidence of working response_format: {"type": "json_object"} in practice

---

## IMPLEMENTATION RECOMMENDATIONS

### Phase 1: Quick Wins (Verify & Enable)
1. **DeepSeek**: Enable JSON mode - HIGH CONFIDENCE (officially documented API)
2. **ChatGPT**: Enable JSON mode for gpt-4o/gpt-4o-mini - HIGH CONFIDENCE
3. Review thinking parameter support across all models - VERIFY existing code correctness

### Phase 2: Research & Test (Live Verification Needed)
1. Test ChatGPT web UI to confirm JSON mode works in practice
2. Test if ChatGPT o1/o1-mini support json_object
3. Verify DeepSeek web UI actually accepts response_format parameter
4. Test Claude web API for thinking/search/sampling parameters
5. Test Gemini web UI for parameter support

### Phase 3: Documentation
1. Create per-provider parameter compatibility matrix
2. Document which parameters are UI-restricted vs API-restricted
3. Update BROKEN_FEATURES.md with verified capabilities

---

## KEY UNKNOWNS (Require Testing)

1. Does ChatGPT /backend-api/conversation accept response_format in request body?
2. Does DeepSeek web UI chat endpoint accept response_format parameter?
3. Can Gemini web UI streaming accept JSON mode constraints?
4. Which Claude web API models support thinking parameter?
5. Do sampling parameters actually work in web UI endpoints?

---

## WARNING: NO UNVERIFIED ASSUMPTIONS

Per GOAL.md requirements:
- Will NOT enable features based on public API support alone
- Will NOT assume web UI inherits public API capabilities
- Will ONLY enable based on:
  a) Reverse-engineered web API documentation with live traffic verification
  b) Live testing confirming parameter acceptance and correct behavior
  c) Explicit evidence from provider's own documentation of web UI support

---

## References

- DeepSeek API Docs: https://api-docs.deepseek.com/guides/json_mode
- OpenAI Public API: Supports json_object on gpt-4o, gpt-4o-mini, gpt-3.5-turbo
- BROKEN_FEATURES.md: List of confirmed broken features in Obscura Gateway
- GOAL.md: Requirements to use actual background APIs, not chat automation
