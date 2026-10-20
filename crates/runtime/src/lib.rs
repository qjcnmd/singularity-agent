#![forbid(unsafe_code)]

//! Thread/Turn 的生命周期协调与进程内 turn 执行管线，由桌面工作台调用。
//! TurnRunner 负责单个 turn，Conversation 维护 Thread 长驻状态：单活动 turn、steer、
//! 取消和设置生效时机。JSONL 持久化在 singularity_agent::session，客户端各自投影事件。

mod conversation;
mod error;
mod runner;

mod event_projection;
mod history;
mod thread_catalog;

pub use conversation::{
    Conversation, ConversationError, ConversationSnapshot, OperationReservation, OperationResult,
};
pub use error::{TurnFailureCause, TurnRunError};
pub use history::accumulate_request_usage;
pub use runner::{TurnOutcome, TurnRunner};
pub use thread_catalog::{CatalogError, SESSIONS_DIR_NAME, ThreadCatalog, ThreadSnapshot};

/// 文字与图片组成的任务输入，RPC 入口解析时校验输入不为空。
pub use singularity_agent::agent::UserInput;
/// 后台进程已经保存并公开的状态事件。
pub use singularity_agent::tools::BashTaskUpdate;

/// 会话目录创建或打开的写者，随后由 Conversation 接管。
pub use singularity_agent::session::SessionManager;
