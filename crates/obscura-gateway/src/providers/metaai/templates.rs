//! Base64-encoded protobuf templates captured from the meta.ai web client.
//!
//! These are the exact `HOME_TEMPLATE` / `CHAT_TEMPLATE` constants shipped in
//! the Ecto-era `muse-spark-web-main.ts` bundle (v.ecto-era). The prompt frame
//! parser decodes one of them, mutates specific field paths (conversation id,
//! prompt text, timestamps, message ids), and re-serializes before sending it
//! over the DGW WebSocket.
//!
//! The home template seeds a brand-new conversation; the chat template is used
//! when continuing an existing conversation id (`isNewConversation=false`).
//! Both were VERIFIED against live meta.ai WS captures from two independent
//! accounts (2026-07-19). The 64-hex session token, actor id, locale and app
//! id inside the templates are app-level constants sent by Meta's own client,
//! not per-user secrets, so they are safe to embed.

/// Template for a new conversation (`KADABRA__HOME__UNIFIED_INPUT_BAR`).
pub const HOME_TEMPLATE_B64: &str =
    "CpkGCqEDCiBLQURBQlJBX19IT01FX19VTklGSUVEX0lOUFVUX0JBUhIQMTUyMjc2Mzg1NTQ3MjU0MyInNWE1Yi04ZDRlLWYwNTQtOTllZi1iMmRlLWRiMDItMGQwNS01MmM3KigqJgokNDExYzRhNzAtOTRkMy00Nzc4LTg5YzYtMjE2MTBiNThjZDk2MAU6C0hVTUFOX0FHRU5UQiIKDzg2NzA1MTMxNDc2NzY5NhIPODY3MDUxMzE0NzY3Njk2UgVFQ1RPMVoRQWJyYSBXZWIgTWFpbiBLZXliGBoSCOkHEg1tb2RlX3RoaW5raW5nIgIIAWoFTGludXhyCnVzZXJfaW5wdXR6Rk1vemlsbGEvNS4wIChYMTE7IExpbnV4IHg4Nl82NDsgcnY6MTQwLjApIEdlY2tvLzIwMTAwMTAxIEZpcmVmb3gvMTQwLjCCAQtkZXNrdG9wX3dlYpoBRwpAZTJiODhmOTg0NjM3OWNiYzI2OTYwZmEzYWUxZDIyMjAxZGZiMTlkZjc4OTBhZTZhM2FjOGEyODg3MGJhYzY4MhUAAIA/EhQIl5H86er8oQIQl5H86er8oQIYAhoCIAEiACoOCIDfuJD/Mxjz3LiQ/zMyJDM2MGJlNGM1LWRkYWItNGY2ZC05NzA1LTg5MGM0YmE1MDg5YjoEYAFoAUoHEgVlbi1VU1JyCiQ2MzE5MmMxYy05NjYzLTQ5ODgtYTRiMy1iZDVhM2Y1ZDNiMjYaJDM2MTQyNzg3LWU4YWYtNDc0My1hNGI3LTk0ZjUzM2UzMzFiOSIkNDExYzRhNzAtOTRkMy00Nzc4LTg5YzYtMjE2MTBiNThjZDk2ehAiDkFmcmljYS9BYmlkamFuggEDsAEBkgEMCgZzdG9ja3MSAggBkgENCgd3ZWF0aGVyEgIIAZIBJAoebWV0YV9rbm93bGVkZ2Vfc2VhcmNoX2Nhcm91c2VsEgIIAZIBIgocbWV0YV9jYXRhbG9nX3NlYXJjaF9jYXJvdXNlbBICCAGSARMKDW1lZGlhX2dhbGxlcnkSAggBogEBA9ABABJsCmEKJDFlMGVkYmNjLWY4NDEtNGRlYy1iYzRjLTE1YTM4YmZlNTE4YxI3CiQ0MTFjNGE3MC05NGQzLTQ3NzgtODljNi0yMTYxMGI1OGNkOTYQgd+4kP8zGJ/W0YG78aD+ZygBEgJ5byIDCgEw";

/// Template for continuing an existing conversation (`KADABRA__CHAT__UNIFIED_INPUT_BAR`).
pub const CHAT_TEMPLATE_B64: &str =
    "CrIGCsADCiBLQURBQlJBX19DSEFUX19VTklGSUVEX0lOUFVUX0JBUhIQMTUyMjc2Mzg1NTQ3MjU0MyInNWE1Yi04ZDRlLWYwNTQtOTllZi1iMmRlLWRiMDItMGQwNS01MmM3KigqJgokYjA4Mzg1YTYtNWE1My00ZjE0LTk2NmUtMzQ3ZjI4MDg4NDU0MAU6C0hVTUFOX0FHRU5UQiIKDzg2NzA1MTMxNDc2NzY5NhIPODY3MDUxMzE0NzY3Njk2UgVFQ1RPMVoRQWJyYSBXZWIgTWFpbiBLZXliBRoDCOgHaghNYWMgT1MgWHIKdXNlcl9pbnB1dHp1TW96aWxsYS81LjAgKE1hY2ludG9zaDsgSW50ZWwgTWFjIE9TIFggMTBfMTVfNykgQXBwbGVXZWJLaXQvNTM3LjM2IChLSFRNTCwgbGlrZSBHZWNrbykgQ2hyb21lLzE0Ni4wLjAuMCBTYWZhcmkvNTM3LjM2ggELZGVza3RvcF93ZWKaAUcKQGUyYjg4Zjk4NDYzNzljYmMyNjk2MGZhM2FlMWQyMjIwMWRmYjE5ZGY3ODkwYWU2YTNhYzhhMjg4NzBiYWM2ODIVAAAAQBIUCLjDpdOTj/IBELjDpdOTj/IBGAIaAiABIgAqDgikgvuW2TMYoYL7ltkzMiRjNmI1ZDI2MS02NjI0LTQ5YWYtOTBjNy0wOWI0NWMwYTZiZWY6AEoHEgVlbi1VU1JyCiQxZDNjZGQzYy1jYTFhLTRlMDItODk1My1kZTBiYTM0NzI5ODkaJDcxODNhMzM0LTFiNWEtNGQyNi1iMjcxLWJjY2Y1NDY2NmJiZiIkYjA4Mzg1YTYtNWE1My00ZjE0LTk2NmUtMzQ3ZjI4MDg4NDU0ehEiD0FtZXJpY2EvQ2hpY2Fnb4IBA7ABAZIBDAoGc3RvY2tzEgIIAZIBDQoHd2VhdGhlchICCAGSASQKHm1ldGFfa25vd2xlZGdlX3NlYXJjaF9jYXJvdXNlbBICCAGSASIKHG1ldGFfY2F0YWxvZ19zZWFyY2hfY2Fyb3VzZWwSAggBkgETCg1tZWRpYV9nYWxsZXJ5EgIIAaIBAQMSlgEKfAokMTc4MDVmYjEtOTY3Zi00YmYyLTlmMjctOWRhYmRhMzYyMTJkEjcKJGIwODM4NWE2LTVhNTMtNGYxNC05NjZlLTM0N2YyODA4ODQ1NBCkgvuW2TMYxN23xoT2rbJnIhtlLjAwcHlKMUtxa3BHTmg5Sk9oWElNdnJRWlYSEWZvbGxvdyB1cCBwcm9iZSAyIgMKATI=";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_valid_base64_and_decode() {
        use base64::Engine;
        for (name, template) in [
            ("home", HOME_TEMPLATE_B64),
            ("chat", CHAT_TEMPLATE_B64),
        ] {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(template)
                .unwrap_or_else(|e| panic!("{name} template is not valid base64: {e}"));
            assert!(bytes.len() > 200, "{name} template suspiciously small");
        }
    }

    #[test]
    fn templates_differ() {
        assert_ne!(HOME_TEMPLATE_B64, CHAT_TEMPLATE_B64);
    }
}
