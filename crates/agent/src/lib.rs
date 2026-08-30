//! 默认禁止 unsafe 代码；例外集中在 `tools::bash` 的 Windows/Unix 进程与
//! 管道底层调用（`job_object.rs` 进程树终止、`pump.rs` 有界读等待、
//! `exec.rs` 句柄处理），各处以显式 `#[allow(unsafe_code)]` 标注。
#![deny(unsafe_code)]
//! Singularity 核心 Agent 执行引擎。
//!
//! 提供无头（Headless）且独立于客户端界面的 Agent 运行能力：
//!
//! - `agent`（`loop.rs`）：核心执行循环，驱动模型调用、工具批次按模型给定顺序串行执行与运行中转向引导（Steer）；
//! - `session`：严苛的线性 JSONL 会话持久化与崩溃状态自愈机制；
//! - `compaction`：长程上下文自动压缩引擎（摘要提取与动态上下文窗口管理）；
//! - `message`：会话消息与内容块数据模型；
//! - `tools`：内建代码操作工具集（`read` / `glob` / `grep` / `bash` / `edit` / `write`）。

#[path = "loop.rs"]
pub mod agent;
pub mod compaction;
pub mod message;
pub mod prompts;
pub mod session;
pub mod tools;
