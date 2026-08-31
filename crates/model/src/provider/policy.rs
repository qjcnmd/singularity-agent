//! Agent 请求生命周期的重试策略：模型子系统声明，Agent 采样层执行。
//!
//! 重试属于模型请求生命周期而非渲染层，因此策略形状由 model 拥有并随
//! [`crate::ModelConfigurationSnapshot`] 逐回合冻结；`singularity_agent` 的
//! 采样包装与压缩摘要请求共用同一实例。

use serde::{Deserialize, Serialize};

/// 可重试 provider 错误的指数退避策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRetryPolicy {
    /// 重试上限；0 表示禁用 agent 层重试。
    pub max_retries: u32,
    /// 基础退避毫秒：delay = base × 2^(attempt-1) × 抖动。
    pub base_delay_ms: u64,
}

impl Default for TurnRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}
