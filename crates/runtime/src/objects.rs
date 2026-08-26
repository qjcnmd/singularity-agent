//! 公开对象：Thread / Turn / usage。
//!
//! 字段与 serde 形状对齐协议层同构类型；runtime 不依赖 protocol，协议适配器
//! 在边界上做一一映射。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// 一个持久化 Thread 的进程内投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub thread_id: String,
    pub cwd: String,
    pub model: Option<String>,
    pub last_turn_status: Option<ThreadStatus>,
}

/// 一个 turn 的身份与终态投影。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub turn_id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
}

/// 模型 usage 的输出形状（与 `singularity_model::ModelUsage` 同构）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    /// Provider 是否上报过原始 usage 对象。
    pub usage_present: bool,
    /// 聚合中的每个 provider 请求是否都携带精确 usage。
    pub usage_complete: bool,
}

impl TurnUsage {
    pub fn from_model_usage(usage: &singularity_model::ModelUsage, complete: bool) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            usage_present: usage.usage_present,
            usage_complete: complete,
        }
    }
}
