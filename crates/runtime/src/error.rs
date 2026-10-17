//! Turn 的失败分类与运行错误。
//!
//! 执行失败的分类由 AgentError 统一提供。本模块表示准备与存储故障，
//! 并把尚未保存的失败原因交给宿主显示。

use singularity_protocol::TurnErrorDetail;
pub use singularity_protocol::TurnFailureCause;
use thiserror::Error;

/// 执行器未提交可信终态时的错误。普通 Agent 错误随 TurnOutcome 返回，
/// 准备、执行期致命故障和终态落盘失败在本枚举中分别表达。
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

impl TurnRunError {
    /// 尚未保存的失败供工作台直接显示，已有条目身份随错误保留。
    pub fn detail(&self) -> TurnErrorDetail {
        let (cause, item_id) = match self {
            Self::Preparation(_) => (TurnFailureCause::Internal, None),
            Self::Execution(detail) => (detail.cause, detail.item_id.clone()),
            Self::Terminalization { execution, .. } => {
                (TurnFailureCause::Store, execution.as_ref().and_then(|error| error.item_id.clone()))
            }
        };
        TurnErrorDetail {
            cause,
            message: self.to_string(),
            item_id,
        }
    }
}

fn terminalization_message(execution: Option<&TurnErrorDetail>, storage: &str) -> String {
    match execution {
        Some(execution) => format!(
            "the operation had already failed ({execution}); its terminal record could not be written: {storage}"
        ),
        None => format!("terminalization failed: {storage}"),
    }
}
