//! Agent 运行事件的出口：生命周期事件与脱敏后的诊断。
//!
//! 事件通过调用方传入的唯一回调 `&mut dyn FnMut(AgentEvent)` 流式送出；投递尽力而为，
//! 消费方自己吸收失败，不影响轮次结果，不需要观察事件的调用方传空闭包。

use serde_json::Value;
use singularity_protocol::DiagnosticSeverity;

use crate::tools::ToolExecution;

pub(crate) mod diagnostic_code {
    pub const COMPACTION_SKIPPED: &str = "compaction_skipped";
    pub const CONTEXT_OVERFLOW_RECOVERY_FAILED: &str = "context_overflow_recovery_failed";
    pub const PROVIDER_RETRY_SCHEDULED: &str = "provider_retry_scheduled";
}

/// 安全且不落盘的诊断。code 对消费方保持稳定；message 文本刻意不包含 provider 的原始 payload（这是脱敏边界）。
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

/// 工具 Started 事件按调用顺序投递，Update/Ended 按实际完成顺序投递；
/// 落盘的 toolResult 按完成顺序追加，模型上下文则按调用顺序投影。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 模型正文的流式增量；message_id 指向它所属的 assistant 消息。
    MessageUpdate { message_id: String, delta: String },
    /// 当前 assistant 消息里对外公开的思考文本的流式增量。
    ThinkingUpdate { message_id: String, delta: String },
    /// 重试前丢掉当前请求还没定稿的正文与思考。
    MessageDiscarded { message_id: String },
    /// 已落盘的正式消息，或最终中断时用于显示的内容。
    MessageFinished {
        message_id: String,
        items: Vec<singularity_protocol::HistoryItem>,
        failed: bool,
    },
    /// 工具开始执行。
    ToolExecutionStarted {
        item_id: String,
        tool_name: String,
        arguments: Value,
    },
    /// 工具执行过程中产生的流式增量输出；工具本身的事实由 Started 建立，这里只按 item_id 更新累计进度。
    ToolExecutionUpdate {
        item_id: String,
        partial_result: String,
    },
    /// 工具执行完成；工具名称与参数已在 Started 发布，这里不再重复。
    ToolExecutionEnded {
        item_id: String,
        execution: ToolExecution,
    },
    /// 非致命且已脱敏的 Agent 诊断；不会写入 Session 的 JSONL。
    Diagnostic(AgentDiagnostic),
    /// 一次 provider HTTP attempt 的生命周期观测；它属于哪个 model-turn 已在
    /// 循环内绑定好。
    ProviderAttempt {
        observation: singularity_protocol::RequestObservation,
        protocol: String,
        retry_after_ms: Option<u64>,
    },
    /// 已落盘的用户消息：初始输入和注入输入共用这一条出口，消息 id 与持久历史
    /// 条目一致，客户端据此把实时条目和历史对上。
    UserMessage { entry_id: String, text: String },
    /// 当前进程内的控制接受与处置通知；runtime 据此更新并发布当前会话的状态。
    /// 控制队列不落盘，重启后不会恢复。
    ControlChanged(singularity_protocol::ControlSnapshot),
}
