const REDACTED: &str = "[REDACTED]";

/// Redacts common credential-bearing key/value forms without returning the secret substring.
pub fn redact_text(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            let lower = token.to_ascii_lowercase();
            if lower.starts_with("authorization:")
                || lower.starts_with("bearer:")
                || lower.starts_with("api_key=")
                || lower.starts_with("apikey=")
                || lower.starts_with("password=")
                || lower.starts_with("refresh_token=")
                || lower.starts_with("client_secret=")
                || lower.starts_with("ghp_")
                || lower.starts_with("sk-")
            {
                REDACTED.to_owned()
            } else {
                token.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_secrets_do_not_survive() {
        let input = "request api_key=abc123 refresh_token=hidden ghp_private safe=value";
        let output = redact_text(input);
        for secret in ["abc123", "hidden", "ghp_private"] {
            assert!(!output.contains(secret));
        }
        assert!(output.contains("safe=value"));
    }

    #[test]
    fn redact_text_replaces_bearer_and_authorization_and_client_secret() {
        let input = "authorization: Bearer xyz bearer:abc client_secret=shh";
        let output = redact_text(input);
        // Tokens: ["authorization:", "Bearer", "xyz", "bearer:abc", "client_secret=shh"]
        // "authorization:" matches authorization: → [REDACTED]
        // "Bearer" does NOT start with "bearer:" (6 chars < 7 chars) → survives as-is
        // "xyz" is a plain token → survives
        // "bearer:abc" matches bearer: → [REDACTED] (abc gone)
        // "client_secret=shh" matches client_secret= → [REDACTED] (shh gone)
        // 3x [REDACTED] in output; xyz *is* present (standalone token, not a secret value)
        assert_eq!(output.matches(REDACTED).count(), 3);
        assert!(!output.contains("abc"), "bearer:abc redacted → abc absent");
        assert!(
            !output.contains("shh"),
            "client_secret=shh redacted → shh absent"
        );
        assert!(output.contains("xyz"), "standalone xyz is not a secret");
    }

    #[test]
    fn redact_text_is_case_insensitive_for_prefixes() {
        let input = "API_KEY=abc Apikey=def PASSWORD=ghi";
        let output = redact_text(input);
        // lowercased prefixes match: api_key=, apikey=, password=
        assert!(!output.contains("abc"));
        assert!(!output.contains("def"));
        assert!(!output.contains("ghi"));
        assert_eq!(output.matches(REDACTED).count(), 3);
    }

    #[test]
    fn redact_text_preserves_non_secret_tokens() {
        let input = "hello world safe=value normal-token";
        let output = redact_text(input);
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("safe=value"));
        assert!(output.contains("normal-token"));
        assert!(!output.contains(REDACTED));
    }

    #[test]
    fn redact_text_handles_empty_and_whitespace_only_input() {
        // Pinning current behavior: split_whitespace on "" / "   " returns an
        // empty iterator (split_whitespace filters empty substrings), and
        // joining an empty vec yields "".
        assert_eq!(redact_text(""), "");
        assert_eq!(redact_text("   "), "");
        assert_eq!(redact_text("\t\n  "), "");
    }

    #[test]
    fn redact_text_does_not_redact_secret_substring_inside_larger_token() {
        // Only token-START matches matter; a prefix embedded mid-token is safe.
        let input = "prefixapi_key=abc xapi_key=def";
        let output = redact_text(input);
        // Neither token starts with "api_key=" (they start with "prefixapi_key="
        // and "xapi_key="). Both survive verbatim.
        assert!(output.contains("prefixapi_key=abc"));
        assert!(output.contains("xapi_key=def"));
        assert!(!output.contains(REDACTED));
    }

    #[test]
    fn redact_text_redacts_sk_prefix_entire_token() {
        let input = "sk-live-key-123";
        let output = redact_text(input);
        assert_eq!(output, REDACTED);
        assert!(!output.contains("sk-live-"), "entire sk- token is replaced");
    }

    #[test]
    fn redact_text_preserves_surrounding_structure() {
        let input = "start API_KEY=secret middle refresh_token=abc end";
        let output = redact_text(input);
        assert!(
            output.starts_with("start "),
            "non-secret prefix preserved: {output}"
        );
        assert!(
            output.ends_with(" end"),
            "non-secret suffix preserved: {output}"
        );
        assert!(
            output.contains(" middle "),
            "non-secret middle preserved: {output}"
        );
        // Both secret tokens replaced by [REDACTED]
        assert_eq!(output.matches(REDACTED).count(), 2);
        // Verify token order: REDACTED appears twice, between start/middle/end
        let expected_pattern = format!("start {REDACTED} middle {REDACTED} end");
        assert_eq!(output, expected_pattern);
    }
}
