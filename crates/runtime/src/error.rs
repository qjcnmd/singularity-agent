//! Turn 失败分类与运行错误。
//!
//! 失败 taxonomy（stage/cause 与线格式词形）由 protocol 单点定义、runtime
//! 直接复用：stage 描述失败发生的管线阶段，cause 描述失败来源，original
//! 保留真实原因文本（认证材料不进入错误文本）。本模块只拥有
//! model 具体失败类型到 provider cause 的分组映射。

use singularity_model::ModelErrorKind;
pub use singularity_protocol::{TurnFailureCause, TurnFailureStage};
use thiserror::Error;

/// Provider 失败的稳定分类：`ModelErrorKind`（12 个具体失败类型）到协议
/// `TurnFailureCause`（9 个 provider 分类）的分组是本函数唯一拥有——线格式
/// 词形由 protocol 的 serde snake_case 投影单源提供，本层只做 kind→cause 分组。
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
        UnknownProviderError | UnsupportedCapability => TurnFailureCause::ProviderUnknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ModelErrorKind::*;

    /// 分组表逐行钉住：某个具体 kind 的归类变化必须先在这张表上显形
    /// （失败归因的不变量，见仓库指令的归因条款）。`ModelErrorKind` 新增
    /// 变体时 `provider_turn_cause` 的非穷尽 match 直接编译失败。
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
            (UnsupportedCapability, TurnFailureCause::ProviderUnknown),
            (UnknownProviderError, TurnFailureCause::ProviderUnknown),
        ] {
            assert_eq!(provider_turn_cause(kind), expected, "kind {kind:?}");
        }
    }
}

/// 一次可归因的 turn 失败事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    pub stage: TurnFailureStage,
    pub cause: TurnFailureCause,
    /// 真实原因文本；认证材料不进入错误文本。
    pub original: Option<String>,
}

/// [`crate::TurnRunner::run`] 的三类失败：
/// 准备阶段失败（无 turn 痕迹）、执行阶段失败（终态已收敛）、
/// 终态化失败（terminal metadata 无法落盘的 fatal 存储错误）。
#[derive(Debug, Error)]
pub enum TurnRunError {
    #[error("{message}")]
    Preparation {
        /// 失败来源分类；turn 未留下任何痕迹。
        cause: TurnFailureCause,
        message: String,
    },
    #[error("turn failed: {0:?}")]
    Execution(TurnFailure),
    #[error("terminalization failed: {0:?}")]
    Terminalization(TurnFailure),
}
