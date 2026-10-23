//! 模型调用的结果、流事件与协议校验。

pub mod contract;

/// 面向 Agent 边界的 provider 流数据：已规范化，且不含敏感内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStreamEvent {
    /// 工具调用名称或参数的生成增量，仅用于记录生成开始时间。
    ToolCallDelta,
    /// 模型响应里对外可见的正文增量。
    OutputTextDelta { delta: String },
    /// 提供方公开的思考文本增量：Chat 的 reasoning 增量和 Responses 的
    /// reasoning summary 增量都从这里发出，不含 provider 私有的回放材料。
    ReasoningTextDelta { delta: String },
}
