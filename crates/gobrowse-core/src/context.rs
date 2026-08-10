use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    SystemPolicy,
    Profile,
    ActiveTask,
    RecentConversation,
    PinnedBook,
    LibraryRetrieval,
    Autobiography,
    Skill,
    Worktree,
    ToolSchema,
    CompactionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCandidate {
    pub source: ContextSource,
    pub stable_id: String,
    pub content: String,
    pub token_estimate: u32,
    pub priority: u16,
    pub required: bool,
    pub trust_label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltContext {
    pub selected: Vec<ContextCandidate>,
    pub omitted_ids: Vec<String>,
    pub used_tokens: u32,
    pub budget: u32,
}

/// Selects required candidates first, then highest-priority optional candidates without slicing
/// content. Callers compact oversized required content before invoking this function.
pub fn build_context(mut candidates: Vec<ContextCandidate>, budget: u32) -> BuiltContext {
    candidates.sort_by(|left, right| {
        right
            .required
            .cmp(&left.required)
            .then_with(|| right.priority.cmp(&left.priority))
            .then_with(|| left.stable_id.cmp(&right.stable_id))
    });
    let mut selected = Vec::new();
    let mut omitted_ids = Vec::new();
    let mut used_tokens = 0_u32;
    for candidate in candidates {
        let fits = used_tokens.saturating_add(candidate.token_estimate) <= budget;
        if fits || candidate.required {
            used_tokens = used_tokens.saturating_add(candidate.token_estimate);
            selected.push(candidate);
        } else {
            omitted_ids.push(candidate.stable_id);
        }
    }
    BuiltContext {
        selected,
        omitted_ids,
        used_tokens,
        budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, tokens: u32, priority: u16, required: bool) -> ContextCandidate {
        ContextCandidate {
            source: ContextSource::LibraryRetrieval,
            stable_id: id.into(),
            content: id.into(),
            token_estimate: tokens,
            priority,
            required,
            trust_label: "untrusted".into(),
        }
    }

    #[test]
    fn required_policy_is_never_displaced_by_retrieval() {
        let built = build_context(
            vec![
                candidate("retrieved", 90, 100, false),
                candidate("policy", 40, 1, true),
            ],
            100,
        );
        assert_eq!(built.selected[0].stable_id, "policy");
        assert_eq!(built.omitted_ids, vec!["retrieved"]);
    }

    #[test]
    fn selection_is_deterministic_for_equal_priorities() {
        let built = build_context(
            vec![candidate("b", 10, 1, false), candidate("a", 10, 1, false)],
            20,
        );
        assert_eq!(built.selected[0].stable_id, "a");
    }
}
