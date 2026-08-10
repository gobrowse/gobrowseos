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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub title: String,
    pub body: String,
    pub book_type: BookType,
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

#[derive(Debug, Clone, Copy)]
pub struct RankingWeights {
    pub lexical: f32,
    pub semantic: f32,
    pub recency: f32,
    pub source: f32,
    pub workspace: f32,
    pub rrf_k: f32,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            lexical: 1.0,
            semantic: 1.0,
            recency: 0.12,
            source: 0.08,
            workspace: 0.15,
            rrf_k: 60.0,
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

    fn autobiography(body: String) -> Book {
        let now = OffsetDateTime::UNIX_EPOCH;
        Book {
            id: Uuid::nil(),
            profile_id: Uuid::nil(),
            title: "Autobiography".into(),
            body,
            book_type: BookType::Autobiography,
            scope: BookScope::Profile,
            tags: vec![],
            provenance: Provenance::User,
            trust: TrustLevel::UserProvided,
            author: "owner".into(),
            workspace_id: None,
            conversation_id: None,
            security_classification: SecurityClassification::Confidential,
            embedding_status: EmbeddingStatus::Pending,
            metadata: serde_json::json!({}),
            revision: 1,
            created_at: now,
            updated_at: now,
        }
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
}
