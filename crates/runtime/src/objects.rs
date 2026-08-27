//! runtime 公开对象。

pub use singularity_protocol::{
    ExecutionThread as Thread, ExecutionTurn as Turn, ExecutionTurnUsage as TurnUsage,
    ThreadStatus, TurnStatus,
};

pub(crate) fn turn_usage_from_model_usage(
    usage: &singularity_model::ModelUsage,
    complete: bool,
) -> TurnUsage {
    TurnUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        usage_present: usage.usage_present,
        usage_complete: complete,
    }
}

/// Provider 配置快照的只读展示投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub source: Option<String>,
    pub snapshot_id: String,
    pub configured: bool,
    pub configuration_blocker: Option<String>,
    pub api_key_present: bool,
    pub base_url_present: bool,
    pub model_present: bool,
}
