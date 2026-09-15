//! Agent 运行事件出口：生命周期事件与脱敏诊断。
//!
//! 事件经调用方传入的单一回调 `&mut dyn FnMut(AgentEvent)` 流式投递；投影为
//! 尽力而为，消费方自行吸收失败，不改变轮次结果。不观察事件的调用方传入空闭包。

use serde_json::Value;
use singularity_protocol::DiagnosticSeverity;

use crate::tools::ToolExecution;

pub(crate) mod diagnostic_code {
    pub const COMPACTION_SKIPPED: &str = "compaction_skipped";
    pub const CONTEXT_OVERFLOW_RECOVERY_FAILED: &str = "context_overflow_recovery_failed";
    pub const PROVIDER_RETRY_SCHEDULED: &str = "provider_retry_scheduled";
}

/// 安全、非持久化的诊断。code 对投影方稳定；message 文本刻意
/// 不包含原始 provider payload（脱敏边界）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDiagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
}

impl AgentDiagnostic {
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Info,
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Agent 运行生命周期事件，统一经调用方的事件回调流式投递。
///
/// tool 的 Started 事件按调用顺序投递，Update/Ended 按实际完成顺序投递；
/// 持久化的 toolResult 按完成顺序追加，模型上下文按调用顺序投影。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 模型流式文本输出增量更新。
    MessageUpdate { message_id: String, delta: String },
    /// 当前 assistant 消息公开思考文本的流式增量。
    ThinkingUpdate { message_id: String, delta: String },
    /// 已保存消息的最终可见内容；存储失败时 items 为空，仅关闭已显示的进度。
    MessageFinished {
        message_id: String,
        items: Vec<singularity_protocol::HistoryItem>,
        failed: bool,
    },
    /// 工具开始执行事件。
    ToolExecutionStarted {
        item_id: String,
        tool_name: String,
        arguments: Value,
    },
    /// 工具执行中产生的流式增量输出事件；工具事实由 Started 建立，
    /// 这里只按 item_id 更新累计进度。
    ToolExecutionUpdate {
        item_id: String,
        partial_result: String,
    },
    /// 工具执行完成事件；名称与参数已在 Started 发布。
    ToolExecutionEnded {
        item_id: String,
        execution: ToolExecution,
    },
    /// 非致命、脱敏 Agent 诊断；不会写入 Session JSONL。
    Diagnostic(AgentDiagnostic),
    /// provider HTTP attempt 生命周期观测；model-turn 序号已在循环内绑定。
    ///
    /// 投影为尽力而为；消费方自行吸收投影失败，不影响 provider 结果。
    ProviderAttempt {
        observation: singularity_protocol::RequestObservation,
        protocol: String,
        diagnostic_code: Option<String>,
        retry_after_ms: Option<u64>,
        retry_after_source: Option<singularity_protocol::RetryAfterSource>,
    },
    /// 已持久化的用户消息事实：初始输入与注入输入共用同一条出口，消息
    /// id 与持久历史条目一致，客户端据此贯通实时条目与历史。
    UserMessage { entry_id: String, text: String },
    /// 当前进程内的控制接受与处置通知；runtime 据此更新并发布当前会话投影。
    /// 控制队列不落盘，重启后不恢复。
    ControlChanged(singularity_protocol::ControlSnapshot),
}
