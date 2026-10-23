use crate::{CHAT_COMPLETIONS_PATH, MODELS_PATH, RESPONSES_PATH};

/// Chat Completions 请求里表示思考开关的字段，由模型配置指定，不按提供方名或模型名猜测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingWireFormat {
    /// `thinking: {"type": "enabled" | "disabled"}`。
    ThinkingType,
    /// 顶层的 `enable_thinking` 布尔值。
    EnableThinking,
    /// 没有单独的开关字段，关闭时发送 `reasoning_effort: "none"`（部分兼容网关的做法）。
    ReasoningEffort,
}

impl ThinkingWireFormat {
    /// 全部取值，顺序就是错误提示里的列举顺序。
    pub(crate) const ALL: [Self; 3] = [Self::ThinkingType, Self::EnableThinking, Self::ReasoningEffort];

    /// 配置里没写 thinking_wire_format 时的默认值。
    pub(crate) const DEFAULT: Self = Self::ReasoningEffort;

    /// `config.json` 中的字段取值；配置解析和错误提示都用这里的名称。
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::ThinkingType => "thinking_type",
            Self::EnableThinking => "enable_thinking",
            Self::ReasoningEffort => "reasoning_effort",
        }
    }

    pub(crate) fn from_wire_name(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.wire_name() == value)
    }

    pub(crate) fn names() -> String {
        Self::ALL.map(Self::wire_name).join(", ")
    }
}

/// Chat Completions 请求里输出上限字段的默认名称。配置可以改成 `max_completion_tokens`，
/// 两种写法都有提供方在用，按配置发送，不按模型名或提供方猜测。
pub(crate) const DEFAULT_CHAT_OUTPUT_TOKENS_FIELD: &str = "max_tokens";

/// 去掉地址首尾空白和结尾的斜杠，其余部分（包括自定义路径）保持不变。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

pub(crate) fn chat_completions_endpoint(base_url: &str) -> String {
    format!("{}{CHAT_COMPLETIONS_PATH}", canonical_base_url(base_url))
}

pub(crate) fn responses_endpoint(base_url: &str) -> String {
    format!("{}{RESPONSES_PATH}", canonical_base_url(base_url))
}

pub(crate) fn models_endpoint(base_url: &str) -> String {
    format!("{}{MODELS_PATH}", canonical_base_url(base_url))
}
