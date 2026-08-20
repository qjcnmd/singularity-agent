use super::{
    full_jitter_delay_ms, provider_retry_backoff, retry_after_delay, retry_backoff_window_ms,
};
use crate::{
    DEFAULT_MAX_CONTEXT_TOKENS, DEFAULT_MAX_OUTPUT_TOKENS, ModelMessage, ModelRole, ModelToolCall,
    ModelToolParseStatus, ModelTurnRequest, OpenAiProvider, OpenAiProviderConfig,
    ProviderApiProtocol, ProviderConfigSource, ProviderReasoningReplay, ProviderToolReasoningMode,
    SelectedModel, ThinkingWireFormat,
};
use crate::{PROVIDER_RETRY_BASE_BACKOFF_MS, PROVIDER_RETRY_MAX_BACKOFF_MS};
use reqwest::header::{HeaderMap, HeaderValue};
use std::time::Duration;

fn tool_result_message(call_id: &str, text: &str) -> ModelMessage {
    let mut message = ModelMessage::text(ModelRole::Tool, text);
    message.tool_call_id = Some(call_id.to_string());
    message
}

fn selected_provider() -> OpenAiProvider {
    let config = OpenAiProviderConfig {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        api_key: "sk-secret-value".to_string(),
        source: ProviderConfigSource::ProcessEnvironment,
        max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
        max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
    };
    OpenAiProvider::new(config)
        .expect("provider")
        .with_selected_model(SelectedModel {
            model_name: "gpt-test".to_string(),
            api_protocol: ProviderApiProtocol::OpenAiChatCompletions,
            max_context_tokens: Some(DEFAULT_MAX_CONTEXT_TOKENS),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            reasoning_variant: Some("on".to_string()),
            reasoning_enabled: true,
            wire_reasoning_effort: None,
            thinking_wire_format: ThinkingWireFormat::ThinkingType,
            tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
            supports_developer_role: true,
            supports_tool_choice: true,
            requires_reasoning_content_for_tool_calls: true,
            requires_assistant_content_for_tool_calls: false,
            capability_overrides: None,
        })
}

/// 请求侧校验：对于无推理历史的普通工具调用消息允许无绑定重放，仅拒绝重复冲突绑定。
#[test]
fn validate_reasoning_history_allows_unbound_legacy_tool_message() {
    let provider = selected_provider();
    let mut legacy = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
        tool_call_id: "legacy_call".to_string(),
        tool_name: "read".to_string(),
        arguments: serde_json::json!({"path": "x"}),
        raw_arguments: "{\"path\":\"x\"}".to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }]);
    legacy.content = "legacy".to_string();
    let mut fresh = ModelMessage::assistant_tool_calls(vec![ModelToolCall {
        tool_call_id: "fresh_call".to_string(),
        tool_name: "read".to_string(),
        arguments: serde_json::json!({"path": "y"}),
        raw_arguments: "{\"path\":\"y\"}".to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    }]);
    fresh.content = "fresh".to_string();
    let mut request = ModelTurnRequest::new(
        "validate_unbound",
        vec![
            ModelMessage::text(ModelRole::User, "hi"),
            legacy,
            tool_result_message("legacy_call", "legacy result"),
            fresh,
            tool_result_message("fresh_call", "fresh result"),
        ],
    );
    request.model_preferences.model_name = Some("provider/model#on".to_string());
    request.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
        provider_name: "openai_compatible".to_string(),
        model_name: "gpt-test".to_string(),
        reasoning_effort: "on".to_string(),
        tool_call_ids: vec!["fresh_call".to_string()],
        reasoning_content: "reasoning for fresh".to_string(),
    }];
    // legacy_call 无绑定 replay 是合法形态（v3 迁移兼容）。
    provider
        .validate_reasoning_history(&request)
        .expect("legacy tool message without replay must be accepted");
    // 重复绑定必须拒绝。
    let mut duplicated = request.clone();
    duplicated.provider_reasoning_history = vec![
        request.provider_reasoning_history[0].clone(),
        ProviderReasoningReplay::Chat {
            provider_name: "openai_compatible".to_string(),
            model_name: "gpt-test".to_string(),
            reasoning_effort: "on".to_string(),
            tool_call_ids: vec!["fresh_call".to_string()],
            reasoning_content: "another replay for fresh".to_string(),
        },
    ];
    assert!(provider.validate_reasoning_history(&duplicated).is_err());
}

#[test]
fn retry_after_parser_prefers_milliseconds_and_accepts_seconds_and_http_date() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after-ms", HeaderValue::from_static("120"));
    headers.insert("retry-after", HeaderValue::from_static("9"));
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(120))
    );

    headers.remove("retry-after-ms");
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_secs(9).min(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS),))
    );

    headers.insert(
        "retry-after",
        HeaderValue::from_static("Wed, 21 Oct 2030 07:28:00 GMT"),
    );
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS)),
    );
}

#[test]
fn retry_after_parser_falls_back_for_invalid_and_clamps_large_values() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("-1"));
    assert_eq!(retry_after_delay(&headers), None);

    headers.insert("retry-after", HeaderValue::from_static("999999"));
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS))
    );

    headers.insert("retry-after-ms", HeaderValue::from_static("not-a-duration"));
    assert_eq!(
        retry_after_delay(&headers),
        Some(Duration::from_millis(PROVIDER_RETRY_MAX_BACKOFF_MS))
    );
}

#[test]
fn provider_retry_backoff_uses_full_jitter_window() {
    for retry_count in 1..=6 {
        let window = retry_backoff_window_ms(retry_count);
        assert_eq!(
            window,
            (PROVIDER_RETRY_BASE_BACKOFF_MS.saturating_mul(1_u64 << (retry_count - 1)))
                .min(PROVIDER_RETRY_MAX_BACKOFF_MS)
        );
        assert_eq!(full_jitter_delay_ms(retry_count, 0), 0);
        assert_eq!(full_jitter_delay_ms(retry_count, window), window);
        assert_eq!(full_jitter_delay_ms(retry_count, window + 1), 0);

        let delay = provider_retry_backoff(retry_count);
        assert!(
            delay <= Duration::from_millis(window),
            "retry {retry_count} produced {delay:?} outside full-jitter window"
        );
    }
    assert_eq!(retry_backoff_window_ms(8), PROVIDER_RETRY_MAX_BACKOFF_MS);
    assert_eq!(
        retry_backoff_window_ms(u32::MAX),
        PROVIDER_RETRY_MAX_BACKOFF_MS
    );
}
