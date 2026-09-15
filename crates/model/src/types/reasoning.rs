use super::message::{ModelMessage, ModelRole};
use crate::ProviderApiProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub(crate) const CHAT_REASONING_FIELDS: &[&str] =
    &["reasoning_content", "reasoning", "reasoning_text"];

/// Provider 私有 reasoning 状态：可在适配器边界安全重放，但绝不展示或
/// 投影进公开会话、trace、评估或错误 schema。Rust 类型公开仅因 harness
/// 拥有 turn 之间的 reasoning-replay 边界。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderReasoningReplay {
    Chat {
        provider_name: String,
        model_name: String,
        /// 记录产生续接时的 reasoning 变体；无变体选择的
        /// 模型为 None。保留会话来源信息，不参与兼容判断或发送到 wire。
        reasoning_effort: Option<String>,
        tool_call_ids: Vec<String>,
        reasoning_content: String,
        /// 保留提供方返回的字段身份；旧会话使用 reasoning_content。
        #[serde(default = "default_reasoning_field")]
        reasoning_field: String,
        /// OpenAI 兼容端点返回的结构化签名或加密推理，不作为文本重建。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_details: Vec<Value>,
    },
    Responses {
        provider_name: String,
        model_name: String,
        reasoning_effort: Option<String>,
        tool_call_ids: Vec<String>,
        /// 完整 provider 输出序列逐字保留；适配器只追加后续的
        /// function_call_output 项。
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
                reasoning_field,
                reasoning_details,
            } => {
                validate_replay_binding(provider_name, model_name, reasoning_effort.as_deref())?;
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

    /// 判断续接数据所属的提供方、模型及协议；effort 不改变数据身份。
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

    /// 续接必须附着在产生它的 assistant 消息上，包括无工具的最终回复。
    pub(crate) fn validate_message(&self, message: &ModelMessage) -> Result<(), &'static str> {
        self.validate()?;
        // 绑定 ID 只借用比较：数量与顺序都必须与消息上的工具调用逐项一致。
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
    "reasoning_content".to_string()
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
    // 档位只记录来源；不据此推断提供方是否实际返回了续接数据。
    if let Some(effort) = reasoning_effort
        && (effort.is_empty()
            || effort
                .chars()
                .any(|character| character.is_whitespace() || character.is_control()))
    {
        return Err("provider reasoning replay binding is malformed");
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
    // 绑定 ID 已在 validate_replay_tool_call_ids 证明非空且唯一；逐项相等
    // 同时约束数量、顺序与唯一性，不需要再建第二个集合。
    if function_call_ids != tool_call_ids {
        return Err("Responses replay function_call ids do not match tool calls");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn earlier_chat_replay_restores_the_original_field_without_rewriting_the_session() {
        let replay: ProviderReasoningReplay = serde_json::from_value(serde_json::json!({
            "protocol": "chat", "provider_name": "provider", "model_name": "model",
            "reasoning_effort": null, "tool_call_ids": [], "reasoning_content": "saved continuation"
        }))
        .unwrap();
        assert!(replay.validate().is_ok());
        assert!(
            matches!(replay, ProviderReasoningReplay::Chat { reasoning_field, reasoning_details, .. }
            if reasoning_field == "reasoning_content" && reasoning_details.is_empty())
        );
    }

    fn function_call_item(call_id: &str) -> Value {
        serde_json::json!({
            "type": "function_call", "call_id": call_id, "name": "read", "arguments": "{}"
        })
    }

    fn responses_replay(ids: &[&str]) -> ProviderReasoningReplay {
        ProviderReasoningReplay::Responses {
            provider_name: "provider".into(),
            model_name: "model".into(),
            reasoning_effort: None,
            tool_call_ids: ids.iter().map(|id| (*id).to_string()).collect(),
            items: std::iter::once(serde_json::json!({"type": "reasoning", "id": "r"}))
                .chain(ids.iter().map(|id| function_call_item(id)))
                .collect(),
        }
    }

    fn assistant_with(ids: &[&str]) -> ModelMessage {
        ModelMessage {
            tool_calls: ids
                .iter()
                .map(|id| crate::ModelToolCall {
                    tool_call_id: (*id).to_string(),
                    tool_name: "read".into(),
                    arguments: serde_json::json!({}),
                })
                .collect(),
            ..ModelMessage::text(ModelRole::Assistant, "answer")
        }
    }

    /// 绑定顺序与数量必须与 assistant 消息逐项一致；重复的绑定 ID 在
    /// validate 入口就被拒绝，不需要下游再判一次唯一性。
    #[test]
    fn responses_replay_binding_requires_unique_ids_in_order() {
        let replay = responses_replay(&["first", "second"]);
        assert!(replay.validate().is_ok());
        assert!(
            replay
                .validate_message(&assistant_with(&["first", "second"]))
                .is_ok()
        );
        assert!(
            replay
                .validate_message(&assistant_with(&["second", "first"]))
                .is_err()
        );
        assert!(
            replay
                .validate_message(&assistant_with(&["first"]))
                .is_err()
        );
        assert!(replay.validate_message(&assistant_with(&[])).is_err());
        assert!(
            replay
                .validate_message(&ModelMessage::text(ModelRole::User, "answer"))
                .is_err()
        );

        assert!(responses_replay(&["same", "same"]).validate().is_err());

        let without_calls = responses_replay(&[]);
        assert!(without_calls.validate().is_ok());
        assert!(without_calls.validate_message(&assistant_with(&[])).is_ok());
        assert!(
            without_calls
                .validate_message(&assistant_with(&["first"]))
                .is_err()
        );
    }
}
