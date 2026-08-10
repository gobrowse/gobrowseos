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
