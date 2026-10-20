//! 未保存的执行错误直接交给工作台；已保存的失败由会话历史显示。
use singularity_protocol::TurnErrorDetail;
pub use singularity_protocol::TurnFailureCause;
use thiserror::Error;

/// 准备失败或执行结果未能保存。
#[derive(Debug, Error)]
pub enum TurnRunError {
    #[error("{0}")]
    Preparation(String),
    #[error("{0}")]
    Execution(TurnErrorDetail),
}

impl TurnRunError {
    /// 工作台使用与执行层相同的错误详情。
    pub fn detail(&self) -> TurnErrorDetail {
        match self {
            Self::Preparation(message) => TurnErrorDetail {
                cause: TurnFailureCause::Internal,
                message: message.clone(),
                item_id: None,
            },
            Self::Execution(detail) => detail.clone(),
        }
    }
}
