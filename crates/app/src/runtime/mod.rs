//! 任务的执行与历史读取，由桌面工作台调用。
//! Conversation 持有单个任务的状态，负责回合、轮换、停止和后台命令结束后的自动继续；
//! 会话日志的读写在 agent::session。

mod conversation;

mod history;
mod thread_catalog;

pub(crate) use conversation::{Conversation, ConversationError};
pub(crate) use thread_catalog::{CatalogError, SESSIONS_DIR_NAME, ThreadCatalog};
