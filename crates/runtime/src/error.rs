//! Turn 失败分类与运行错误。
//!
//! 失败 taxonomy（cause 与线格式词形）由 protocol 单点定义、runtime 直接复用：
//! cause 描述失败来源，message 保留真实原因文本（认证材料不进入错误文本）。
//! 本模块只拥有 model 具体失败类型到 provider cause 的分组映射。

use singularity_model::ModelErrorKind;
use singularity_protocol::TurnErrorDetail;
pub use singularity_protocol::TurnFailureCause;
use thiserror::Error;

/// 将模型提供方的具体失败归入 TurnFailureCause；线格式词形由 protocol 定义。
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

#[cfg(test)]
mod tests {
    use super::*;
    use ModelErrorKind::*;

    /// 分组表逐行钉住：某个具体 kind 的归类变化必须先在这张表上显形
    /// （失败归因的不变量，见仓库指令的归因条款）。ModelErrorKind 新增
    /// 变体时 provider_turn_cause 的非穷尽 match 直接编译失败。
    #[test]
    fn provider_kind_groups_map_to_stable_causes() {
        for (kind, expected) in [
            (Cancelled, TurnFailureCause::ProviderCancelled),
            (NetworkError, TurnFailureCause::ProviderNetwork),
            (Timeout, TurnFailureCause::ProviderTimeout),
            (RateLimited, TurnFailureCause::ProviderRateLimited),
            (ProviderOverloaded, TurnFailureCause::ProviderOverloaded),
            (AuthError, TurnFailureCause::ProviderAuth),
            (InvalidRequest, TurnFailureCause::ProviderValidation),
            (JsonSchemaViolation, TurnFailureCause::ProviderValidation),
            (ContentFilter, TurnFailureCause::ProviderValidation),
            (
                ContextLengthExceeded,
                TurnFailureCause::ProviderContextOverflow,
            ),
            (UnknownProviderError, TurnFailureCause::ProviderUnknown),
        ] {
            assert_eq!(provider_turn_cause(kind), expected, "kind {kind:?}");
        }
    }
}

/// crate::TurnRunner::run 的两类失败，各自表示「不存在可信终态」：
/// 准备阶段失败（尚未持久开始）与终态化失败（已经开始但故障阻止可信终态）。
/// Agent 执行失败不是这里的变体：它以协议错误细节随
/// crate::TurnOutcome 的可信失败终态返回。
#[derive(Debug, Error)]
pub enum TurnRunError {
    #[error("{message}")]
    Preparation {
        /// 失败来源分类；尚未成功追加 OperationStarted。
        cause: TurnFailureCause,
        message: String,
    },
    /// 执行已经开始，但不存在可信终态：终态记录没有落盘，或执行期出现
    /// 存储/宿主故障而不再尝试写一份可信终态。`execution` 是已经发生的执行
    /// 失败（若有），`storage` 是阻止终态落盘的存储故障（若有）；两者至少
    /// 一项存在，先发生的失败事实不被后发生的故障覆盖。
    #[error("{}", terminalization_message(execution.as_ref(), storage.as_deref()))]
    Terminalization {
        execution: Option<TurnErrorDetail>,
        storage: Option<String>,
    },
}

/// 收尾故障的宿主报告：主执行原因与收尾原因并列，不互相覆盖。
fn terminalization_message(execution: Option<&TurnErrorDetail>, storage: Option<&str>) -> String {
    match (execution, storage) {
        (Some(execution), Some(storage)) => format!(
            "the turn had already failed ({execution}); its terminal record could not be written: {storage}"
        ),
        (Some(execution), None) => {
            format!("execution stopped without a trustworthy terminal record: {execution}")
        }
        (None, Some(storage)) => format!("terminalization failed: {storage}"),
        // 类型上仍可达，但两个构造点都必然带至少一项原因。
        (None, None) => "terminalization failed without a recorded cause".to_string(),
    }
}
