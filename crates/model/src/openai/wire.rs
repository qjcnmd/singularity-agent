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
    pub(crate) const ALL: [Self; 3] = [Self::ThinkingType, Self::EnableThinking, Self::ReasoningEffort];

    /// 没有声明 thinking_wire_format 时用的词形。
    pub(crate) const DEFAULT: Self = Self::ReasoningEffort;

    /// 配置解析、错误提示与目录发现共用的词形文本，词形只在这一处定义。
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

/// Chat Completions 请求里输出上限所用的 wire 字段名，也是配置没写时的默认值。
/// 配置里写什么就发什么，两个合法词形都有 provider 在用；不按模型名或提供方去猜。
pub(crate) const DEFAULT_CHAT_OUTPUT_TOKENS_FIELD: &str = "max_tokens";

/// 输入的规范形状：去掉首尾空白与结尾斜杠；不改变地址语义。
pub(crate) fn canonical_base_url(value: &str) -> &str {
    value.trim().trim_end_matches('/')
}

/// 已通过配置校验的 `base_url` 的 API 根：只剥掉路径末尾的三种已知端点。
/// 裸主机、版本根和自定义前缀都保留；推理与目录共用同一个根，不额外补版本段。
pub(crate) fn api_root(base_url: &str) -> String {
    // 配置入口已验证 URL；只处理路径，避免把主机名当作端点。
    let url = reqwest::Url::parse(canonical_base_url(base_url)).expect("base_url must be validated");
    let path = url.path().trim_end_matches('/');
    let root = KNOWN_ENDPOINTS.iter().find_map(|endpoint| path.strip_suffix(endpoint)).unwrap_or(path);
    format!("{}{root}", url.origin().ascii_serialization())
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
