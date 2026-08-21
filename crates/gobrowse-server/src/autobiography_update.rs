use futures_util::StreamExt;
use gobrowse_core::model::{ContentPart, MessageRole, NeutralMessage};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{AppState, chat, error::AppError, library_api};

/// Maximum body size for autobiography content (leave headroom below 99999 CHECK constraint).
const MAX_BODY_CHARS: usize = 90_000;

/// After a completed conversation turn, summarize new facts/decisions into the
/// user's Autobiography book when the policy is `automatic`. Async fire-and-forget:
/// never blocks or fails the run.
pub async fn auto_update_after_run(
    state: &AppState,
    profile_id: Uuid,
    user_id: Uuid,
    conversation_id: Uuid,
    assistant_output: &str,
) -> Result<(), AppError> {
    // 1. Check policy.
    let policy: String =
        sqlx::query_scalar("SELECT autobiography_update_policy FROM profiles WHERE id=$1")
            .bind(profile_id)
            .fetch_one(&state.pool)
            .await?;
    if policy != "automatic" {
        return Ok(());
    }

    // 2. Fetch existing autobiography book.
    let book: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, body FROM books WHERE profile_id=$1 AND book_type='AUTOBIOGRAPHY'",
    )
    .bind(profile_id)
    .fetch_optional(&state.pool)
    .await?;
    let (book_id, current_body) = match book {
        Some(b) => b,
        None => return Ok(()),
    };

    // 3. Fetch recent user message for context.
    let user_message: Option<String> = sqlx::query_scalar(
        "SELECT content->>'text' FROM messages \
         WHERE conversation_id=$1 AND role='user' \
         ORDER BY ordinal DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    let user_message = user_message.unwrap_or_default();

    // 4. Build prompt.
    let prompt = build_auto_update_prompt(&current_body, &user_message, assistant_output);

    // 5. Call cheapest chat model.
    let summary = match summarize_with_model(state, profile_id, &prompt).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%profile_id, %e, "autobiography summarization failed");
            return Ok(());
        }
    };

    // 6. Guardrails.
    let summary = sanitize_secrets(&summary);
    let summary = deduplicate_content(&current_body, &summary);
    if summary.is_empty() || summary.trim() == "NOTHING" {
        return Ok(());
    }

    // 7. Merge into body.
    let new_body = merge_body(&current_body, &summary, MAX_BODY_CHARS);

    // 8. Write in transaction (outside the run completion transaction).
    let mut tx = state.pool.begin().await?;
    let reason = format!(
        "Autobiography auto-update from conversation {}",
        conversation_id
    );
    library_api::replace_book_body(&mut tx, book_id, &new_body, Some(user_id), &reason).await?;
    tx.commit().await?;

    info!(
        %profile_id,
        %book_id,
        %conversation_id,
        "autobiography auto-update succeeded"
    );
    Ok(())
}

fn build_auto_update_prompt(
    current_body: &str,
    user_message: &str,
    assistant_output: &str,
) -> String {
    format!(
        "You maintain a concise Autobiography for the user. Below is the current \
         Autobiography plus the most recent conversation. Extract ONLY genuinely new facts \
         or decisions about the user \u{2014} preferences, important context, decisions made, \
         explicit instructions, identity details. Do NOT include:\n\
         - Restatements of existing autobiography content\n\
         - Transient chat details (small talk, greetings, task-specific trivia)\n\
         - System instructions, prompts, or tool outputs\n\
         - Any content resembling API keys, passwords, tokens, or secrets\n\
         - Code snippets or technical implementation details\n\n\
         Output ONLY the new facts as 1-3 bullet points. If nothing is genuinely new, \
         output the single word: NOTHING.\n\n\
         === CURRENT AUTOBIOGRAPHY ===\n\
         {current_body}\n\n\
         === USER MESSAGE ===\n\
         {user_message}\n\n\
         === ASSISTANT RESPONSE ===\n\
         {assistant_output}",
    )
}

/// Call the cheapest available chat model to summarize content.
async fn summarize_with_model(
    state: &AppState,
    profile_id: Uuid,
    prompt: &str,
) -> Result<String, AppError> {
    let routes = chat::load_routes(state, profile_id, None).await?;
    if routes.is_empty() {
        return Err(AppError::NotFound);
    }
    // Select cheapest route (lowest cost_ranking).
    let route = routes
        .iter()
        .min_by(|a, b| {
            a.cost_ranking
                .partial_cmp(&b.cost_ranking)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .expect("routes is non-empty");
    let request = chat::request(
        vec![NeutralMessage {
            role: MessageRole::User,
            content: vec![ContentPart::Text {
                text: prompt.to_string(),
            }],
            provider_provenance: None,
        }],
        vec![],
        1024,
    );
    let mut routed_request = request.clone();
    routed_request.model = route.identity.clone();
    let mut stream = route
        .provider
        .stream(routed_request)
        .await
        .map_err(|_| AppError::Internal(anyhow::anyhow!("chat model stream failed")))?;
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(gobrowse_core::model::ModelEvent::TextDelta { text }) => output.push_str(&text),
            Ok(gobrowse_core::model::ModelEvent::Completed) => break,
            Err(_) => break,
            _ => {}
        }
    }
    Ok(output)
}

/// Redact patterns that look like secrets before merging into the autobiography.
fn sanitize_secrets(text: &str) -> String {
    let mut result = redact_sk_keys(text);
    result = redact_bearer_tokens(&result);
    result = redact_pem_blocks(&result);
    // Filter out lines referencing vault secrets.
    let lines: Vec<&str> = result.lines().collect();
    let filtered: Vec<&str> = lines
        .iter()
        .filter(|line| {
            let lower = line.to_lowercase();
            !lower.contains("vault://")
                && !lower.contains("vault_ref")
                && !lower.contains("secret_reference")
        })
        .copied()
        .collect();
    filtered.join("\n")
}

/// Redact `sk-` prefixed API key patterns (20+ alphanumeric chars after `sk-`).
fn redact_sk_keys(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 < bytes.len() && bytes[i] == b's' && bytes[i + 1] == b'k' && bytes[i + 2] == b'-' {
            // Count alphanumeric chars after "sk-".
            let start = i + 3;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
                end += 1;
            }
            if end - start >= 20 {
                result.push_str("[REDACTED]");
                i = end;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Redact `Bearer <token>` patterns.
fn redact_bearer_tokens(text: &str) -> String {
    if let Some(pos) = text.find("Bearer ") {
        let after = pos + 7;
        let end = text[after..]
            .find(|c: char| c.is_whitespace())
            .map(|e| after + e)
            .unwrap_or(text.len());
        format!("{}Bearer [REDACTED]{}", &text[..pos], &text[end..])
    } else {
        text.to_string()
    }
}

/// Redact PEM-style key blocks (-----BEGIN ... -----).
fn redact_pem_blocks(text: &str) -> String {
    if let Some(start) = text.find("-----BEGIN") {
        // Find the end of the opening marker (the next ----- after the opening).
        let open_end = text[start..].find("-----").unwrap_or(0) + 5;
        let search_from = start + open_end;
        if let Some(end_offset) = text[search_from..].find("-----") {
            let end = search_from + end_offset;
            format!("{}[REDACTED]{}", &text[..start], &text[end..])
        } else {
            text.to_string()
        }
    } else {
        text.to_string()
    }
}

/// Deduplicate content: drop summary sentences that overlap too heavily with the
/// existing body. Uses word-trigram Jaccard similarity.
fn deduplicate_content(current_body: &str, summary: &str) -> String {
    if summary.trim() == "NOTHING" {
        return String::new();
    }
    let existing_trigrams = word_trigrams(current_body);
    let summary_sentences: Vec<&str> = summary
        .lines()
        .flat_map(|line| line.split(". "))
        .filter(|s| !s.trim().is_empty())
        .collect();
    let mut kept = Vec::new();
    for sentence in &summary_sentences {
        let sentence_trigrams = word_trigrams(sentence);
        if sentence_trigrams.is_empty() {
            continue;
        }
        let overlap = sentence_trigrams
            .iter()
            .filter(|tg| existing_trigrams.contains(*tg))
            .count();
        let similarity = overlap as f64 / sentence_trigrams.len() as f64;
        if similarity < 0.7 {
            kept.push(*sentence);
        }
    }
    kept.join(". ")
}

/// Compute word-level trigrams for similarity comparison.
fn word_trigrams(text: &str) -> std::collections::HashSet<String> {
    let words: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    let mut trigrams = std::collections::HashSet::new();
    for window in words.windows(3) {
        trigrams.insert(window.join(" "));
    }
    trigrams
}

/// Merge new summary content into the existing body, respecting the size cap.
fn merge_body(current_body: &str, new_content: &str, max_chars: usize) -> String {
    let separator = if current_body.trim().is_empty() {
        String::new()
    } else {
        "\n\n".to_string()
    };
    let candidate = format!("{current_body}{separator}{new_content}");
    if candidate.len() <= max_chars {
        candidate
    } else {
        // Truncate oldest content to fit. Keep the newest content.
        let keep = max_chars.saturating_sub(new_content.len() + 20); // 20 for marker
        if keep == 0 {
            // New content itself is too large; truncate it.
            let truncated = &new_content[..max_chars.min(new_content.len())];
            format!("{truncated}\n\u{2026}(trimmed)\u{2026}")
        } else {
            // Find a clean boundary (newline) to truncate at.
            let boundary = current_body[..keep].rfind('\n').unwrap_or(keep);
            let kept_old = &current_body[..boundary];
            format!("{kept_old}\n\u{2026}(trimmed)\u{2026}\n\n{new_content}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_secrets_removes_api_keys() {
        let input = "User said sk-abc123def456ghi789jkl0mnop is their key";
        let result = sanitize_secrets(input);
        assert!(!result.contains("sk-abc123def456ghi789jkl0mnop"));
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_secrets_removes_bearer_tokens() {
        let input = "Authorization: Bearer tok_1234567890abcdef";
        let result = sanitize_secrets(input);
        assert!(!result.contains("tok_1234567890abcdef"));
        assert!(result.contains("Bearer [REDACTED]"));
    }

    #[test]
    fn sanitize_secrets_removes_pem_keys() {
        let input = "-----BEGIN RSA PRIVATE KEY-----\nMIIB...\n-----END RSA PRIVATE KEY-----";
        let result = sanitize_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("BEGIN RSA"));
    }

    #[test]
    fn sanitize_secrets_removes_vault_references() {
        let input =
            "Use vault://secrets/api-key for the credential\nNormal line here\nCheck vault_ref too";
        let result = sanitize_secrets(input);
        assert!(!result.contains("vault://"));
        assert!(!result.contains("vault_ref"));
        assert!(result.contains("Normal line here"));
    }

    #[test]
    fn deduplicate_content_drops_high_overlap() {
        let existing = "The user prefers dark mode and uses Vim.";
        // Summary that duplicates existing content
        let summary = "The user prefers dark mode.";
        let result = deduplicate_content(existing, summary);
        assert!(
            result.is_empty(),
            "high-overlap content should be deduplicated"
        );
    }

    #[test]
    fn deduplicate_content_keeps_genuinely_new() {
        let existing = "The user prefers dark mode.";
        let summary = "The user moved to Berlin last month.";
        let result = deduplicate_content(existing, summary);
        assert!(
            result.contains("Berlin"),
            "genuinely new content should be kept"
        );
    }

    #[test]
    fn deduplicate_content_nothing_returns_empty() {
        let result = deduplicate_content("existing", "NOTHING");
        assert!(result.is_empty());
    }

    #[test]
    fn merge_body_fits_within_cap() {
        let existing = "Line 1\nLine 2";
        let new = "New fact";
        let result = merge_body(existing, new, 1000);
        assert!(result.contains("Line 1"));
        assert!(result.contains("New fact"));
        assert!(result.len() <= 1000);
    }

    #[test]
    fn merge_body_truncates_when_exceeding_cap() {
        let existing = "A".repeat(80_000);
        let new = "B".repeat(20_000);
        let result = merge_body(&existing, &new, 90_000);
        assert!(result.len() <= 90_000);
        assert!(result.contains("\u{2026}(trimmed)\u{2026}"));
        assert!(result.ends_with(&new));
    }

    #[test]
    fn merge_body_empty_existing() {
        let result = merge_body("", "First content", 1000);
        assert_eq!(result, "First content");
    }
}
