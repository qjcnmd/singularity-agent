#![forbid(unsafe_code)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentHostStatus {
    NotMigrated,
    Running,
    Completed,
    Blocked,
    Failed,
}

impl AgentHostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotMigrated => "not_migrated",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopBridge {
    pub status: AgentHostStatus,
    pub completed: bool,
}

impl AgentLoopBridge {
    pub fn not_migrated() -> Self {
        Self {
            status: AgentHostStatus::NotMigrated,
            completed: false,
        }
    }
}
