// 不写 unsafe 代码；命令进程树交给 process-wrap 管理。
#![deny(unsafe_code)]
//! Singularity 的 Agent：模型与工具循环、会话日志和内置工具。
//!
//! - agent：一轮执行的入口；
//! - session：JSONL 会话日志的读写，格式版本固定为 1；
//! - compaction：摘要请求的构造和校验；
//! - message：把消息转换成工作台历史条目；
//! - prompts：Developer 指令；
//! - tools：内置工具。

pub mod agent;
pub mod compaction;
mod image;
pub mod message;
mod prompts;
mod request_execution;
pub mod session;
pub mod tools;
