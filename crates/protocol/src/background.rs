//! 后台命令的执行事实；实时状态、保存记录与工作台共用。

use serde::{Deserialize, Serialize};

/// 一个归属于会话的后台命令，沿用启动它的回合和工具条目身份。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTask {
    pub task_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub command: String,
    pub description: String,
    pub started_at: String,
    /// 有界输出尾部；超长输出的完整文件位置沿用 Bash 的保存说明。
    pub output: String,
    pub state: BackgroundTaskState,
}

/// 命令的实际运行或结束状态；重启不能从保存的 PID 推测进程仍然存在。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "status", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum BackgroundTaskState {
    Running,
    Exited {
        exit_code: i32,
        finished_at: String,
        duration_ms: u64,
    },
    Stopped {
        finished_at: String,
        duration_ms: u64,
    },
    Failed {
        message: String,
        finished_at: String,
        duration_ms: u64,
    },
    Interrupted,
}

impl BackgroundTaskState {
    /// 只有实际运行的命令需要等待和停止。
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// 后台命令变化的会话修订号与阶段，和普通执行事件采用相同的读取顺序。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskUpdate {
    pub session_revision: u64,
    pub phase: crate::SessionPhase,
    pub task: BackgroundTask,
}
