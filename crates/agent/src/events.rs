//! Agent 运行事件的出口：生命周期事件。
//!
//! 事件通过调用方传入的 `&mut dyn FnMut(AgentEvent)` 回调流式送出。投递尽力而为，
//! 消费方自己吸收失败，不影响轮次结果；不需要观察事件的调用方传空闭包。

use serde_json::Value;

use crate::tools::ToolExecution;

/// 工具 Started 事件按调用顺序投递，Update/Ended 按实际完成顺序投递；
/// 落盘的 toolResult 按完成顺序追加，模型上下文则按调用顺序投影。
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// 本轮已保存的指令、Skills 目录或工具定义。
    ContextInjected { item: singularity_protocol::HistoryItem },
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
    /// 压缩开始、完成或失败；运行态与历史读取使用同一条目身份和结果。
    CompactionUpdated { item: singularity_protocol::HistoryItem },
    /// 工具开始执行。
    ToolExecutionStarted {
        item_id: String,
        tool_name: String,
        arguments: Value,
    },
    /// 工具执行中的累计输出，按 item_id 更新。
    ToolExecutionUpdate { item_id: String, partial_result: String },
    /// 工具执行完成；名称与参数见 Started。
    ToolExecutionEnded {
        item_id: String,
        execution: ToolExecution,
    },
    /// 一次 provider HTTP attempt 的生命周期观测，身份在请求开始时分配。
    ProviderAttempt {
        observation: singularity_protocol::RequestObservation,
    },
    /// 已落盘的用户消息：初始输入和注入输入共用这一条出口，消息 id 与持久历史
    /// 条目一致，客户端据此把实时条目和历史对上。
    UserMessage {
        entry_id: String,
        text: String,
        images: Vec<singularity_protocol::ImageAttachment>,
    },
    /// 等待回答的问题出现或结束；宿主据此重新发布会话状态。问题只在进程内保存。
    PendingQuestionChanged,
}
