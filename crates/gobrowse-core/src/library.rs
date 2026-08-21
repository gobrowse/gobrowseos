use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub const AUTOBIOGRAPHY_MAX_CHARS: usize = 99_999;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BookScope {
    Global,
    User,
    Profile,
    Workspace,
    Project,
    Conversation,
    Agent,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BookType {
    Note,
    Document,
    Conversation,
    Project,
    Autobiography,
    Summary,
    Instruction,
    Imported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Provenance {
    User,
    Conversation,
    Agent,
    File,
    Web,
    Mcp,
    Import,
    Skill,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TrustLevel {
    Verified,
    UserProvided,
    AgentInferred,
    External,
    Untrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SecurityClassification {
    Public,
    Internal,
    Confidential,
    Restricted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingStatus {
    Pending,
    Processing,
    Ready,
    Failed,
    Stale,
}

/// Registry role of a Book in the unified library. `None` (SQL NULL) means `Source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BookKind {
    Source,
    Skill,
    Mcp,
    Plugin,
    Autobiography,
}

/// Lifecycle state of a plugin, matching the `plugins.state` column vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Discovered,
    Staged,
    Installed,
    Enabled,
    Dormant,
    Active,
    Unhealthy,
    UpdateAvailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub title: String,
    pub body: String,
    pub book_type: BookType,
    /// Registry role; `None` (SQL NULL) means `Source`.
    #[serde(default)]
    pub kind: Option<BookKind>,
    pub scope: BookScope,
    pub tags: Vec<String>,
    pub provenance: Provenance,
    pub trust: TrustLevel,
    pub author: String,
    pub workspace_id: Option<Uuid>,
    pub conversation_id: Option<Uuid>,
    pub security_classification: SecurityClassification,
    pub embedding_status: EmbeddingStatus,
    pub metadata: serde_json::Value,
    pub revision: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BookValidationError {
    #[error("book title must not be blank")]
    EmptyTitle,
    #[error("book title exceeds 512 characters")]
    TitleTooLong,
    #[error("autobiography exceeds {AUTOBIOGRAPHY_MAX_CHARS} characters")]
    AutobiographyTooLong,
    #[error("workspace scope requires a workspace id")]
    MissingWorkspace,
    #[error("project scope requires a workspace id")]
    MissingProjectWorkspace,
    #[error("conversation scope requires a conversation id")]
    MissingConversation,
}

impl Book {
    pub fn validate(&self) -> Result<(), BookValidationError> {
        let title_chars = self.title.chars().count();
        if self.title.trim().is_empty() {
            return Err(BookValidationError::EmptyTitle);
        }
        if title_chars > 512 {
            return Err(BookValidationError::TitleTooLong);
        }
        if self.book_type == BookType::Autobiography
            && self.body.chars().count() > AUTOBIOGRAPHY_MAX_CHARS
        {
            return Err(BookValidationError::AutobiographyTooLong);
        }
        if self.scope == BookScope::Workspace && self.workspace_id.is_none() {
            return Err(BookValidationError::MissingWorkspace);
        }
        if self.scope == BookScope::Project && self.workspace_id.is_none() {
            return Err(BookValidationError::MissingProjectWorkspace);
        }
        if self.scope == BookScope::Conversation && self.conversation_id.is_none() {
            return Err(BookValidationError::MissingConversation);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookChunk {
    pub ordinal: usize,
    pub text: String,
    pub token_estimate: usize,
    pub source_start: usize,
    pub source_end: usize,
}

/// Chunks on Unicode scalar boundaries and retains overlapping source offsets.
pub fn chunk_text(body: &str, max_chars: usize, overlap_chars: usize) -> Vec<BookChunk> {
    assert!(max_chars > 0, "max_chars must be positive");
    assert!(
        overlap_chars < max_chars,
        "overlap must be smaller than chunk"
    );

    let chars: Vec<(usize, char)> = body.char_indices().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut char_start = 0;
    while char_start < chars.len() {
        let char_end = (char_start + max_chars).min(chars.len());
        let byte_start = chars[char_start].0;
        let byte_end = chars
            .get(char_end)
            .map_or(body.len(), |(offset, _)| *offset);
        let text = body[byte_start..byte_end].to_owned();
        chunks.push(BookChunk {
            ordinal: chunks.len(),
            token_estimate: text.chars().count().div_ceil(4),
            text,
            source_start: byte_start,
            source_end: byte_end,
        });
        if char_end == chars.len() {
            break;
        }
        char_start = char_end - overlap_chars;
    }
    chunks
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    Coding,
    Research,
    DataAnalysis,
    DocumentCreation,
    GeneralQA,
    ShellAutomation,
    Ecommerce,
    SystemAdministration,
}

/// Returns a static mapping of task classes to relevant capability keywords.
pub fn task_capability_map() -> std::collections::HashMap<TaskClass, Vec<&'static str>> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    map.insert(
        TaskClass::Coding,
        vec![
            "rust",
            "python",
            "javascript",
            "typescript",
            "go",
            "code",
            "debug",
            "test",
            "implement",
        ],
    );
    map.insert(
        TaskClass::Research,
        vec![
            "search",
            "find",
            "lookup",
            "investigate",
            "explore",
            "documentation",
            "docs",
        ],
    );
    map.insert(
        TaskClass::DataAnalysis,
        vec![
            "data",
            "csv",
            "json",
            "analyze",
            "chart",
            "graph",
            "statistics",
            "pandas",
        ],
    );
    map.insert(
        TaskClass::DocumentCreation,
        vec![
            "write", "document", "markdown", "report", "summary", "create",
        ],
    );
    map.insert(
        TaskClass::GeneralQA,
        vec!["what", "how", "why", "explain", "describe", "tell"],
    );
    map.insert(
        TaskClass::ShellAutomation,
        vec![
            "bash", "shell", "command", "script", "terminal", "execute", "run",
        ],
    );
    map.insert(
        TaskClass::Ecommerce,
        vec![
            "shop", "buy", "purchase", "cart", "checkout", "payment", "order",
        ],
    );
    map.insert(
        TaskClass::SystemAdministration,
        vec![
            "system", "config", "service", "daemon", "log", "monitor", "admin",
        ],
    );
    map
}

#[derive(Debug, Clone, Copy)]
pub struct RankingWeights {
    pub lexical: f32,
    pub semantic: f32,
    pub recency: f32,
    pub source: f32,
    pub workspace: f32,
    pub rrf_k: f32,
    // M23 additions:
    pub capability_match: f32,
    pub past_success: f32,
    pub token_cost: f32,
    pub permission_risk: f32,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            lexical: 1.0,
            semantic: 1.0,
            recency: 0.004,
            source: 0.003,
            workspace: 0.005,
            rrf_k: 60.0,
            // M23 defaults:
            capability_match: 0.05,
            past_success: 0.002,
            token_cost: -0.001,
            permission_risk: -0.003,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedBook {
    pub id: Uuid,
    pub score: f32,
}

/// Weighted reciprocal-rank fusion. Metadata boosts are bounded to `[0, 1]`.
pub fn rank_fusion(
    lexical: &[Uuid],
    semantic: &[Uuid],
    metadata_boosts: &HashMap<Uuid, (f32, f32, f32)>,
    weights: RankingWeights,
) -> Vec<RankedBook> {
    let mut scores = HashMap::<Uuid, f32>::new();
    for (rank, id) in lexical.iter().enumerate() {
        *scores.entry(*id).or_default() += weights.lexical / (weights.rrf_k + rank as f32 + 1.0);
    }
    for (rank, id) in semantic.iter().enumerate() {
        *scores.entry(*id).or_default() += weights.semantic / (weights.rrf_k + rank as f32 + 1.0);
    }
    for (id, score) in &mut scores {
        if let Some((recency, source, workspace)) = metadata_boosts.get(id) {
            *score += weights.recency * recency.clamp(0.0, 1.0)
                + weights.source * source.clamp(0.0, 1.0)
                + weights.workspace * workspace.clamp(0.0, 1.0);
        }
    }
    let mut ranked: Vec<_> = scores
        .into_iter()
        .map(|(id, score)| RankedBook { id, score })
        .collect();
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(overrides: impl FnOnce(&mut Book)) -> Book {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut b = Book {
            id: Uuid::nil(),
            profile_id: Uuid::nil(),
            title: "Valid Title".into(),
            body: String::new(),
            book_type: BookType::Note,
            kind: None,
            scope: BookScope::Global,
            tags: vec![],
            provenance: Provenance::User,
            trust: TrustLevel::UserProvided,
            author: "owner".into(),
            workspace_id: None,
            conversation_id: None,
            security_classification: SecurityClassification::Public,
            embedding_status: EmbeddingStatus::Pending,
            metadata: serde_json::json!({}),
            revision: 1,
            created_at: now,
            updated_at: now,
        };
        overrides(&mut b);
        b
    }

    fn autobiography(body: String) -> Book {
        book(|b| {
            b.title = "Autobiography".into();
            b.body = body;
            b.book_type = BookType::Autobiography;
            b.scope = BookScope::Profile;
            b.security_classification = SecurityClassification::Confidential;
        })
    }

    #[test]
    fn autobiography_limit_counts_unicode_characters() {
        assert!(
            autobiography("🦀".repeat(AUTOBIOGRAPHY_MAX_CHARS))
                .validate()
                .is_ok()
        );
        assert_eq!(
            autobiography("🦀".repeat(AUTOBIOGRAPHY_MAX_CHARS + 1)).validate(),
            Err(BookValidationError::AutobiographyTooLong)
        );
    }

    #[test]
    fn book_validation_enforces_all_scope_and_title_invariants() {
        // EmptyTitle: blank
        assert_eq!(
            book(|b| b.title = String::new()).validate(),
            Err(BookValidationError::EmptyTitle)
        );
        // EmptyTitle: whitespace-only
        assert_eq!(
            book(|b| b.title = "   ".into()).validate(),
            Err(BookValidationError::EmptyTitle)
        );
        // TitleTooLong
        assert_eq!(
            book(|b| b.title = "x".repeat(513)).validate(),
            Err(BookValidationError::TitleTooLong)
        );
        // MissingWorkspace
        assert_eq!(
            book(|b| b.scope = BookScope::Workspace).validate(),
            Err(BookValidationError::MissingWorkspace)
        );
        // MissingProjectWorkspace
        assert_eq!(
            book(|b| b.scope = BookScope::Project).validate(),
            Err(BookValidationError::MissingProjectWorkspace)
        );
        // MissingConversation
        assert_eq!(
            book(|b| b.scope = BookScope::Conversation).validate(),
            Err(BookValidationError::MissingConversation)
        );
        // Workspace scope with workspace_id → Ok
        assert!(
            book(|b| {
                b.scope = BookScope::Workspace;
                b.workspace_id = Some(Uuid::new_v4());
            })
            .validate()
            .is_ok()
        );
        // Conversation scope with conversation_id → Ok
        assert!(
            book(|b| {
                b.scope = BookScope::Conversation;
                b.conversation_id = Some(Uuid::new_v4());
            })
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn book_kind_serde_matches_db_vocabulary_and_legacy_json_defaults_to_none() {
        for (kind, db) in [
            (BookKind::Source, "SOURCE"),
            (BookKind::Skill, "SKILL"),
            (BookKind::Mcp, "MCP"),
            (BookKind::Plugin, "PLUGIN"),
            (BookKind::Autobiography, "AUTOBIOGRAPHY"),
        ] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{db}\""),
                "BookKind::{kind:?} must match the books.kind CHECK vocabulary"
            );
            assert_eq!(
                serde_json::from_str::<BookKind>(&format!("\"{db}\"")).unwrap(),
                kind
            );
        }
        for (state, db) in [
            (PluginState::Discovered, "discovered"),
            (PluginState::Staged, "staged"),
            (PluginState::Installed, "installed"),
            (PluginState::Enabled, "enabled"),
            (PluginState::Dormant, "dormant"),
            (PluginState::Active, "active"),
            (PluginState::Unhealthy, "unhealthy"),
            (PluginState::UpdateAvailable, "update_available"),
        ] {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{db}\""),
                "PluginState::{state:?} must match the plugins.state CHECK vocabulary"
            );
        }

        // Current Book JSON always carries `kind`; legacy JSON without it parses as None.
        let mut value = serde_json::to_value(book(|b| b.kind = Some(BookKind::Skill))).unwrap();
        assert_eq!(
            value.get("kind").and_then(serde_json::Value::as_str),
            Some("SKILL")
        );
        value.as_object_mut().unwrap().remove("kind");
        let parsed: Book = serde_json::from_value(value).unwrap();
        assert_eq!(parsed.kind, None);
    }

    #[test]
    fn chunking_preserves_unicode_and_overlap() {
        let chunks = chunk_text("a🦀bcdef", 4, 1);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "a🦀bc");
        assert_eq!(chunks[1].text, "cdef");
        assert_eq!(
            &"a🦀bcdef"[chunks[1].source_start..chunks[1].source_end],
            "cdef"
        );
    }

    #[test]
    fn chunk_text_handles_empty_and_single_chunk() {
        // Empty body → empty vec
        let chunks = chunk_text("", 64, 8);
        assert!(chunks.is_empty());

        // Single chunk: text shorter than max_chars
        let chunks = chunk_text("hello world", 64, 8);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert_eq!(chunks[0].ordinal, 0);
        assert_eq!(chunks[0].source_start, 0);
        assert_eq!(chunks[0].source_end, "hello world".len());
    }

    #[test]
    fn fusion_rewards_presence_in_both_lists() {
        let shared = Uuid::new_v4();
        let lexical_only = Uuid::new_v4();
        let semantic_only = Uuid::new_v4();
        let ranked = rank_fusion(
            &[lexical_only, shared],
            &[semantic_only, shared],
            &HashMap::new(),
            RankingWeights::default(),
        );
        assert_eq!(ranked[0].id, shared);
    }

    #[test]
    fn rank_fusion_clamps_out_of_range_boosts_and_breaks_ties_by_id() {
        let id_a = Uuid::nil();
        let id_b = Uuid::from_u128(1);
        // Same lexical rank → tie; boost one with out-of-range values
        let mut boosts = HashMap::new();
        // recency 2.0 should be clamped to 1.0, source -1.0 clamped to 0.0,
        // workspace 0.5 in-range
        boosts.insert(id_a, (2.0, -1.0, 0.5));
        boosts.insert(id_b, (0.0, 0.0, 0.0));
        let weights = RankingWeights::default();
        let ranked = rank_fusion(&[id_a, id_b], &[], &boosts, weights);
        // Both have same base score (same rank in lexical, none in semantic)
        // id_a gets clamped boost, so it should rank above id_b
        assert!(ranked[0].score > ranked[1].score);
        assert_eq!(ranked[0].id, id_a);

        // Tie-break by id: same score → lower UUID first
        let ranked = rank_fusion(&[id_a, id_b], &[], &HashMap::new(), weights);
        assert_eq!(ranked.len(), 2);
        // Same score from same lexical rank; tie breaks by id ascending
        assert_eq!(ranked[0].id, id_a); // nil UUID < uuid(1)
        assert_eq!(ranked[1].id, id_b);
    }
}
