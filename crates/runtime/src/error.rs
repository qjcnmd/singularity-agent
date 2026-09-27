//! Turn 的失败分类与运行错误。
//!
//! 失败的分类体系（cause 和它在 wire 上的词形）由 protocol 单点定义，runtime 直接
//! 复用：cause 说明失败来自哪里，message 保留真实原因文本（认证材料不进错误文本）。
//! 本模块只负责一件事：把 model 的具体失败类型归入对应的 provider cause。

use singularity_model::ModelErrorKind;
use singularity_protocol::TurnErrorDetail;
pub use singularity_protocol::TurnFailureCause;
use thiserror::Error;

pub(crate) fn provider_turn_cause(kind: ModelErrorKind) -> TurnFailureCause {
    use ModelErrorKind::*;
    match kind {
        RateLimited => TurnFailureCause::ProviderRateLimited,
        NetworkError => TurnFailureCause::ProviderNetwork,
        Timeout => TurnFailureCause::ProviderTimeout,
        AuthError => TurnFailureCause::ProviderAuth,
        InvalidRequest | JsonSchemaViolation | ContentFilter => {
            TurnFailureCause::ProviderValidation
        }
        ProviderOverloaded => TurnFailureCause::ProviderOverloaded,
        Cancelled => TurnFailureCause::ProviderCancelled,
        ContextLengthExceeded => TurnFailureCause::ProviderContextOverflow,
        UnknownProviderError => TurnFailureCause::ProviderUnknown,
    }
}

/// 执行器未提交可信终态的失败。普通 Agent 错误随 TurnOutcome 返回；
/// 准备、执行期致命故障和终态落盘失败在这里分别表达。
#[derive(Debug, Error)]
pub enum TurnRunError {
    #[error("{0}")]
    Preparation(String),
    /// 执行期存储或宿主故障，不能继续写可信终态。
    #[error("execution stopped without a trustworthy terminal record: {0}")]
    Execution(TurnErrorDetail),
    /// 终态落盘失败，同时保留先前发生的执行错误。
    #[error("{}", terminalization_message(execution.as_ref(), storage))]
    Terminalization {
        execution: Option<TurnErrorDetail>,
        storage: String,
    },
}

fn terminalization_message(execution: Option<&TurnErrorDetail>, storage: &str) -> String {
    match execution {
        Some(execution) => format!(
            "the turn had already failed ({execution}); its terminal record could not be written: {storage}"
        ),
        None => format!("terminalization failed: {storage}"),
    }
}
