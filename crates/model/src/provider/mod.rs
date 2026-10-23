//! 模型调用的结果、流事件与协议校验。

pub mod contract;

/// 流式解码过程中发给调用方的增量事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 工具调用名称或参数的生成增量，仅用于记录生成开始时间。
    ToolCallDelta,
    /// 模型响应里对外可见的正文增量。
    OutputTextDelta { delta: String },
    /// 模型公开的思考文本增量（Chat 的 reasoning 增量或 Responses 的 reasoning summary 增量），不含加密推理等提供方私有字段。
    ReasoningTextDelta { delta: String },
}
