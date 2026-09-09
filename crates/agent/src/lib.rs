//! 默认禁止 unsafe 代码；例外集中在 tools::bash 的 Windows/Unix 进程与
//! 管道底层调用（job_object.rs 进程树终止、pump.rs 有界读等待、
//! exec.rs 句柄处理），各处以显式 #[allow(unsafe_code)] 标注。
#![deny(unsafe_code)]
//! Singularity 核心 Agent 执行引擎。
//!
//! 提供无头（Headless）且独立于客户端界面的 Agent 运行能力：
//!
//! - agent：单一 Agent execution seam——轮步循环驱动模型请求、
//!   可取消重试、只读工具并发和副作用工具串行、逐项持久化结果、主动/溢出压缩、steer 注入与
//!   终态转换，并在每个执行边界落盘 operation ledger 事实；
//! - session：严格 JSONL v6 持久化——线性消息/压缩条目 + 单 lane operation
//!   ledger 记录，单写者锁、durable 前缀归约与崩溃自愈（绝不重放未知副作用）；
//! - compaction：长程上下文压缩引擎（摘要提取与合法切点策略）；
//! - message：会话消息与内容块数据模型；
//! - prompts：系统提示词、工具说明与运行环境的装配出口；
//! - tools：内建代码操作与技能工具集（read / glob / grep / bash / edit / write / skill）
//!   与注册表快照。

pub mod agent;
pub mod compaction;
pub mod message;
pub mod prompts;
pub mod session;
pub mod tools;
