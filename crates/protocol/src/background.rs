//! 后台命令的状态；实时事件、日志记录和工作台共用。

use serde::{Deserialize, Serialize};

/// 给模型读取的命令纯文本输出快照，与界面展示正文分开保存。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandOutputSnapshot {
    /// 最多 100 KiB 的纯文本尾部。
    pub text: String,
    /// 从命令输出起点累计的纯文本字节数。
    pub total_bytes: u64,
    /// 完整纯文本输出文件的路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub full_output: Option<String>,
    /// 保存完整输出时的错误。
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub file_error: Option<String>,
    /// 管道排空超时，输出尚未全部收齐。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "typescript", ts(as = "Option<bool>", optional))]
    pub incomplete: bool,
}

/// 任务中的一个后台命令。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackgroundTask {
    pub task_id: String,
    pub command: String,
    pub description: String,
    /// 输出的末尾部分；超长时附带完整输出文件的位置，与 bash 工具的结果相同。
    pub output: String,
    pub model_output: CommandOutputSnapshot,
    pub state: BackgroundTaskState,
}

/// 命令的运行或结束状态；重启后历史中仍为运行中的命令显示为中断，不根据 PID 判断进程是否还在。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 只有实际运行的命令需要等待和停止。
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

/// 后台命令的状态变化，附带当时的任务阶段；先后顺序由外层事件序号决定。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskUpdate {
    pub phase: crate::SessionPhase,
    pub task: BackgroundTask,
}
