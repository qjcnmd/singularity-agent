//! 默认禁止 unsafe 代码；唯一例外是 `tools::bash::handle_inheritance` 模块
//! （显式 `#[allow(unsafe_code)]`）：Windows 上清除 stdout/stderr 句柄继承位，
//! 防止强杀后的残留子进程直写本进程 stdout 管道破坏 JSON-RPC 流。
#![deny(unsafe_code)]
//! Singularity 核心 Agent 执行引擎。
//!
//! 提供无头（Headless）且独立于客户端界面的 Agent 运行能力：
//!
//! - `agent`（`loop.rs`）：核心执行循环，驱动模型调用、工具批次并发执行、转向引导（Steer）与跟进（FollowUp）；
//! - `session`：严苛的线性 JSONL 会话持久化与崩溃状态自愈机制；
//! - `compaction`：长程上下文自动压缩引擎（摘要提取与动态上下文窗口管理）；
//! - `message`：会话消息与内容块数据模型；
//! - `tools`：内建代码操作工具集（`read` / `bash` / `edit` / `write`）及并发文件修改队列保护。

#[path = "loop.rs"]
pub mod agent;
pub mod compaction;
pub mod message;
pub mod session;
pub mod tools;
