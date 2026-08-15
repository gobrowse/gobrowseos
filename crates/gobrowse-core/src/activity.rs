use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActivityKind {
    TaskCreated,
    TaskAssigned,
    AgentStarted,
    AgentStopped,
    WorktreeCreated,
    FilesChanged,
    CommitCreated,
    TestStarted,
    TestCompleted,
    Blocked,
    Waiting,
    MergeRequested,
    Merged,
    SkillCreated,
    SkillUpdated,
    BookCreated,
    BookUpdated,
    WorktreeDeleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: i64,
    pub workspace_id: Uuid,
    pub task_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub kind: ActivityKind,
    pub payload: serde_json::Value,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Backlog,
    Ready,
    Running,
    Blocked,
    Review,
    Done,
    Failed,
    Canceled,
}

impl TaskState {
    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskState::{Backlog, Blocked, Canceled, Done, Failed, Ready, Review, Running};
        matches!(
            (self, next),
            (Backlog, Ready | Canceled)
                | (Ready, Running | Blocked | Canceled)
                | (Running, Blocked | Review | Done | Failed | Canceled)
                | (Blocked, Ready | Running | Canceled)
                | (Review, Running | Done | Canceled)
                | (Failed, Ready | Canceled)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All 18 allowed `(from, to)` pairs transcribed directly from the
    /// `matches!` arms in `can_transition_to`:
    ///
    ///   (Backlog, Ready)         (Backlog, Canceled)
    ///   (Ready, Running)         (Ready, Blocked)       (Ready, Canceled)
    ///   (Running, Blocked)       (Running, Review)      (Running, Done)
    ///   (Running, Failed)        (Running, Canceled)
    ///   (Blocked, Ready)         (Blocked, Running)     (Blocked, Canceled)
    ///   (Review, Running)        (Review, Done)         (Review, Canceled)
    ///   (Failed, Ready)          (Failed, Canceled)
    ///
    /// Self-transitions are never allowed. `Done` and `Canceled` have no
    /// outgoing edges (terminal sinks).
    const ALLOWED: &[(TaskState, TaskState)] = &[
        (TaskState::Backlog, TaskState::Ready),
        (TaskState::Backlog, TaskState::Canceled),
        (TaskState::Ready, TaskState::Running),
        (TaskState::Ready, TaskState::Blocked),
        (TaskState::Ready, TaskState::Canceled),
        (TaskState::Running, TaskState::Blocked),
        (TaskState::Running, TaskState::Review),
        (TaskState::Running, TaskState::Done),
        (TaskState::Running, TaskState::Failed),
        (TaskState::Running, TaskState::Canceled),
        (TaskState::Blocked, TaskState::Ready),
        (TaskState::Blocked, TaskState::Running),
        (TaskState::Blocked, TaskState::Canceled),
        (TaskState::Review, TaskState::Running),
        (TaskState::Review, TaskState::Done),
        (TaskState::Review, TaskState::Canceled),
        (TaskState::Failed, TaskState::Ready),
        (TaskState::Failed, TaskState::Canceled),
    ];

    const ALL_STATES: &[TaskState] = &[
        TaskState::Backlog,
        TaskState::Ready,
        TaskState::Running,
        TaskState::Blocked,
        TaskState::Review,
        TaskState::Done,
        TaskState::Failed,
        TaskState::Canceled,
    ];

    #[test]
    fn transition_table_exhaustively_matches_can_transition_to() {
        // Build the expected set from the hand-transcribed ALLOWED constant
        // — we do NOT call can_transition_to to compute it.  Then assert
        // every one of the 64 `(from, to)` pairs matches.
        for &from in ALL_STATES {
            for &to in ALL_STATES {
                let expected = ALLOWED.contains(&(from, to));
                let actual = from.can_transition_to(to);
                assert_eq!(
                    actual, expected,
                    "({from:?}, {to:?}): expected {expected}, got {actual}"
                );
            }
        }
    }

    #[test]
    fn terminal_states_reject_every_transition() {
        for terminal in [TaskState::Done, TaskState::Canceled] {
            for &to in ALL_STATES {
                assert!(
                    !terminal.can_transition_to(to),
                    "terminal {terminal:?} → {to:?} should be false"
                );
            }
        }
    }

    #[test]
    fn no_state_transitions_to_itself() {
        for &state in ALL_STATES {
            assert!(
                !state.can_transition_to(state),
                "{state:?} → {state:?} should be false (no self-loops)"
            );
        }
    }

    #[test]
    fn non_terminal_states_can_cancel() {
        let cancellable = [
            TaskState::Backlog,
            TaskState::Ready,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Review,
            TaskState::Failed,
        ];
        for &from in &cancellable {
            assert!(
                from.can_transition_to(TaskState::Canceled),
                "{from:?} → Canceled should be true"
            );
        }
    }

    #[test]
    fn failed_task_can_retry_to_ready() {
        assert!(TaskState::Failed.can_transition_to(TaskState::Ready));
    }

    #[test]
    fn running_can_proceed_to_review_done_failed_or_blocked() {
        assert!(TaskState::Running.can_transition_to(TaskState::Review));
        assert!(TaskState::Running.can_transition_to(TaskState::Done));
        assert!(TaskState::Running.can_transition_to(TaskState::Failed));
        assert!(TaskState::Running.can_transition_to(TaskState::Blocked));
        assert!(TaskState::Running.can_transition_to(TaskState::Canceled));
        // No spontaneous rewind
        assert!(!TaskState::Running.can_transition_to(TaskState::Ready));
        assert!(!TaskState::Running.can_transition_to(TaskState::Backlog));
    }
}
