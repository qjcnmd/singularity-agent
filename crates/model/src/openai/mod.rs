//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。
//!
//! 具体 Provider（协议选择与一次调用编排）也在本包内，见 [`provider`]；
//! 传输能力（HTTP client、SSE 帧、有界读取）由 transport 提供。

pub(crate) mod chat;
pub(crate) mod parse;
pub(crate) mod provider;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::*;
pub use provider::OpenAiProvider;
pub(crate) use responses::*;
pub(crate) use wire::{
    api_root, canonical_base_url, chat_completions_endpoint, models_endpoint, responses_endpoint,
};

use crate::config::selection::SelectedModel;
use crate::types::{ModelMessage, ProviderReasoningReplay};

/// 编码边界上的私有续接选择：只有身份等于当前 provider/model/协议的数据才进入
/// wire；不匹配时返回 `None`，调用方只省略私有载荷，公开内容仍按账本发送。
///
/// Chat 与 Responses 两个 encoder 共用这一条身份规则。账本消息不被复制或改写，
/// 被筛掉的续接材料仍留在会话里。
pub(crate) fn reasoning_replay_for<'a>(
    message: &'a ModelMessage,
    selection: &SelectedModel,
    provider_name: &str,
) -> Option<&'a ProviderReasoningReplay> {
    message.provider_reasoning_replay.as_ref().filter(|replay| {
        replay.is_for_model(provider_name, &selection.model_name, selection.api_protocol)
    })
}

pub(crate) struct ReasoningWireDecision<'a> {
    pub(crate) enabled: Option<bool>,
    pub(crate) effort: Option<&'a str>,
}

pub(crate) fn reasoning_wire_decision(selection: &SelectedModel) -> ReasoningWireDecision<'_> {
    ReasoningWireDecision {
        enabled: selection
            .reasoning_variant
            .as_ref()
            .map(|_| selection.reasoning_enabled),
        effort: selection.wire_reasoning_effort.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use crate::http_test_support::{test_config, test_selection};

    use super::parse_openai_responses_response;
    use super::responses::openai_responses_input;
    use crate::error::ModelErrorKind;
    use crate::provider::contract::ProviderApiProtocol;
    use crate::types::{ModelMessage, ModelRole};
    use serde_json::{Value, json};

    #[test]
    fn responses_native_tool_contract_preserves_recoverable_arguments() {
        let config = test_config("http://localhost/v1");
        for (id, name, arguments, count, rejection) in [
            ("call", "read", Some(json!({"path": "a"})), 1, None),
            ("call", "read", Some(json!("{\"path\":\"a\"}")), 1, None),
            ("call", "read", Some(json!("{")), 1, None),
            ("call", "read", Some(json!("[]")), 1, None),
            (
                "",
                "read",
                Some(json!("{")),
                1,
                Some("missing_tool_call_id"),
            ),
            ("call", "", Some(json!("{}")), 1, Some("missing_tool_name")),
            // 未知工具是结构合法的调用：它是否存在由工具注册表在 preflight
            // 判定，并以模型可见的失败结果回到主循环，协议层不得提前终结。
            ("call", "unknown", Some(json!("{")), 1, None),
            (
                "call",
                "read",
                Some(json!("{}")),
                2,
                Some("duplicate_tool_call_id"),
            ),
            ("call", "read", None, 1, Some("tool_call_arguments_missing")),
            (
                "call",
                "read",
                Some(Value::Null),
                1,
                Some("tool_call_arguments_type_invalid"),
            ),
        ] {
            let mut function = json!({"name": name});
            if let Some(arguments) = &arguments {
                function["arguments"] = arguments.clone();
            }
            function["type"] = json!("function_call");
            function["call_id"] = json!(id);
            let payload =
                json!({"id": "response", "status": "completed", "output": vec![function; count]});
            let result = parse_openai_responses_response(&config, payload, "model", None);
            if let Some(code) = rejection {
                let error = result.expect_err(code);
                assert_eq!(error.kind, ModelErrorKind::JsonSchemaViolation);
                assert_eq!(error.code.as_deref(), Some("provider_response_invalid"));
                assert!(error.to_string().contains(code), "{error}");
            } else {
                let response = result.expect("native calls remain available for tool validation");
                assert_eq!(response.tool_calls().len(), 1);
                if let Some(Value::String(raw)) = arguments {
                    assert_eq!(
                        response.tool_calls()[0].arguments,
                        serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw))
                    );
                }
            }
        }
    }

    #[test]
    fn responses_projects_non_leading_developer_to_system() {
        // 这几条消息没有续接材料，身份参数在这里不参与筛选。
        let (instructions, input) = openai_responses_input(
            &[
                ModelMessage::text(ModelRole::User, "first"),
                ModelMessage::text(ModelRole::Developer, "late instruction"),
                ModelMessage::text(ModelRole::User, "last"),
            ],
            &test_selection(ProviderApiProtocol::Responses),
            "test",
        );

        assert_eq!(instructions, None);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "system");
        assert_eq!(input[2]["role"], "user");
    }
}
