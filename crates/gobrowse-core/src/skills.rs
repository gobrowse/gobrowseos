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
            && self.attempts > 0
            && self.success_rate() >= previous.success_rate()
            && self.errors <= previous.errors
            && self.user_corrections <= previous.user_corrections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval() -> SkillEvaluation {
        SkillEvaluation {
            deterministic_checks_passed: true,
            attempts: 0,
            successful_attempts: 0,
            steps: 0,
            retries: 0,
            errors: 0,
            duration_ms: 0,
            user_corrections: 0,
        }
    }

    // ── can_auto_promote_over ──────────────────────────────

    #[test]
    fn skill_auto_promote_rejects_failing_deterministic_checks() {
        let current = eval_with(|e| e.deterministic_checks_passed = false);
        // previous is worse in every dimension, but the boolean short-circuit wins
        let previous = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 1;
            e.errors = 9;
            e.user_corrections = 5;
        });
        assert!(!current.can_auto_promote_over(&previous));
    }

    #[test]
    fn skill_auto_promote_rejects_higher_error_or_lower_success_rate() {
        // same success_rate but current.errors > previous.errors
        let current = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 9;
            e.errors = 3;
            e.user_corrections = 0;
        });
        let previous = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 9;
            e.errors = 2;
            e.user_corrections = 0;
        });
        assert!(
            !current.can_auto_promote_over(&previous),
            "more errors blocks promotion"
        );

        // lower success_rate even though errors improved
        let current = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 8;
            e.errors = 1;
            e.user_corrections = 0;
        });
        let previous = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 9;
            e.errors = 2;
            e.user_corrections = 0;
        });
        assert!(
            !current.can_auto_promote_over(&previous),
            "lower success_rate blocks promotion even though errors improved"
        );
    }

    #[test]
    fn skill_auto_promote_rejects_zero_attempts_successor() {
        // Guard: `attempts > 0` ensures the candidate has evidence before it can auto-promote.
        let current = eval();
        assert_eq!(current.attempts, 0);
        let previous = eval();
        assert_eq!(previous.attempts, 0);
        assert!(!current.can_auto_promote_over(&previous));
    }

    #[test]
    fn skill_auto_promote_allows_zero_attempts_predecessor() {
        // A proven successor (attempts > 0) CAN promote over a never-attempted predecessor.
        let current = eval_with(|e| {
            e.attempts = 2;
            e.successful_attempts = 2;
            e.errors = 0;
            e.user_corrections = 0;
            e.deterministic_checks_passed = true;
        });
        let previous = eval(); // attempts == 0, success_rate == 0.0
        assert!(current.can_auto_promote_over(&previous));
    }

    #[test]
    fn skill_auto_promote_allows_exact_tie_success_rate() {
        let current = eval_with(|e| {
            e.attempts = 20;
            e.successful_attempts = 18;
            e.errors = 1;
            e.user_corrections = 0;
        });
        let previous = eval_with(|e| {
            e.attempts = 20;
            e.successful_attempts = 18;
            e.errors = 1;
            e.user_corrections = 0;
        });
        assert!(current.can_auto_promote_over(&previous));
    }

    #[test]
    fn skill_auto_promote_allows_strict_improvement() {
        let current = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 10;
            e.errors = 0;
            e.user_corrections = 0;
        });
        let previous = eval_with(|e| {
            e.attempts = 10;
            e.successful_attempts = 7;
            e.errors = 3;
            e.user_corrections = 2;
        });
        assert!(current.can_auto_promote_over(&previous));
    }

    // ── success_rate ───────────────────────────────────────

    #[test]
    fn success_rate_returns_zero_for_zero_attempts() {
        let current = eval();
        assert_eq!(current.success_rate(), 0.0);
    }

    #[test]
    fn success_rate_computes_fraction_for_nonzero_attempts() {
        let current = eval_with(|e| {
            e.attempts = 4;
            e.successful_attempts = 3;
        });
        assert_eq!(current.success_rate(), 0.75);
    }

    // ── helper ─────────────────────────────────────────────

    fn eval_with(mutate: impl FnOnce(&mut SkillEvaluation)) -> SkillEvaluation {
        let mut e = eval();
        mutate(&mut e);
        e
    }
}
