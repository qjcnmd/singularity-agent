//! 默认禁止 unsafe 代码；唯一例外是 `tools::bash::handle_inheritance` 模块
//! （显式 `#[allow(unsafe_code)]`）：Windows 上清除 stdout/stderr 句柄继承位，
//! 防止强杀后的残留子进程直写本进程 stdout 管道破坏 JSON-RPC 流。
#![deny(unsafe_code)]
//! Singularity headless agent core（Phase 3 目标形态）。
//!
//! 可嵌入的 agent 核心，与具体 CLI / app-server 解耦：
//!
//! - `agent`（loop.rs）：`Agent` 循环，驱动模型 turn 与工具执行；
//! - `session`：JSONL 会话文件持久化；
//! - `compaction`：上下文压缩（summary 生成与窗口管理）；
//! - `message`：会话消息模型（含 compaction 标记）；
//! - `tools`：内置工具集（read / bash / edit / write），进程内执行，继承宿主权限。

#[path = "loop.rs"]
pub mod agent;
pub mod compaction;
pub mod message;
pub mod session;
pub mod tools;
