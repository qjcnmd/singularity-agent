use crate::{CHAT_COMPLETIONS_PATH, MODELS_PATH, RESPONSES_PATH};

/// 已知的 OpenAI 协议端点；`base_url` 末尾写着其中一个时，先把它剥掉。
const KNOWN_ENDPOINTS: [&str; 3] = [CHAT_COMPLETIONS_PATH, RESPONSES_PATH, MODELS_PATH];

/// Chat Completions 的 reasoning 字段形状由模型目录显式指定，不靠 provider 名或模型名
/// 去猜；枚举本身不参与序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingWireFormat {
    /// 用既有的 thinking: {"type": "enabled|disabled"} 字段表达开关。
    ThinkingType,
    /// 文档声明支持该能力的 provider，用顶层的 enable_thinking 布尔值表达开关。
    EnableThinking,
    /// 思考开关没有独立的 wire 字段，只能靠 reasoning_effort 表达（部分兼容网关的 Chat 形状）。
    ReasoningEffort,
}

impl ThinkingWireFormat {
    /// 全部合法词形，顺序就是错误提示里的列举顺序。
    pub(crate) const ALL: [Self; 3] = [
        Self::ThinkingType,
        Self::EnableThinking,
        Self::ReasoningEffort,
    ];

    /// 没有声明 thinking_wire_format 时用的词形。
    pub(crate) const DEFAULT: Self = Self::ReasoningEffort;

    /// 配置解析、错误提示与目录发现共用的词形文本，也是词形的唯一定义处。
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

    pub(crate) fn names() -> String {
        Self::ALL.map(Self::wire_name).join(", ")
    }
}

/// Chat Completions 请求里输出上限所用的 wire 字段名，也是配置没写时的默认值。
/// 配置里写什么就发什么（DeepSeek、dashscope 用 `max_tokens`，OpenAI 官方推理模型用
/// `max_completion_tokens`），不按模型名或提供方去猜。
pub(crate) const DEFAULT_CHAT_OUTPUT_TOKENS_FIELD: &str = "max_tokens";

/// 输入的规范形状：去掉首尾空白与结尾斜杠；不改变地址语义。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

/// `base_url` 里的 API 根：三种端点都从这一个根拼出来。规则只有一条：剥掉末尾已知的
/// 端点后剩下的路径就是根，逐字使用；裸主机、版本根和自定义前缀都按用户写的那样用。
/// 中间层不给调用方偷偷补版本段，否则同一个 `base_url` 在推理和目录之间会有两种含义。
/// 返回值始终是 `base_url` 的切片，只有需要持有端点 URL 的调用方才分配。
pub(crate) fn api_root(base_url: &str) -> &str {
    let base = canonical_base_url(base_url);
    KNOWN_ENDPOINTS
        .iter()
        .find_map(|endpoint| base.strip_suffix(endpoint))
        .filter(|root| !root.is_empty())
        .unwrap_or(base)
}

pub(crate) fn chat_completions_endpoint(base_url: &str) -> String {
    format!("{}{CHAT_COMPLETIONS_PATH}", api_root(base_url))
}

pub(crate) fn responses_endpoint(base_url: &str) -> String {
    format!("{}{RESPONSES_PATH}", api_root(base_url))
}

pub(crate) fn models_endpoint(base_url: &str) -> String {
    format!("{}{MODELS_PATH}", api_root(base_url))
}
