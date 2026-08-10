use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    BuildingContext,
    AwaitingModel,
    AwaitingPolicy,
    AwaitingApproval,
    RunningTool,
    Paused,
    Completed,
    Failed,
    Canceled,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEvent {
    Started,
    ContextBuilt,
    ModelRequested,
    ToolRequested,
    PolicyAllowed,
    ApprovalRequired,
    ApprovalGranted,
    ApprovalDenied,
    ToolCompleted,
    PauseRequested,
    ResumeRequested,
    TurnCompleted,
    Failed,
    CancelRequested,
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid agent transition from {from:?} on {event:?}")]
pub struct TransitionError {
    pub from: RunState,
    pub event: RunEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: Uuid,
    pub state: RunState,
    pub step: u32,
}

impl AgentRun {
    pub fn transition(&mut self, event: RunEvent) -> Result<RunState, TransitionError> {
        let next = match (self.state, &event) {
            (RunState::Queued, RunEvent::Started) => RunState::BuildingContext,
            (RunState::BuildingContext, RunEvent::ContextBuilt) => RunState::AwaitingModel,
            (RunState::AwaitingModel, RunEvent::ModelRequested) => RunState::AwaitingModel,
            (RunState::AwaitingModel, RunEvent::ToolRequested) => RunState::AwaitingPolicy,
            (RunState::AwaitingPolicy, RunEvent::PolicyAllowed) => RunState::RunningTool,
            (RunState::AwaitingPolicy, RunEvent::ApprovalRequired) => RunState::AwaitingApproval,
            (RunState::AwaitingApproval, RunEvent::ApprovalGranted) => RunState::RunningTool,
            (RunState::AwaitingApproval, RunEvent::ApprovalDenied) => RunState::AwaitingModel,
            (RunState::RunningTool, RunEvent::ToolCompleted) => RunState::AwaitingModel,
            (RunState::AwaitingModel, RunEvent::TurnCompleted) => RunState::Completed,
            (RunState::Paused, RunEvent::ResumeRequested) => RunState::BuildingContext,
            (state, RunEvent::PauseRequested) if !state.is_terminal() => RunState::Paused,
            (state, RunEvent::CancelRequested) if !state.is_terminal() => RunState::Canceled,
            (state, RunEvent::Failed) if !state.is_terminal() => RunState::Failed,
            _ => {
                return Err(TransitionError {
                    from: self.state,
                    event,
                });
            }
        };
        self.state = next;
        self.step = self.step.saturating_add(1);
        Ok(next)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    pub context_window: u32,
    pub reserved_output: u32,
    pub system: u32,
    pub recent_conversation: u32,
    pub pinned_books: u32,
    pub retrieved_library: u32,
    pub skills: u32,
    pub tools: u32,
}

impl TokenBudget {
    pub fn input_limit(self) -> u32 {
        self.context_window.saturating_sub(self.reserved_output)
    }

    pub fn allocated(self) -> u32 {
        self.system
            .saturating_add(self.recent_conversation)
            .saturating_add(self.pinned_books)
            .saturating_add(self.retrieved_library)
            .saturating_add(self.skills)
            .saturating_add(self.tools)
    }

    pub fn fits(self) -> bool {
        self.allocated() <= self.input_limit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_path_is_explicit_and_resumable() {
        let mut run = AgentRun {
            id: Uuid::nil(),
            state: RunState::Queued,
            step: 0,
        };
        for event in [
            RunEvent::Started,
            RunEvent::ContextBuilt,
            RunEvent::ToolRequested,
            RunEvent::ApprovalRequired,
            RunEvent::ApprovalGranted,
            RunEvent::ToolCompleted,
            RunEvent::TurnCompleted,
        ] {
            run.transition(event).unwrap();
        }
        assert_eq!(run.state, RunState::Completed);
        assert_eq!(run.step, 7);
    }

    #[test]
    fn terminal_runs_cannot_be_resumed() {
        let mut run = AgentRun {
            id: Uuid::nil(),
            state: RunState::Completed,
            step: 3,
        };
        assert!(run.transition(RunEvent::ResumeRequested).is_err());
    }

    #[test]
    fn budget_never_underflows() {
        let budget = TokenBudget {
            context_window: 100,
            reserved_output: 200,
            system: 1,
            recent_conversation: 0,
            pinned_books: 0,
            retrieved_library: 0,
            skills: 0,
            tools: 0,
        };
        assert_eq!(budget.input_limit(), 0);
        assert!(!budget.fits());
    }
}
