//! 默认禁止 unsafe 代码；例外集中在 tools::bash 调用 Windows 进程与管道的底层代码
//! （job_object.rs、pump.rs、exec.rs），每处都用 #[allow(unsafe_code)] 显式标注。
#![deny(unsafe_code)]
//! Singularity 的核心 Agent 执行引擎。
//!
//! 各模块职责见其自身模块文档：
//! - agent：Agent 执行流程的入口；
//! - session：JSONL 会话持久化，当前格式版本见 `session::format::CURRENT_SESSION_VERSION`；
//! - compaction / message / prompts / tools：压缩引擎、消息模型、提示词装配与内建工具集。
//! - events：Agent 运行事件的出口与脱敏诊断。

pub mod agent;
pub mod compaction;
mod events;
pub mod image;
pub mod message;
mod prompts;
mod request_execution;
pub mod session;
pub mod tools;
