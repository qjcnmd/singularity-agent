#![forbid(unsafe_code)]

//! Thread/Turn 的生命周期协调与进程内 turn 执行管线，评估入口（--json）和桌面工作台共用。
//! TurnRunner 负责单个 turn，Conversation 维护 Thread 长驻状态：单活动 turn、steer/followUp、
//! 取消和设置生效时机。JSONL 持久化在 singularity_agent::session，客户端各自投影事件。

mod conversation;
mod error;
mod runner;

mod assistant_items;
mod history;
mod thread_catalog;

pub use conversation::{
    Conversation, ConversationControlError, ConversationError, ConversationSnapshot, FollowUpPromotion,
    OperationGuard, OperationReservation, OperationResult, validate_input,
};
pub use error::{TurnFailureCause, TurnRunError};
pub use runner::{TurnOutcome, TurnRunner};
pub use singularity_agent::tools::bash::ensure_available as ensure_bash_available;
pub use thread_catalog::{CatalogError, SESSIONS_DIR_NAME, ThreadCatalog, ThreadSnapshot};

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests;

/// 文字与图片组成的任务输入，Conversation 在接受操作时校验输入不为空。
pub use singularity_agent::agent::UserInput;
