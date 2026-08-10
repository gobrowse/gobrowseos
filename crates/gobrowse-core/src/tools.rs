use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::policy::RiskClass;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub id: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub risk: RiskClass,
    pub permissions: Vec<String>,
    pub timeout_seconds: u64,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub call_id: Uuid,
    pub user_id: Uuid,
    pub profile_id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub run_id: Uuid,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ToolError {
    #[error("tool input did not match its schema")]
    InvalidInput,
    #[error("tool permission was denied")]
    PermissionDenied,
    #[error("tool timed out")]
    Timeout,
    #[error("tool execution failed")]
    Execution,
    #[error("tool execution was canceled")]
    Canceled,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> &ToolDescriptor;

    async fn execute(
        &self,
        context: &ToolContext,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError>;
}
