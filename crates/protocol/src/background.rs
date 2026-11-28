use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct VisibleBackgroundTask {
    pub task_id: String,
    pub command: String,
    pub description: String,
    pub output: String,
    pub state: BackgroundTaskState,
}

impl From<crate::BackgroundTask> for VisibleBackgroundTask {
    fn from(task: crate::BackgroundTask) -> Self {
        Self {
            task_id: task.task_id,
            command: task.command,
            description: task.description,
            output: task.output,
            state: task.state,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "status", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum BackgroundTaskState {
    Running,
    Exited { exit_code: i32, duration_ms: u64 },
    Stopped { duration_ms: u64 },
    Failed { message: String, duration_ms: u64 },
    Interrupted,
}

impl BackgroundTaskState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskUpdate {
    pub phase: crate::SessionPhase,
    pub task: VisibleBackgroundTask,
}
