use crate::{CHAT_COMPLETIONS_PATH, MODELS_PATH, RESPONSES_PATH};

/// 已知的 OpenAI 协议端点；`base_url` 写明了其中一个时先剥掉它。
const KNOWN_ENDPOINTS: [&str; 3] = [CHAT_COMPLETIONS_PATH, RESPONSES_PATH, MODELS_PATH];

/// Chat Completions reasoning 字段由模型目录显式选择；不解释任何
/// provider 或模型名来决定 wire 形状。
///
/// 词形只在 [`ThinkingWireFormat::wire_name`] 一处表示：配置解析、错误文案与
/// 目录发现都从那里取，枚举自身不参与序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingWireFormat {
    /// 既有 thinking: {"type": "enabled|disabled"} 字段。
    ThinkingType,
    /// 文档化此能力的 provider 使用顶层 enable_thinking 布尔。
    EnableThinking,
    /// 思考开关无独立 wire 字段：仅发送 reasoning_effort（部分
    /// OpenAI 兼容网关的 Chat 形状）。
    ReasoningEffort,
}

impl ThinkingWireFormat {
    /// 全部合法词形，顺序即错误提示中的列举顺序。
    pub(crate) const ALL: [Self; 3] = [
        Self::ThinkingType,
        Self::EnableThinking,
        Self::ReasoningEffort,
    ];

    /// 未声明 thinking_wire_format 时的词形。
    pub(crate) const DEFAULT: Self = Self::ReasoningEffort;

    /// 配置与目录共用的词形文本。
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::ThinkingType => "thinking_type",
            Self::EnableThinking => "enable_thinking",
            Self::ReasoningEffort => "reasoning_effort",
        }
    }

    pub(crate) fn from_wire_name(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|format| format.wire_name() == value)
    }

    /// 合法词形清单，供配置错误提示。
    pub(crate) fn names() -> String {
        Self::ALL.map(Self::wire_name).join(", ")
    }
}

/// Chat Completions 请求里输出上限使用的 wire 字段名，配置未声明时的取值。
///
/// 配置里写什么就发什么：DeepSeek、dashscope 等端点用 `max_tokens`，OpenAI
/// 官方推理模型用 `max_completion_tokens`。serializer 只发送已解析的名字，
/// 不按模型名或提供方猜测。
pub(crate) const DEFAULT_CHAT_OUTPUT_TOKENS_FIELD: &str = "max_tokens";

/// 输入的规范形状：去首尾空白与结尾斜杠；不改变地址语义。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

/// `base_url` 指向的 API 根：三种端点都由这一个根拼出。
///
/// 规则只有一条：已知端点先被剥掉，剩下的路径就是根，逐字使用——裸主机
/// （`https://api.deepseek.com`）的根就是它本身，版本根与自定义前缀都按用户
/// 写的那样使用。中间层不替任何消费者暗补版本段，否则同一个 `base_url`
/// 在推理与目录之间会有两个含义。
///
/// 结果始终是 `base_url` 的切片，因此返回借用；只有真正需要拥有端点 URL 的
/// 调用方才分配。
pub(crate) fn api_root(base_url: &str) -> &str {
    let base = canonical_base_url(base_url);
    KNOWN_ENDPOINTS
        .iter()
        .find_map(|endpoint| base.strip_suffix(endpoint))
        .filter(|root| !root.is_empty())
        .unwrap_or(base)
}

/// 将基础 URL 解析为兼容 OpenAI 的 Chat Completions 端点。
pub(crate) fn chat_completions_endpoint(base_url: &str) -> String {
    format!("{}{CHAT_COMPLETIONS_PATH}", api_root(base_url))
}

/// 将基础 URL 解析为兼容 OpenAI 的 Responses 端点。
pub(crate) fn responses_endpoint(base_url: &str) -> String {
    format!("{}{RESPONSES_PATH}", api_root(base_url))
}

/// 模型目录端点：与推理共用同一个根。
pub(crate) fn models_endpoint(base_url: &str) -> String {
    format!("{}{MODELS_PATH}", api_root(base_url))
}
