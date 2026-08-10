use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionPolicy {
    Manual,
    #[default]
    Propose,
    Automatic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRevision {
    pub id: Uuid,
    pub skill_id: Uuid,
    pub revision: i64,
    pub content: String,
    pub author: String,
    pub reason: String,
    pub source_conversation_ids: Vec<Uuid>,
    pub created_at: OffsetDateTime,
    pub evaluation: Option<SkillEvaluation>,
    pub promoted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvaluation {
    pub deterministic_checks_passed: bool,
    pub attempts: u32,
    pub successful_attempts: u32,
    pub steps: u32,
    pub retries: u32,
    pub errors: u32,
    pub duration_ms: u64,
    pub user_corrections: u32,
}

impl SkillEvaluation {
    pub fn success_rate(&self) -> f64 {
        if self.attempts == 0 {
            0.0
        } else {
            f64::from(self.successful_attempts) / f64::from(self.attempts)
        }
    }

    pub fn can_auto_promote_over(&self, previous: &Self) -> bool {
        self.deterministic_checks_passed
            && self.success_rate() >= previous.success_rate()
            && self.errors <= previous.errors
            && self.user_corrections <= previous.user_corrections
    }
}
