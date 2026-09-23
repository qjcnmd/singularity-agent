use super::message::{ModelMessage, ModelRole};
use crate::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub(crate) const DEFAULT_CHAT_REASONING_FIELD: &str = "reasoning_content";

pub(crate) const CHAT_REASONING_FIELDS: &[&str] =
    &[DEFAULT_CHAT_REASONING_FIELD, "reasoning", "reasoning_text"];

/// 提供方私有的推理状态：在适配器边界内可以安全重放，但绝不展示，也不进入公开会话、trace、
/// 评估或错误结构；类型公开只因为 harness 持有轮次之间的推理重放边界。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderReasoningReplay {
    Chat {
        provider_name: String,
        model_name: String,
        tool_call_ids: Vec<String>,
        reasoning_content: String,
        /// 保留提供方返回时用的字段名；旧会话用的是 reasoning_content。
        #[serde(default = "default_reasoning_field")]
        reasoning_field: String,
        /// OpenAI 兼容端点返回的结构化签名或加密推理；不当作文本重建。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_details: Vec<Value>,
    },
    Responses {
        provider_name: String,
        model_name: String,
        tool_call_ids: Vec<String>,
        /// 提供方的完整输出序列，逐字保留；适配器只往后追加 function_call_output 项。
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
    /// 在所属提供方边界校验这段不透明的续接数据，出错信息里不暴露私有内容。
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                tool_call_ids,
                reasoning_content,
                reasoning_field,
                reasoning_details,
                ..
            } => {
                validate_replay_binding(provider_name, model_name)?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                if !CHAT_REASONING_FIELDS.contains(&reasoning_field.as_str()) {
                    return Err("provider reasoning replay field is unsupported");
                }
                if reasoning_content.is_empty() && reasoning_details.is_empty() {
                    return Err("provider reasoning replay content is empty");
                }
                if !reasoning_details.iter().all(Value::is_object) {
                    return Err("provider reasoning replay details are malformed");
                }
            }
            Self::Responses {
                provider_name,
                model_name,
                tool_call_ids,
                items,
                ..
            } => {
                validate_replay_binding(provider_name, model_name)?;
                validate_replay_tool_call_ids(tool_call_ids)?;
                validate_responses_replay_items(items, tool_call_ids)?;
            }
        }
        Ok(())
    }

    /// 判断这段续接数据属于哪个提供方、模型和协议；档位不影响数据身份。
    pub(crate) fn is_for_model(
        &self,
        provider_name: &str,
        model_name: &str,
        protocol: ProviderApiProtocol,
    ) -> bool {
        let (provider, model) = self.model_identity();
        provider == provider_name
            && model == model_name
            && matches!(
                (self, protocol),
                (Self::Chat { .. }, ProviderApiProtocol::Chat)
                    | (Self::Responses { .. }, ProviderApiProtocol::Responses)
            )
    }

    /// 续接数据必须挂在产生它的 assistant 消息上，没有工具调用的最终回复也一样。
    pub(crate) fn validate_message(&self, message: &ModelMessage) -> Result<(), &'static str> {
        self.validate()?;
        // 这里只借用 ID 做比较：数量和顺序都必须和消息上的工具调用逐项一致。
        let bound = match self {
            Self::Chat { tool_call_ids, .. } | Self::Responses { tool_call_ids, .. } => {
                tool_call_ids
            }
        };
        let attached = bound.len() == message.tool_calls.len()
            && bound.iter().map(String::as_str).eq(message
                .tool_calls
                .iter()
                .map(|call| call.tool_call_id.as_str()));
        if message.role != ModelRole::Assistant || !attached {
            return Err("provider reasoning replay does not match its assistant message");
        }
        Ok(())
    }

    fn model_identity(&self) -> (&str, &str) {
        match self {
            Self::Chat {
                provider_name,
                model_name,
                ..
            }
            | Self::Responses {
                provider_name,
                model_name,
                ..
            } => (provider_name, model_name),
        }
    }
}

fn default_reasoning_field() -> String {
    DEFAULT_CHAT_REASONING_FIELD.to_string()
}

fn validate_replay_binding(provider_name: &str, model_name: &str) -> Result<(), &'static str> {
    for value in [provider_name, model_name] {
        if value.is_empty()
            || value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err("provider reasoning replay binding is malformed");
        }
    }
    Ok(())
}

fn validate_replay_tool_call_ids(ids: &[String]) -> Result<(), &'static str> {
    if ids.iter().any(|id| {
        id.is_empty()
            || id
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
    }) || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != ids.len()
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
    let mut function_call_ids: Vec<&str> = Vec::new();
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
                if id.chars().any(char::is_control) {
                    return Err("Responses reasoning item id is invalid");
                }
                reasoning_count = reasoning_count.saturating_add(1);
            }
            // 助手文本项允许出现，但不参与工具调用的绑定校验。
            "message" => {}
            "function_call" => {
                let call_id = object
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or("Responses function_call id is missing")?;
                if call_id.chars().any(char::is_control) {
                    return Err("Responses function_call id is invalid");
                }
                function_call_ids.push(call_id);
            }
            _ => return Err("Responses replay output item type is unsupported"),
        }
    }
    if reasoning_count == 0 {
        return Err("Responses reasoning replay item is missing");
    }
    // 绑定 ID 在 validate_replay_tool_call_ids 里已确认非空且唯一；逐项比较
    // 同时约束了数量、顺序和唯一性，不必再建一个集合。
    if function_call_ids != tool_call_ids {
        return Err("Responses replay function_call ids do not match tool calls");
    }
    Ok(())
}
