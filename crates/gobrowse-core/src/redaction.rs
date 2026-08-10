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
}
