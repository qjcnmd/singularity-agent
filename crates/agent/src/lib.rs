//! 默认禁止 unsafe 代码；例外只集中在 tools::bash 调用 Windows 进程与管道的底层
//! 代码（job_object.rs、pump.rs、exec.rs），每一处都用显式的 #[allow(unsafe_code)] 标注。
#![deny(unsafe_code)]
//! Singularity 的核心 Agent 执行引擎。
//!
//! 提供无头（Headless）、不依赖客户端界面的 Agent 运行能力；各模块职责见其自身模块文档。
//!
//! - agent：Agent 执行流程唯一的入口（execution seam）；
//! - session：严格的 JSONL 会话持久化，当前格式版本见
//!   `session::format::CURRENT_SESSION_VERSION`；
//! - compaction / message / prompts / tools：压缩引擎、消息模型、提示词装配与内建工具集。

pub mod agent;
pub mod compaction;
mod events;
pub mod message;
pub mod prompts;
mod request_execution;
pub mod session;
pub mod tools;
