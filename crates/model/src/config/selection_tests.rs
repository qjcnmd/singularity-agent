#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::config::schema::{ConfiguredModel, ConfiguredProvider, ModelsFileReasoningVariant};
use crate::provider::Provider;
use crate::provider::contract::ProviderProtocolContract;
use crate::provider::runtime::OpenAiProviderConfig;
use crate::transport::OpenAiProvider;
use crate::{ThinkingWireFormat, TurnRetryPolicy};
use std::collections::BTreeMap;

/// 构造一个只用于选择接缝的 live provider：不触网，仅承载配置与 selected model。
fn live_provider(provider: &str) -> OpenAiProvider {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let config = OpenAiProviderConfig {
        provider_name: provider.to_string(),
        base_url: "https://example.invalid/v1".to_string(),
        api_key: "test-key".to_string(),
    };
    OpenAiProvider::new(config, runtime.handle().clone()).expect("provider")
}

fn configured_model(protocol: ProviderApiProtocol) -> ConfiguredModel {
    let mut reasoning_variants = BTreeMap::new();
    reasoning_variants.insert(
        "high".to_string(),
        ModelsFileReasoningVariant {
            enabled: true,
            wire_effort: Some("high".to_string()),
        },
    );
    reasoning_variants.insert(
        "off".to_string(),
        ModelsFileReasoningVariant {
            enabled: false,
            wire_effort: None,
        },
    );
    ConfiguredModel {
        protocol,
        max_context_tokens: Some(128_000),
        max_output_tokens: 4096,
        reasoning_variants,
        default_variant: None,
        thinking_wire_format: ThinkingWireFormat::ReasoningEffort,
        tool_reasoning_mode: ProviderToolReasoningMode::ReplayReasoningContent,
        supports_developer_role: true,
        supports_tool_choice: true,
        requires_reasoning_content_for_tool_calls: true,
        requires_assistant_content_for_tool_calls: false,
    }
}

fn catalog(
    default: &str,
    provider: &str,
    model: &str,
    instance: Option<OpenAiProvider>,
) -> ModelSelectionSnapshot {
    let mut models = BTreeMap::new();
    models.insert(
        model.to_string(),
        configured_model(ProviderApiProtocol::OpenAiResponses),
    );
    let mut providers = BTreeMap::new();
    providers.insert(
        provider.to_string(),
        ConfiguredProvider {
            provider: instance.ok_or_else(super::missing_provider_auth_error),
            models,
        },
    );
    ModelSelectionSnapshot {
        default_model: default.to_string(),
        providers,
    }
}

/// selector 拆分与组合互逆；空段视为缺省。
#[test]
fn selector_split_and_compose_are_inverse() {
    for selector in ["openai/gpt-x", "openai/gpt-x#high", "gpt-x", "gpt-x#off"] {
        let parts = split_model_selector(selector);
        let composed = compose_model_selector(
            parts.provider.unwrap_or("openai"),
            parts.model.unwrap_or(""),
            parts.effort,
        );
        // 无 provider 段时组合补默认 provider，其余段原样还原。
        if parts.provider.is_some() {
            assert_eq!(composed, selector, "round-trip {selector}");
        }
    }
    let parts = split_model_selector("openai/gpt-x#high");
    assert_eq!(parts.provider, Some("openai"));
    assert_eq!(parts.model, Some("gpt-x"));
    assert_eq!(parts.effort, Some("high"));
    assert_eq!(
        split_model_selector("openai/").model,
        None,
        "empty model is absent"
    );
    assert_eq!(
        split_model_selector("openai/gpt-x#").effort,
        None,
        "empty effort is absent"
    );
}

/// 严格解析拒绝缺 provider、缺分隔符与非法 id。
#[test]
fn parse_selector_rejects_malformed_input() {
    for bad in ["no-separator", "/gpt-x", "openai/"] {
        let error = parse_model_selector(bad)
            .err()
            .unwrap_or_else(|| panic!("{bad} must be rejected"));
        assert_eq!(
            error.error.code.as_deref(),
            Some("provider_selector_invalid"),
            "{bad}: {error}"
        );
    }
    let parsed = parse_model_selector("openai/gpt-x#high").expect("valid selector");
    assert_eq!(parsed.provider_name, "openai");
    assert_eq!(parsed.model_name, "gpt-x");
    assert_eq!(parsed.reasoning_effort, Some("high"));
}

/// 未知 provider 与未知模型分别落到稳定错误码，选择接缝不猜测。
#[test]
fn selection_rejects_unknown_provider_and_model() {
    let snapshot = catalog("openai/gpt-x", "openai", "gpt-x", None);
    let unknown_provider = match provider_for_selection(&snapshot, Some("other/gpt-x")) {
        Ok(_) => panic!("unknown provider must fail"),
        Err(error) => error,
    };
    assert_eq!(
        unknown_provider.error.code.as_deref(),
        Some("provider_selector_unknown_provider")
    );
    let unknown_model = match provider_for_selection(&snapshot, Some("openai/nope")) {
        Ok(_) => panic!("unknown model must fail"),
        Err(error) => error,
    };
    assert_eq!(
        unknown_model.error.code.as_deref(),
        Some("provider_selector_unknown_model")
    );
}

/// 协议能力随选择冻结进快照：无变体时 reasoning_variant 为空、协议取自模型；
/// 选定启用变体时携带变体并透传模型的 tool_reasoning_mode。
#[test]
fn selection_freezes_protocol_capabilities_into_snapshot() {
    let provider = live_provider("openai");
    let snapshot = catalog("openai/gpt-x", "openai", "gpt-x", Some(provider));

    let plain = provider_for_selection(&snapshot, Some("openai/gpt-x")).expect("select");
    let model = plain.model_configuration();
    assert_eq!(model.provider, "openai");
    assert_eq!(model.model, "gpt-x");
    assert_eq!(model.reasoning_variant, None);
    assert_eq!(model.protocol, ProviderApiProtocol::OpenAiResponses);
    assert_eq!(
        model.capabilities.tool_reasoning_mode,
        ProviderToolReasoningMode::Unspecified,
        "no variant selected leaves tool reasoning unspecified"
    );
    assert_eq!(model.retry, TurnRetryPolicy::default());

    let varianted =
        provider_for_selection(&snapshot, Some("openai/gpt-x#high")).expect("select variant");
    let model = varianted.model_configuration();
    assert_eq!(model.reasoning_variant.as_deref(), Some("high"));
    assert_eq!(
        model.capabilities.tool_reasoning_mode,
        ProviderToolReasoningMode::ReplayReasoningContent,
        "enabled variant透传模型声明的 tool reasoning mode"
    );

    let disabled = provider_for_selection(&snapshot, Some("openai/gpt-x#off")).expect("select off");
    assert_eq!(
        disabled
            .model_configuration()
            .capabilities
            .tool_reasoning_mode,
        ProviderToolReasoningMode::DisabledForToolCalls,
        "explicitly disabled variant forbids tool-call reasoning replay"
    );
}

/// 未知或禁用的变体被拒绝，绝不回退到默认变体。
#[test]
fn selection_rejects_unknown_reasoning_variant() {
    let provider = live_provider("openai");
    let snapshot = catalog("openai/gpt-x", "openai", "gpt-x", Some(provider));
    let error = match provider_for_selection(&snapshot, Some("openai/gpt-x#turbo")) {
        Ok(_) => panic!("unknown variant must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_selector_unknown_reasoning_variant")
    );
}

/// 快照的持久化形状是封闭的：camelCase、七键必填、未知字段拒绝。
#[test]
fn snapshot_wire_shape_is_closed() {
    let snapshot = ModelConfigurationSnapshot {
        provider: "openai_compatible".to_string(),
        model: "gpt-x".to_string(),
        reasoning_variant: None,
        protocol: ProviderApiProtocol::OpenAiChatCompletions,
        capabilities: ProviderProtocolContract::default(),
        credential_provenance: "auth.json:openai_compatible".to_string(),
        retry: TurnRetryPolicy::default(),
    };
    let value = serde_json::to_value(&snapshot).expect("serialize");
    assert_eq!(value["credentialProvenance"], "auth.json:openai_compatible");
    assert!(
        value.get("reasoningVariant").is_none(),
        "absent variant omitted"
    );
    assert_eq!(value["protocol"], "open_ai_chat_completions");
    let round: ModelConfigurationSnapshot = serde_json::from_value(value).expect("deserialize");
    assert_eq!(round, snapshot);

    let mut with_unknown = serde_json::to_value(&snapshot).expect("serialize");
    with_unknown
        .as_object_mut()
        .expect("object")
        .insert("surprise".to_string(), serde_json::json!(1));
    assert!(
        serde_json::from_value::<ModelConfigurationSnapshot>(with_unknown).is_err(),
        "unknown snapshot fields must be rejected"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn model_config_owner_saves_catalog_and_keeps_credentials_write_only() {
    use singularity_protocol::{
        ModelConfigurationStatus, ProviderApiProtocol as InputProtocol, ProviderConfigurationInput,
        ProviderModelInput, ReasoningVariantInput,
    };

    let home = tempfile::tempdir().expect("temporary config home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open_at(home.path().to_path_buf(), runtime.handle().clone());
    assert_eq!(
        owner.redacted_catalog().configuration,
        ModelConfigurationStatus::Missing
    );

    let saved = owner
        .save_provider(ProviderConfigurationInput {
            provider_id: "openai".to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            models: vec![ProviderModelInput {
                model_id: "gpt-x".to_string(),
                api_protocol: InputProtocol::Responses,
                max_context_tokens: Some(128_000),
                max_output_tokens: Some(8_192),
                reasoning_variants: vec![ReasoningVariantInput {
                    id: "high".to_string(),
                    enabled: true,
                    wire_effort: Some("high".to_string()),
                }],
                default_variant: Some("high".to_string()),
                tool_reasoning_history: Some("responses_items".to_string()),
            }],
            make_default: true,
        })
        .expect("save provider");
    assert_eq!(saved.configuration, ModelConfigurationStatus::Missing);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));

    let configured = owner
        .set_api_key("openai", "top-secret-token")
        .expect("write credential");
    assert!(configured.credential_configured);
    let catalog = owner.redacted_catalog();
    assert_eq!(catalog.configuration, ModelConfigurationStatus::Ready);
    assert_eq!(
        catalog.providers[0].models[0].default_variant.as_deref(),
        Some("high")
    );
    let serialized = serde_json::to_string(&catalog).expect("catalog serializes");
    assert!(!serialized.contains("top-secret-token"));
    assert!(!serialized.contains("api_key"));
    assert!(
        std::fs::read_to_string(home.path().join(crate::USER_AUTH_FILE_NAME))
            .expect("auth file")
            .contains("top-secret-token"),
        "the credential is persisted only in the private auth owner"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn model_config_owner_reports_invalid_persisted_configuration() {
    let home = tempfile::tempdir().expect("temporary config home");
    std::fs::write(home.path().join(crate::USER_CONFIG_FILE_NAME), "{invalid")
        .expect("invalid config fixture");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let owner =
        crate::ModelConfigOwner::open_at(home.path().to_path_buf(), runtime.handle().clone());
    assert_eq!(
        owner.redacted_catalog().configuration,
        singularity_protocol::ModelConfigurationStatus::Invalid
    );
}
