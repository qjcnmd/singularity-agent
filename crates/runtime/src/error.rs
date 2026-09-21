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

/// crate::TurnRunner::run 的两类失败，都表示「没有可信终态」：准备阶段失败（还没持久化开始
/// 标记）和终态化失败（已经开始，但故障让可信终态写不下去）。Agent 执行失败不属于这里：它
/// 作为协议错误细节，随 crate::TurnOutcome 里的可信失败终态返回。
#[derive(Debug, Error)]
pub enum TurnRunError {
    #[error("{message}")]
    Preparation {
        /// 失败来源分类；此时还没成功追加 OperationStarted。
        cause: TurnFailureCause,
        message: String,
    },
    /// 执行已经开始，但没有可信终态：要么终态记录没落盘，要么执行期出了存储/宿主故障，于是不再
    /// 尝试写一份可信终态。`execution` 是已发生的执行失败，`storage` 是挡住终态落盘的存储故障；
    /// 两者至少有一个存在，先发生的失败事实不会被后发生的故障覆盖。
    #[error("{}", terminalization_message(execution.as_ref(), storage.as_deref()))]
    Terminalization {
        execution: Option<TurnErrorDetail>,
        storage: Option<String>,
    },
}

fn terminalization_message(execution: Option<&TurnErrorDetail>, storage: Option<&str>) -> String {
    match (execution, storage) {
        (Some(execution), Some(storage)) => format!(
            "the turn had already failed ({execution}); its terminal record could not be written: {storage}"
        ),
        (Some(execution), None) => {
            format!("execution stopped without a trustworthy terminal record: {execution}")
        }
        (None, Some(storage)) => format!("terminalization failed: {storage}"),
        // 类型上仍然可达，但两个构造点都必然带至少一项原因。
        (None, None) => "terminalization failed without a recorded cause".to_string(),
    }
}
