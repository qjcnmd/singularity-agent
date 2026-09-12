#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;
use crate::config::schema::{ConfiguredModel, ConfiguredProvider, ModelsFileReasoningVariant};
use crate::provider::Provider;
use crate::provider::runtime::OpenAiProviderConfig;
use crate::{ThinkingWireFormat, TurnRetryPolicy};
use std::collections::BTreeMap;

fn provider_config(provider: &str) -> OpenAiProviderConfig {
    OpenAiProviderConfig {
        provider_name: provider.to_string(),
        base_url: "https://example.invalid/v1".to_string(),
        api_key: "test-key".to_string(),
    }
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
    config: Option<OpenAiProviderConfig>,
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
            config: config.ok_or_else(super::missing_provider_auth_error),
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
            error.code.as_deref(),
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
    let unknown_provider = match resolve_model_selection(&snapshot, Some("other/gpt-x")) {
        Ok(_) => panic!("unknown provider must fail"),
        Err(error) => error,
    };
    assert_eq!(
        unknown_provider.code.as_deref(),
        Some("provider_selector_unknown_provider")
    );
    let unknown_model = match resolve_model_selection(&snapshot, Some("openai/nope")) {
        Ok(_) => panic!("unknown model must fail"),
        Err(error) => error,
    };
    assert_eq!(
        unknown_model.code.as_deref(),
        Some("provider_selector_unknown_model")
    );
}

/// 协议能力随选择冻结进快照：无变体时 reasoning_variant 为空、协议取自模型；
/// 选择思考档位不改变协议或上下文容量。
#[test]
fn selection_freezes_protocol_capabilities_into_snapshot() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let snapshot = catalog(
        "openai/gpt-x",
        "openai",
        "gpt-x",
        Some(provider_config("openai")),
    );
    let select = |selector| {
        let (config, model) = resolve_model_selection(&snapshot, Some(selector)).unwrap();
        OpenAiProvider::new(config.clone(), model, runtime.handle().clone()).unwrap()
    };

    let plain = select("openai/gpt-x");
    let model = plain.model_configuration();
    assert_eq!(model.provider, "openai");
    assert_eq!(model.model, "gpt-x");
    assert_eq!(model.reasoning_variant, None);
    assert_eq!(model.protocol, ProviderApiProtocol::OpenAiResponses);
    assert_eq!(model.retry, TurnRetryPolicy::default());

    let varianted = select("openai/gpt-x#high");
    let model = varianted.model_configuration();
    assert_eq!(model.reasoning_variant.as_deref(), Some("high"));
    assert_eq!(model.protocol, ProviderApiProtocol::OpenAiResponses);
    assert_eq!(model.capabilities.max_output_tokens, 4096);
    let disabled = select("openai/gpt-x#off");
    assert_eq!(
        disabled.model_configuration().reasoning_variant.as_deref(),
        Some("off")
    );
}

/// 未知或禁用的变体被拒绝，绝不回退到默认变体。
#[test]
fn selection_rejects_unknown_reasoning_variant() {
    let provider = provider_config("openai");
    let snapshot = catalog("openai/gpt-x", "openai", "gpt-x", Some(provider));
    let error = match resolve_model_selection(&snapshot, Some("openai/gpt-x#turbo")) {
        Ok(_) => panic!("unknown variant must fail"),
        Err(error) => error,
    };
    assert_eq!(
        error.code.as_deref(),
        Some("provider_selector_unknown_reasoning_variant")
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

    let input = ProviderConfigurationInput {
        provider_id: "openai".to_string(),
        display_name: Some("OpenAI compatible".to_string()),
        base_url: "https://example.invalid/v1".to_string(),
        models: vec![ProviderModelInput {
            model_id: "gpt-x".to_string(),
            display_name: Some("GPT X".to_string()),
            api_protocol: InputProtocol::Responses,
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(8_192),
            reasoning_variants: vec![ReasoningVariantInput {
                id: "high".to_string(),
                enabled: true,
                wire_effort: Some("high".to_string()),
            }],
            default_variant: Some("high".to_string()),
            thinking_wire_format: None,
        }],
    };
    let saved = owner.save_provider(input.clone()).expect("save provider");
    assert_eq!(saved.configuration, ModelConfigurationStatus::Missing);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));

    let configured = owner
        .set_api_key("openai", "top-secret-token")
        .expect("write credential");
    assert!(configured.credential_configured);
    let catalog = owner.redacted_catalog();
    assert_eq!(catalog.configuration, ModelConfigurationStatus::Ready);
    let frozen = owner.snapshot();
    assert_eq!(
        frozen.resolved_default_selector().as_deref(),
        Some("openai/gpt-x#high")
    );
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
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let before = std::fs::read(&config_path).expect("saved config");
    let mut invalid = input.clone();
    invalid.models[0].max_context_tokens = Some(1024);
    let error = owner
        .save_provider(invalid)
        .expect_err("output must fit the context window");
    assert!(error.message.contains("max_output_tokens must be smaller"));
    assert_eq!(std::fs::read(&config_path).unwrap(), before);
    assert_eq!(
        owner.redacted_catalog().configuration,
        ModelConfigurationStatus::Ready
    );

    let mut config: serde_json::Value = serde_json::from_slice(&before).unwrap();
    config["default_model"] = serde_json::json!("openai/gpt-x#missing");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(
        owner.redacted_catalog().configuration,
        ModelConfigurationStatus::Invalid
    );
    assert!(owner.snapshot().provider_for_selector(None).is_err());
    assert_eq!(
        frozen
            .provider_for_selector(None)
            .unwrap()
            .model_configuration()
            .reasoning_variant
            .as_deref(),
        Some("high"),
        "external edits only affect later snapshots, including before client construction"
    );

    config["default_model"] = serde_json::json!("openai/gpt-x#high");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut edited = input;
    edited.models[0].reasoning_variants.clear();
    edited.models[0].default_variant = None;
    let saved = owner
        .save_provider(edited)
        .expect("remove selected variant");
    assert_eq!(saved.configuration, ModelConfigurationStatus::Ready);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));
    assert!(owner.snapshot().provider_for_selector(None).is_ok());
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
    let catalog = owner.redacted_catalog();
    assert_eq!(
        catalog.configuration,
        singularity_protocol::ModelConfigurationStatus::Invalid
    );
    assert!(
        catalog
            .message
            .as_deref()
            .unwrap()
            .contains("line 1 column 2")
    );
}

#[test]
fn retired_replay_setting_is_readable_and_removed_on_save() {
    use crate::config::user::UserConfigModel;
    for value in ["disabled", "reasoning_content", "responses_items"] {
        let stored = serde_json::json!({
            "api_protocol": "chat", "tool_reasoning_history": value,
            "max_context_tokens": 32768, "max_output_tokens": 4096
        });
        let model: UserConfigModel = serde_json::from_value(stored).unwrap();
        let saved = serde_json::to_value(model).unwrap();
        assert!(saved.get("tool_reasoning_history").is_none());
        assert_eq!(saved["max_output_tokens"], 4096);
    }
}
