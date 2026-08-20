//! Adaptive Capability Router — task classification, capability ranking, model routing.
//!
//! This module implements the M23 Adaptive Capability Router which teaches Gobrowse
//! WHAT to load and use. It provides:
//! - Rule-based task classification with optional model fallback
//! - Capability ranking with task-aware signals
//! - Model selection based on task class
//! - Plain-language routing reasons for UI display

use std::collections::HashMap;
use uuid::Uuid;

use gobrowse_core::library::{TaskClass, RankingWeights, task_capability_map};
use gobrowse_core::model::ModelRoute;

/// A model-task route mapping from the `model_task_routes` table.
#[derive(Debug, Clone)]
pub struct ModelTaskRoute {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub task_class: TaskClass,
    pub preferred_model_id: String,
    pub position: i32,
}

/// Book usage statistics from the `book_usage_stats` table.
#[derive(Debug, Clone, Default)]
pub struct BookUsageStat {
    pub book_id: Uuid,
    pub total_searches: i64,
    pub total_loads: i64,
    pub total_tool_uses: i64,
    pub last_loaded_at: Option<time::OffsetDateTime>,
}

/// Book summary with extended ranking information for router use.
/// This is a wrapper around the library_api BookSummary with additional
/// routing-specific fields.
pub type RouterBookSummary = crate::library_api::BookSummary;

/// Classifies a user message into a task class using rule-based pattern matching.
///
/// The classification uses deterministic rules:
/// - Code fences/backticks → `Coding`
/// - Shell commands (`$ `, `> `, `curl`, `git`, `npm`, `cargo`) → `ShellAutomation`
/// - URLs + "summarize"/"research"/"find" → `Research`
/// - File paths + "create"/"write" → `DocumentCreation`
/// - "pay"/"buy"/"purchase"/"checkout" → `Ecommerce`
/// - Fallback → `GeneralQA`
///
/// Returns a tuple of (TaskClass, source_label) where source_label is "rule" or "model".
pub fn classify_task(text: &str) -> (TaskClass, &'static str) {
    let text_lower = text.to_lowercase();

    // Check for code fences/backticks (coding)
    if text.contains("```") || text.contains('`') {
        return (TaskClass::Coding, "rule");
    }

    // Check for coding keywords
    let coding_keywords = ["fn ", "func ", "def ", "class ", "function", "rust", "python", "javascript", "typescript", "code", "implement", "debug"];
    if coding_keywords.iter().any(|k| text_lower.contains(k)) {
        return (TaskClass::Coding, "rule");
    }

    // Check for shell commands (shell_automation)
    let shell_patterns = ["$ ", "> ", "curl ", "git ", "npm ", "cargo ", "bash", "sh ", "sudo "];
    if shell_patterns.iter().any(|p| text.contains(p)) {
        return (TaskClass::ShellAutomation, "rule");
    }

    // Check for URLs + research keywords (research)
    let has_url = text.contains("http://") || text.contains("https://") || text.contains("www.");
    let research_keywords = ["summarize", "research", "find", "explore", "investigate"];
    if has_url && research_keywords.iter().any(|k| text_lower.contains(k)) {
        return (TaskClass::Research, "rule");
    }

    // Check for file paths + create/write keywords (document_creation)
    let has_file_path = text.contains(".md") || text.contains(".txt") || text.contains(".rs") || text.contains("/") && text.contains("file");
    let create_keywords = ["create", "write", "document", "draft"];
    if has_file_path || create_keywords.iter().any(|k| text_lower.contains(k)) && text_lower.contains("file") {
        return (TaskClass::DocumentCreation, "rule");
    }

    // Check for ecommerce keywords
    let ecommerce_keywords = ["pay", "buy", "purchase", "checkout", "order", "cart"];
    if ecommerce_keywords.iter().any(|k| text_lower.contains(k)) {
        return (TaskClass::Ecommerce, "rule");
    }

    // Check for system administration keywords
    let admin_keywords = ["system", "config", "service", "daemon", "log", "monitor", "admin"];
    if admin_keywords.iter().any(|k| text_lower.contains(k)) {
        return (TaskClass::SystemAdministration, "rule");
    }

    // Check for data analysis keywords
    let data_keywords = ["data", "csv", "analyze", "chart", "graph", "statistics"];
    if data_keywords.iter().any(|k| text_lower.contains(k)) {
        return (TaskClass::DataAnalysis, "rule");
    }

    // Default fallback
    (TaskClass::GeneralQA, "rule")
}

/// Signals used to generate routing reasons.
/// Each field represents a signal that contributed to the routing decision.
#[derive(Default)]
pub struct RoutingSignals {
    pub capability_match: Option<Vec<String>>,
    pub semantic_relevance: Option<String>,
    pub trust: Option<String>,
    pub recency_days: Option<i64>,
    pub scope: Option<String>,
    pub token_cost_estimate: Option<usize>,
    pub permission_risk: Option<String>,
    pub past_success_rate: Option<f32>,
}

/// Generates a plain-language routing reason from the given signals.
///
/// The reason is assembled from signal-specific clauses joined with semicolons.
/// Each clause maps to a concrete, verifiable fact about the routing decision.
pub fn generate_routing_reason(
    _book: &RouterBookSummary,
    _task_class: Option<TaskClass>,
    signals: &RoutingSignals,
) -> String {
    let mut clauses = Vec::new();

    // Capability match
    if let Some(caps) = &signals.capability_match && !caps.is_empty() {
        clauses.push(format!("Capability match ({})", caps.join(", ")));
    }

    // Semantic relevance
    if let Some(ref relevance) = signals.semantic_relevance {
        clauses.push(format!("Semantic relevance ({relevance})"));
    }

    // Trust
    if let Some(ref trust) = signals.trust {
        match trust.as_str() {
            "VERIFIED" => clauses.push("high trust (VERIFIED)".into()),
            "USER_PROVIDED" => clauses.push("high trust (USER_PROVIDED)".into()),
            "AGENT_INFERRED" => clauses.push("moderate trust (AGENT_INFERRED)".into()),
            "EXTERNAL" => clauses.push("low trust (EXTERNAL)".into()),
            "UNTRUSTED" => clauses.push("low trust (UNTRUSTED)".into()),
            _ => {}
        }
    }

    // Recency
    if let Some(days) = signals.recency_days {
        if days < 7 {
            clauses.push(format!("recently updated ({}d ago)", days));
        } else if days > 90 {
            clauses.push(format!("stale ({}d)", days));
        }
    }

    // Scope
    if let Some(ref scope) = signals.scope {
        match scope.as_str() {
            "WORKSPACE" | "PROJECT" => clauses.push("workspace-linked".into()),
            "PROFILE" => clauses.push("profile-wide".into()),
            "GLOBAL" => clauses.push("global".into()),
            _ => {}
        }
    }

    // Token cost
    if let Some(cost) = signals.token_cost_estimate {
        if cost < 1000 {
            clauses.push(format!("low token cost (~{} tokens)", cost));
        } else if cost > 5000 {
            clauses.push(format!("high token cost (~{} tokens)", cost));
        }
    }

    // Permission risk
    if let Some(ref risk) = signals.permission_risk {
        clauses.push(risk.clone());
    }

    // Past success
    if let Some(rate) = signals.past_success_rate && rate > 0.0 {
        clauses.push(format!(
            "previously used successfully ({:.0}% success rate)",
            rate * 100.0
        ));
    }

    // If no clauses, provide a default reason
    if clauses.is_empty() {
        clauses.push("matched search query".into());
    }

    clauses.join("; ")
}

/// Ranks capabilities by extending the existing scoring with task-aware signals.
///
/// This function sorts the books in-place by an extended score that incorporates:
/// - Capability match (Jaccard similarity between book capabilities and task capabilities)
/// - Past success (from usage stats)
/// - Token cost proxy (based on book kind and estimated size)
/// - Permission risk (based on security classification and trust)
///
/// The weights for these signals come from the `RankingWeights` parameter.
pub fn rank_capabilities(
    books: &mut Vec<RouterBookSummary>,
    task_class: Option<TaskClass>,
    weights: &RankingWeights,
    usage_stats: &HashMap<Uuid, BookUsageStat>,
) {
    if books.is_empty() {
        return;
    }

    // Get task capabilities if task class is provided
    let task_capabilities: Vec<String> = task_class
        .as_ref()
        .and_then(|tc| task_capability_map().get(tc).map(|v| v.iter().map(|s| s.to_string()).collect()))
        .unwrap_or_default();

    // Compute extended scores for each book
    let mut scored_books: Vec<(usize, f32)> = books
        .iter()
        .enumerate()
        .map(|(idx, book)| {
            let mut score = book.relevance; // Start with existing relevance score

            // Capability match signal
            if !task_capabilities.is_empty() && !book.capabilities.is_empty() {
                let book_caps: std::collections::HashSet<&str> =
                    book.capabilities.iter().map(|s| s.as_str()).collect();
                let task_caps: std::collections::HashSet<&str> =
                    task_capabilities.iter().map(|s| s.as_str()).collect();

                let intersection = book_caps.intersection(&task_caps).count() as f32;
                let union = book_caps.union(&task_caps).count() as f32;

                if union > 0.0 {
                    let jaccard = intersection / union;
                    score += jaccard * weights.capability_match;
                }
            }

            // Past success signal
            if let Some(stats) = usage_stats.get(&book.id) {
                let total_attempts = stats.total_loads.max(1) as f32;
                let success_rate = stats.total_tool_uses as f32 / total_attempts;
                // Default to 0.5 for unseen books (neutral)
                let rate = if stats.total_loads > 0 {
                    success_rate
                } else {
                    0.5
                };
                score += rate * weights.past_success;
            }

            // Token cost proxy (negative weight)
            let token_cost_factor = match book.kind.as_deref() {
                Some("SKILL") => 0.5,  // Lower cost
                Some("SOURCE") | None => 1.0,  // Standard cost
                Some("PLUGIN") => 1.5,  // Higher cost
                Some("MCP") => 2.0,     // Highest cost (activation expensive)
                _ => 1.0,
            };
            score += token_cost_factor * weights.token_cost;

            // Permission risk (negative weight)
            let risk_factor = match (book.trust.as_str(), "INTERNAL") {
                // Simplified risk assessment
                ("UNTRUSTED", _) => -1.0,
                ("EXTERNAL", _) => -0.5,
                _ => 0.0,
            };
            score += risk_factor * weights.permission_risk;

            (idx, score)
        })
        .collect();

    // Sort by score descending
    scored_books.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Reorder books based on sorted scores
    let mut sorted_books = Vec::new();
    for (idx, _) in scored_books {
        sorted_books.push(books[idx].clone());
    }
    *books = sorted_books;
}

/// Selects a model for a given task class.
///
/// If the task has a configured route in `model_task_routes`, that preferred model
/// is used. Otherwise, the profile default (first model in the fallback chain) is used.
///
/// Returns a tuple of (model_id, routing_reason).
pub fn select_model_for_task(
    task_class: TaskClass,
    task_routes: &[ModelTaskRoute],
    fallback_chain: &[ModelRoute],
) -> (String, String) {
    // Check for task-specific route
    if let Some(route) = task_routes.iter().find(|r| r.task_class == task_class) {
        let reason = format!(
            "task '{}' prefers model '{}' (configured in model_task_routes); \
             meets capability requirements; escalation none.",
            format!("{:?}", task_class).to_lowercase(),
            route.preferred_model_id
        );
        return (route.preferred_model_id.clone(), reason);
    }

    // Fall back to default (first model in fallback chain)
    if let Some(default_model) = fallback_chain.first() {
        let model_id = default_model.identity.model.clone();
        let reason = format!(
            "no task-specific routing configured for '{}'; \
             using profile default (highest-priority enabled chat model).",
            format!("{:?}", task_class).to_lowercase()
        );
        (model_id, reason)
    } else {
        // No fallback available
        ("".into(), "no model available in fallback chain".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gobrowse_core::library::TaskClass;

    #[test]
    fn test_classify_task_coding() {
        let (class, source) = classify_task("Write a Rust function to parse JSON");
        assert_eq!(class, TaskClass::Coding);
        assert_eq!(source, "rule");
    }

    #[test]
    fn test_classify_task_research() {
        let (class, source) = classify_task("Summarize https://example.com/article");
        assert_eq!(class, TaskClass::Research);
        assert_eq!(source, "rule");
    }

    #[test]
    fn test_classify_task_general_qa() {
        let (class, source) = classify_task("What is the capital of France?");
        assert_eq!(class, TaskClass::GeneralQA);
        assert_eq!(source, "rule");
    }

    #[test]
    fn test_classify_task_shell_automation() {
        let (class, source) = classify_task("npm install express");
        assert_eq!(class, TaskClass::ShellAutomation);
        assert_eq!(source, "rule");
    }

    #[test]
    fn test_classify_task_ecommerce() {
        let (class, source) = classify_task("buy a domain name");
        assert_eq!(class, TaskClass::Ecommerce);
        assert_eq!(source, "rule");
    }

    #[test]
    fn test_generate_routing_reason() {
        use time::OffsetDateTime;

        let book = RouterBookSummary {
            id: Uuid::new_v4(),
            title: "Test Book".into(),
            snippet: "Test".into(),
            kind: Some("SKILL".into()),
            capabilities: vec!["rust".into(), "code".into()],
            book_type: "INSTRUCTION".into(),
            scope: "PROFILE".into(),
            tags: vec![],
            provenance: "USER".into(),
            trust: "USER_PROVIDED".into(),
            revision: 1,
            relevance: 0.8,
            retrieval_mode: "semantic".into(),
            lexical_score: None,
            semantic_score: Some(0.8),
            updated_at: OffsetDateTime::now_utc(),
        };

        let signals = RoutingSignals {
            capability_match: Some(vec!["rust".into(), "code".into()]),
            trust: Some("USER_PROVIDED".into()),
            scope: Some("PROFILE".into()),
            ..Default::default()
        };

        let reason = generate_routing_reason(&book, Some(TaskClass::Coding), &signals);
        assert!(reason.contains("Capability match"));
        assert!(reason.contains("high trust"));
    }
}
