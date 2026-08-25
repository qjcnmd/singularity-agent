use super::message::{ModelMessage, ModelRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// tool 推理内容是否符合模型提供方的 tool call 历史契约。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderToolReasoningMode {
    #[default]
    Unspecified,
    DisabledForToolCalls,
    /// 适配器必须在每条 assistant 工具调用续接上保留 Chat Completions
    /// `reasoning_content`。
    ReplayReasoningContent,
    /// 适配器必须逐字保留 Responses reasoning 输出项。
    ReplayResponsesItems,
}

/// Provider 私有 reasoning 状态：可在适配器边界安全重放，但绝不展示或
/// 投影进公开会话、trace、评估或错误 schema。Rust 类型公开仅因 harness
/// 拥有 turn 之间的 reasoning-replay 边界。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderReasoningReplay {
    Chat {
        provider_name: String,
        model_name: String,
        /// 绑定构造 replay 时请求侧实际选定的 reasoning 变体；无变体选择的
        /// 模型为 `None`。仅作绑定标识，不发送到 wire。
        reasoning_effort: Option<String>,
        tool_call_ids: Vec<String>,
        reasoning_content: String,
    },
    Responses {
        provider_name: String,
        model_name: String,
        reasoning_effort: Option<String>,
        tool_call_ids: Vec<String>,
        /// 完整 provider 输出序列逐字保留；适配器只追加后续的
        /// `function_call_output` 项。
        items: Vec<Value>,
    },
}

impl fmt::Debug for ProviderReasoningReplay {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("ProviderReasoningReplay");
        match self {
            Self::Chat {
                tool_call_ids,
                reasoning_content,
                ..
            } => {
                debug
                    .field("protocol", &"chat")
                    .field("tool_call_count", &tool_call_ids.len())
                    .field("reasoning_content_len", &reasoning_content.len());
            }
            Self::Responses {
                tool_call_ids,
                items,
                ..
            } => {
                debug
                    .field("protocol", &"responses")
                    .field("tool_call_count", &tool_call_ids.len())
                    .field("output_item_count", &items.len())
                    .field("reasoning_item_present", &true);
            }
        }
        debug.finish()
    }
}

impl ProviderReasoningReplay {
    /// 在所属 provider 边界校验 opaque replay，且错误中不暴露私有 payload。
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                reasoning_content,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort.as_deref())?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                if reasoning_content.is_empty() {
                    return Err("provider reasoning replay content is empty");
                }
            }
            Self::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                tool_call_ids,
                items,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort.as_deref())?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                validate_responses_replay_items(items, tool_call_ids)?;
            }
        }
        Ok(())
    }

    /// 在非模型边界只返回校验结果。
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// 检查私有 replay 绑定而不暴露其 opaque payload；持久化 thread
    /// 切换 provider/model 时 agent 使用此门。
    pub fn is_compatible_with(
        &self,
        provider_name: &str,
        model_name: &str,
        reasoning_variant: Option<&str>,
        mode: ProviderToolReasoningMode,
    ) -> bool {
        self.validate_for(provider_name, model_name, reasoning_variant, mode)
            .is_ok()
    }

    /// 对照一个选定的 provider/model/变体与模式校验 replay。
    /// 变体比较为 Option 语义：双侧同为空或 `Some` 相等即过。
    pub(crate) fn validate_for(
        &self,
        provider_name: &str,
        model_name: &str,
        reasoning_variant: Option<&str>,
        mode: ProviderToolReasoningMode,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let (replay_provider, replay_model, replay_variant) = self.binding_internal();
        if replay_provider != provider_name
            || replay_model != model_name
            || replay_variant != reasoning_variant
            || self.mode_internal() != mode
        {
            return Err("provider reasoning replay binding does not match selected model");
        }
        Ok(())
    }

    fn binding_internal(&self) -> (&str, &str, Option<&str>) {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                reasoning_effort,
                ..
            }
            | Self::Responses {
                provider_name,
                model_name,
                reasoning_effort,
                ..
            } => (provider_name, model_name, reasoning_effort.as_deref()),
        }
    }

    /// replay 是否按序绑定到全部给定 tool-call id。
    pub fn matches_tool_call_ids(&self, ids: &[String]) -> bool {
        match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids == ids
            }
        }
    }

    /// replay 是否包含某 tool-call id（不暴露 id 列表）。
    pub fn has_tool_call_id(&self, id: &str) -> bool {
        match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids.iter().any(|candidate| candidate == id)
            }
        }
    }

    /// 给定模型历史中是否恰好一条 assistant 消息携带本 replay 的
    /// 有序 tool-call 绑定。
    pub fn is_bound_to_messages(&self, messages: &[ModelMessage]) -> bool {
        self.bound_assistant_count(messages) == 1
    }

    /// 统计携带本 replay 精确有序绑定的 assistant 工具调用消息条数。
    pub fn bound_assistant_count(&self, messages: &[ModelMessage]) -> usize {
        messages
            .iter()
            .filter(|message| {
                message.role == ModelRole::Assistant
                    && self.matches_tool_call_ids(
                        &message
                            .tool_calls
                            .iter()
                            .map(|call| call.tool_call_id.clone())
                            .collect::<Vec<_>>(),
                    )
            })
            .count()
    }

    /// 返回 model crate 内部的协议专属 reasoning 模式。
    pub(crate) fn mode_internal(&self) -> ProviderToolReasoningMode {
        match self {
            Self::Chat { .. } => ProviderToolReasoningMode::ReplayReasoningContent,
            Self::Responses { .. } => ProviderToolReasoningMode::ReplayResponsesItems,
        }
    }
}

fn validate_replay_binding(
    provider_name: &str,
    model_name: &str,
    reasoning_effort: Option<&str>,
) -> Result<(), &'static str> {
    for value in [provider_name, model_name] {
        if value.is_empty()
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err("provider reasoning replay binding is malformed");
        }
    }
    // 无变体选择的模型绑定 `None` 是合法的；有变体时 `"off"` 是真正的禁用
    // 变体，不能作为 replay 绑定存活。
    if let Some(effort) = reasoning_effort {
        if effort.is_empty()
            || effort
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err("provider reasoning replay binding is malformed");
        }
        if effort == "off" {
            return Err("provider reasoning replay cannot use disabled variant");
        }
    }
    Ok(())
}

fn validate_replay_tool_call_ids(ids: &[String]) -> Result<(), &'static str> {
    if ids.is_empty()
        || ids.iter().any(|id| {
            id.is_empty()
                || id
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        })
        || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
    {
        return Err("provider reasoning replay tool-call identity is invalid");
    }
    Ok(())
}

fn validate_responses_replay_items(
    items: &[Value],
    tool_call_ids: &[String],
) -> Result<(), &'static str> {
    if items.is_empty() {
        return Err("Responses reasoning replay output is empty");
    }
    let mut reasoning_count = 0usize;
    let mut function_call_ids = Vec::new();
    for item in items {
        let object = item
            .as_object()
            .ok_or("Responses replay output item is not an object")?;
        let item_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or("Responses replay output item type is missing")?;
        match item_type {
            "reasoning" => {
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses reasoning item id is missing")?;
                if id.chars().any(|character| character.is_control()) {
                    return Err("Responses reasoning item id is invalid");
                }
                reasoning_count = reasoning_count.saturating_add(1);
            }
            "message" => {}
            "function_call" => {
                let call_id = object
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses function_call id is missing")?;
                if call_id.chars().any(|character| character.is_control()) {
                    return Err("Responses function_call id is invalid");
                }
                function_call_ids.push(call_id.to_string());
            }
            _ => return Err("Responses replay output item type is unsupported"),
        }
    }
    if reasoning_count == 0 {
        return Err("Responses reasoning replay item is missing");
    }
    if function_call_ids != tool_call_ids
        || function_call_ids
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != function_call_ids.len()
    {
        return Err("Responses replay function_call ids do not match tool calls");
    }
    Ok(())
}
