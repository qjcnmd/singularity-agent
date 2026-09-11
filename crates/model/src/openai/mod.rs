//! OpenAI Chat Completions/Responses 的请求投影、响应解码和 envelope 校验。

pub(crate) mod chat;
pub(crate) mod parse;
pub(crate) use parse::*;
pub(crate) mod responses;
pub(crate) mod wire;

pub(crate) use chat::*;
pub(crate) use responses::*;
pub(crate) use wire::OpenAiCompletion;
pub use wire::{chat_completions_endpoint, responses_endpoint};

use crate::provider::runtime::SelectedModel;

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

    use super::parse_openai_responses_response;
    use super::responses::openai_responses_input;
    use crate::error::ModelErrorKind;
    use crate::provider::runtime::OpenAiProviderConfig;
    use crate::types::{ModelMessage, ModelRole, ModelToolSchema, ModelTurnRequest};
    use serde_json::{Value, json};

    #[test]
    fn responses_native_tool_contract_preserves_recoverable_arguments() {
        let config = OpenAiProviderConfig {
            provider_name: "test".into(),
            base_url: "http://localhost/v1".into(),
            api_key: "test".into(),
        };
        let mut request = ModelTurnRequest::new(
            "request",
            vec![ModelMessage::text(ModelRole::User, "read a file")],
        );
        request.tools.push(ModelToolSchema {
            name: "read".into(),
            description: "read a file".into(),
            parameters_schema: json!({"type": "object", "required": ["path"]}),
        });
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
            ("call", "unknown", Some(json!("{")), 1, Some("unknown_tool")),
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
            let result = parse_openai_responses_response(&request, &config, payload, "model", None);
            if let Some(code) = rejection {
                let error = result.expect_err(code);
                assert_eq!(error.kind, ModelErrorKind::JsonSchemaViolation);
                assert_eq!(error.code.as_deref(), Some("provider_response_invalid"));
                assert!(error.to_string().contains(code), "{error}");
            } else {
                let response = result.expect("native calls remain available for tool validation");
                assert_eq!(response.tool_calls().len(), 1);
                if let Some(Value::String(raw)) = arguments {
                    assert_eq!(response.tool_calls()[0].raw_arguments, raw);
                }
            }
        }
    }

    #[test]
    fn responses_projects_non_leading_developer_to_system() {
        let (instructions, input) = openai_responses_input(&[
            ModelMessage::text(ModelRole::User, "first"),
            ModelMessage::text(ModelRole::Developer, "late instruction"),
            ModelMessage::text(ModelRole::User, "last"),
        ]);

        assert_eq!(instructions, None);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "system");
        assert_eq!(input[2]["role"], "user");
    }
}
