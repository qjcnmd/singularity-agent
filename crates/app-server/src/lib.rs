#![forbid(unsafe_code)]

//! 在进程边界负责 session/turn 准入、协议对象转换与事件投影的 stdio JSON-RPC
//! 应用服务。
//!
//! 职责边界：Thread/Turn 执行语义（Agent 构造、Provider 准备、会话单写者、
//! 事件顺序、usage 聚合、终态落盘、取消、steer/followUp、设置时序）由
//! [`singularity_runtime`] 唯一拥有；本 crate 只做 JSON-RPC 解析、只读投影、
//! 协议对象转换，并把 [`singularity_runtime::TurnEvent`] 一一映射为协议通知。
//! JSONL rollout 是会话正文与列表元数据的唯一持久事实源。

mod dispatch;
mod events;
mod lifecycle;
pub mod paths;
mod state;
mod transport;
mod wire;

use std::fmt;
use std::path::Path;

use serde_json::Value;
use singularity_protocol::{
    ErrorCode, InitializeParams, JsonRpcId, JsonRpcMessage, Method, MethodKind,
    ServerShutdownResult, SessionDeleteResult, SessionIdParams, Thread, ThreadListResult,
    ThreadReadParams, ThreadReadResult, ThreadSettingsParams, ThreadSettingsResult,
    ThreadStartParams, ThreadStartResult, ThreadStatus, Turn, TurnIdParams, TurnInjectionParams,
    TurnInjectionResult, TurnInterruptResult, TurnStartParams, TurnStatus,
};
use thiserror::Error;

const THREAD_NOT_FOUND: &str = "Thread not found";
const TURN_NOT_FOUND: &str = "Turn not found";
const SESSION_DELETE_TURN_ACTIVE: &str =
    "session/delete rejected: a turn is still active for this session";
const SESSION_DELETE_WRITER_ACTIVE: &str =
    "session/delete rejected: session is being written by an active writer";
const APP_ERROR_INVALID_STATE: i64 = -32005;

/// 运行 stdio JSON-Lines app-server，供同包二进制入口调用。
#[doc(hidden)]
pub async fn run_stdio(runtime_handle: tokio::runtime::Handle) -> Result<(), String> {
    transport::run(runtime_handle).await
}

/// 在应用边界转换为 JSON-RPC 响应的错误。
#[derive(Debug, Error)]
pub enum AppServerError {
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("workspace error: {0}")]
    Workspace(String),
    /// 共享核心的 turn 执行失败：分类来自 runtime 的失败 taxonomy，
    /// original 仅在 RPC 边界用于透出真实原因，不参与持久化分类。
    #[error("turn execution failed during {stage} ({cause})")]
    TurnExecution {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        original: Option<String>,
    },
    #[error("turn execution failed during {stage} ({cause}); terminalization failed ({failure})")]
    TurnTerminalization {
        stage: TurnFailureStage,
        cause: TurnFailureCause,
        failure: TurnTerminalizationFailure,
        /// 原始失败文本；仅在 RPC 边界用于透出真实原因，不参与持久化分类。
        original: Option<String>,
    },
}

/// `AppServer` 请求处理和生命周期操作使用的结果类型。
pub type AppServerResult<T> = Result<T, AppServerError>;

/// 已进入 Running Turn 的失败阶段与原因分类：直接复用 runtime 的类型化
/// taxonomy（wire 词形在 runtime 单点定义），不在本边界复制第二套枚举。
pub use singularity_runtime::{ProviderFailureKind, TurnFailureCause, TurnFailureStage};

/// 终态补偿失败的稳定分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnTerminalizationFailure {
    Store,
}

impl fmt::Display for TurnTerminalizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Store => "store",
        })
    }
}

/// AppServer 交给 stdout transport 的消息。
pub type AppServerOutput = Value;

pub use dispatch::{TurnClaim, TurnStartClaim};
pub use paths::AppPaths;
pub use state::{AppServer, AppServerCancellationHandle, AppServerControlHandle};
pub use wire::thread_from_summary;
