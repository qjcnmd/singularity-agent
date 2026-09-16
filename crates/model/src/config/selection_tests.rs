#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::provider::Provider;

fn config(default: &str, credential: bool) -> UserConfigData {
    UserConfigData {
        config: serde_json::from_value(serde_json::json!({
            "default_model": default,
            "providers": {"openai": {"base_url": "https://example.invalid/v1", "models": {
                "gpt-x": {"api_protocol": "responses", "max_context_tokens": 128000, "max_output_tokens": 4096,
                    "supports_developer_role": true, "requires_reasoning_content_for_tool_calls": true,
                    "default_variant": "off",
                    "reasoning_variants": {"high": {"enabled": true, "wire_effort": "high"}, "off": {"enabled": false}}},
                "plain": {"api_protocol": "responses", "max_context_tokens": 128000, "max_output_tokens": 4096}
            }}}
        })).unwrap(),
        auth: serde_json::from_value(if credential {
            serde_json::json!({"providers": {"openai": {"api_key": "test-key"}}})
        } else { serde_json::json!({}) }).unwrap(),
    }
}

fn snapshot(
    default: &str,
    credential: bool,
    runtime: &tokio::runtime::Runtime,
) -> ProviderConfigSnapshot {
    let home = tempfile::tempdir().unwrap();
    let data = config(default, credential);
    std::fs::write(
        home.path().join("config.json"),
        serde_json::to_vec(&data.config).unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::to_vec(&data.auth).unwrap(),
    )
    .unwrap();
    ProviderConfigSnapshot::capture(home.path(), runtime.handle().clone())
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let snapshot = snapshot("openai/gpt-x", false, &runtime);
    let unknown_provider = snapshot.validate_selector(Some("other/gpt-x")).unwrap_err();
    assert_eq!(
        unknown_provider.code.as_deref(),
        Some("provider_selector_unknown_provider")
    );
    let unknown_model = snapshot.validate_selector(Some("openai/nope")).unwrap_err();
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
    let snapshot = snapshot("openai/gpt-x", true, &runtime);
    let select = |selector| snapshot.provider_for_selector(Some(selector)).unwrap();

    let plain = select("openai/plain");
    let model = plain.model_configuration();
    assert_eq!(model.provider, "openai");
    assert_eq!(model.model, "plain");
    assert_eq!(model.reasoning_variant, None);
    assert_eq!(model.protocol, ProviderApiProtocol::Responses);

    let varianted = select("openai/gpt-x#high");
    let model = varianted.model_configuration();
    assert_eq!(model.reasoning_variant.as_deref(), Some("high"));
    assert_eq!(model.protocol, ProviderApiProtocol::Responses);
    assert_eq!(model.max_output_tokens, 4096);
    let disabled = select("openai/gpt-x#off");
    assert_eq!(
        disabled.model_configuration().reasoning_variant.as_deref(),
        Some("off")
    );
}

/// 未知或禁用的变体被拒绝，绝不回退到默认变体。
#[test]
fn selection_rejects_unknown_reasoning_variant() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let snapshot = snapshot("openai/gpt-x", true, &runtime);
    let error = snapshot
        .validate_selector(Some("openai/gpt-x#turbo"))
        .unwrap_err();
    assert_eq!(
        error.code.as_deref(),
        Some("provider_selector_unknown_reasoning_variant")
    );
}

#[test]
fn explicit_selection_works_when_the_default_provider_is_incomplete() {
    let home = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut data = config("unfinished/model", true);
    let provider = data.config.providers["openai"].clone();
    data.config.providers.insert("unfinished".into(), provider);
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::to_vec(&data.auth).unwrap(),
    )
    .unwrap();
    let owner = ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    for default in ["unfinished/model", "unfinished/gpt-x", "malformed"] {
        data.config.default_model = Some(default.into());
        std::fs::write(
            home.path().join("config.json"),
            serde_json::to_vec(&data.config).unwrap(),
        )
        .unwrap();
        let snapshot = owner.snapshot();
        assert!(snapshot.validate_selector(None).is_err());
        snapshot
            .validate_selector(Some("openai/gpt-x#high"))
            .unwrap();
        let catalog = owner.redacted_catalog();
        assert!(
            catalog
                .providers
                .iter()
                .any(|provider| provider.provider_id == "openai" && provider.credential_configured)
        );
    }
}

#[test]
fn model_config_owner_saves_catalog_and_keeps_credentials_write_only() {
    use singularity_protocol::{
        ModelConfigurationInput, ModelConfigurationStatus, ProviderConfigurationInput,
        ReasoningVariant,
    };

    let home = tempfile::tempdir().expect("temporary config home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    assert_eq!(
        owner.redacted_catalog().configuration,
        ModelConfigurationStatus::Missing
    );

    let input = ProviderConfigurationInput {
        provider_id: "openai".to_string(),
        display_name: Some("OpenAI compatible".to_string()),
        base_url: "https://example.invalid/v1".to_string(),
        models: vec![ModelConfigurationInput {
            model_id: "gpt-x".to_string(),
            display_name: Some("GPT X".to_string()),
            api_protocol: Some("responses".into()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(8_192),
            reasoning_variants: vec![ReasoningVariant {
                id: "high".to_string(),
                enabled: true,
                wire_effort: Some("high".to_string()),
            }],
            default_variant: Some("high".to_string()),
            thinking_wire_format: None,
            chat_output_tokens_field: None,
        }],
    };
    owner
        .save_provider(input.clone(), None)
        .expect("save provider");
    let saved = owner.redacted_catalog();
    assert_eq!(saved.configuration, ModelConfigurationStatus::Missing);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));

    owner
        .set_api_key("openai", "top-secret-token")
        .expect("write credential");
    // 证明凭据已生效的是 catalog 读取路径，而非命令回执。
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
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert!(config.get("default_provider").is_none());
    for protocol in [None, Some("unsupported")] {
        let mut raw = config.clone();
        let raw_model = &mut raw["providers"]["openai"]["models"]["gpt-x"];
        raw_model["api_protocol"] = serde_json::json!(protocol);
        raw_model["supports_developer_role"] = serde_json::json!(false);
        raw_model["supports_tool_choice"] = serde_json::json!(false);
        raw_model["requires_reasoning_content_for_tool_calls"] = serde_json::json!(true);
        raw_model["requires_assistant_content_for_tool_calls"] = serde_json::json!(true);
        std::fs::write(&config_path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let catalog = owner.redacted_catalog();
        assert_eq!(catalog.configuration, ModelConfigurationStatus::Invalid);
        assert_eq!(
            catalog.providers[0].models[0].api_protocol.as_deref(),
            protocol
        );
        let mut edit = input.clone();
        edit.models = catalog.providers[0].models.clone();
        let before = std::fs::read(&config_path).unwrap();
        assert!(owner.save_provider(edit.clone(), None).is_err());
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        edit.models[0].api_protocol = Some("chat".into());
        owner
            .save_provider(edit, None)
            .expect("repair protocol through editor contract");
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
        let model = &saved["providers"]["openai"]["models"]["gpt-x"];
        assert_eq!(model["supports_developer_role"], false);
        assert_eq!(model["supports_tool_choice"], false);
        assert_eq!(model["requires_reasoning_content_for_tool_calls"], true);
        assert_eq!(model["requires_assistant_content_for_tool_calls"], true);
        assert_eq!(
            owner.redacted_catalog().providers[0].models[0]
                .api_protocol
                .as_deref(),
            Some("chat")
        );
    }
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut alternate = input.clone();
    alternate.provider_id = "alternate".into();
    owner
        .save_provider(alternate, None)
        .expect("save another provider");
    assert_eq!(
        owner.redacted_catalog().default_selector.as_deref(),
        Some("openai/gpt-x"),
        "saving another provider preserves the valid default even without default_provider"
    );
    let before = std::fs::read(&config_path).expect("saved config");
    let mut invalid = input.clone();
    invalid.models[0].max_context_tokens = Some(1024);
    let error = owner
        .save_provider(invalid, None)
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
    owner
        .save_provider(edited, None)
        .expect("remove selected variant");
    let saved = owner.redacted_catalog();
    assert_eq!(saved.configuration, ModelConfigurationStatus::Ready);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));
    assert!(owner.snapshot().provider_for_selector(None).is_ok());
}

/// 保存只规范输入形状（去空白与结尾斜杠）：写明的端点原样保留，由
/// `openai::wire` 一处解释；目录发现用同一个解释取根。
#[test]
fn saved_base_url_keeps_the_endpoint_the_user_gave() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    let stored = "https://example.invalid/api/paas/v4/chat/completions";
    let input = ProviderConfigurationInput {
        provider_id: "custom".to_string(),
        display_name: None,
        base_url: format!("  {stored}/  "),
        models: vec![ModelConfigurationInput {
            model_id: "m".to_string(),
            display_name: None,
            api_protocol: Some("chat".into()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(4_096),
            reasoning_variants: Vec::new(),
            default_variant: None,
            thinking_wire_format: None,
            chat_output_tokens_field: None,
        }],
    };
    owner.save_provider(input, None).expect("save provider");
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path().join(crate::USER_CONFIG_FILE_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(config["providers"]["custom"]["base_url"], stored);
    assert_eq!(owner.redacted_catalog().providers[0].base_url, stored);
    let request = owner
        .model_discovery_request("custom", stored, Some("key"))
        .expect("discovery request")
        .build()
        .expect("build discovery request");
    assert_eq!(
        request.url().as_str(),
        "https://example.invalid/api/paas/v4/models"
    );
}

#[test]
fn model_config_owner_reports_invalid_persisted_configuration() {
    let home = tempfile::tempdir().expect("temporary config home");
    std::fs::write(home.path().join(crate::USER_CONFIG_FILE_NAME), "{invalid")
        .expect("invalid config fixture");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let owner = crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
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

/// 配置与密钥是两个文件、两种职责：各自只读写需要的那一份。
/// 密钥更新保留其他提供方的条目且不改 config.json，未提交新密钥的配置编辑
/// 不写 auth.json，需要密钥的操作也不受 config.json 缺失或损坏影响。
#[test]
fn credentials_and_config_are_read_and_written_per_file() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let auth_path = home.path().join(crate::USER_AUTH_FILE_NAME);
    let stored_key = |provider_id: &str| -> Option<String> {
        let auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
        auth["providers"][provider_id]["api_key"]
            .as_str()
            .map(str::to_string)
    };
    let provider = |provider_id: &str| ProviderConfigurationInput {
        provider_id: provider_id.to_string(),
        display_name: None,
        base_url: "https://example.invalid/v1".to_string(),
        models: vec![ModelConfigurationInput {
            model_id: "model".to_string(),
            display_name: None,
            api_protocol: Some("chat".into()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(8_192),
            reasoning_variants: Vec::new(),
            default_variant: None,
            thinking_wire_format: None,
            chat_output_tokens_field: None,
        }],
    };

    owner
        .save_provider(provider("one"), Some("key-one"))
        .expect("save first provider");
    owner
        .save_provider(provider("two"), Some("key-two"))
        .expect("save second provider");
    assert_eq!(stored_key("one").as_deref(), Some("key-one"));
    assert_eq!(stored_key("two").as_deref(), Some("key-two"));

    let config_before = std::fs::read(&config_path).unwrap();
    owner.set_api_key("one", "rotated").expect("rotate one key");
    assert_eq!(stored_key("one").as_deref(), Some("rotated"));
    assert_eq!(
        stored_key("two").as_deref(),
        Some("key-two"),
        "updating one credential keeps every other provider entry"
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        config_before,
        "a credential update never rewrites config.json"
    );

    let auth_before = std::fs::read(&auth_path).unwrap();
    owner
        .save_provider(provider("two"), None)
        .expect("edit provider config without a new key");
    assert_eq!(
        std::fs::read(&auth_path).unwrap(),
        auth_before,
        "a config edit without a new key never writes auth.json"
    );

    // auth.json 可以先于 config.json 独立存在：密钥读取只依据 auth 本身。
    std::fs::remove_file(&config_path).unwrap();
    owner
        .set_api_key("one", "rotated-again")
        .expect("update key while config.json is absent");
    assert_eq!(stored_key("two").as_deref(), Some("key-two"));
    std::fs::write(&config_path, "{ not json").unwrap();
    owner
        .set_api_key("one", "rotated-under-broken-config")
        .expect("a broken config.json does not block a credential update");
    assert_eq!(
        stored_key("one").as_deref(),
        Some("rotated-under-broken-config")
    );
}

/// Chat 输出上限字段：配置里写什么就发什么，缺省 `max_tokens`。
///
/// Responses 不使用该字段，声明即配置错误——避免把无效果的开关静默留在配置里。
#[test]
fn chat_output_tokens_field_is_declared_per_model_and_scoped_to_chat() {
    use crate::provider::Provider;

    let model = |extra: serde_json::Value| -> UserConfigModel {
        let mut value = serde_json::json!({
            "api_protocol": "chat",
            "max_context_tokens": 128000,
            "max_output_tokens": 4096,
        });
        if let (Some(base), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        serde_json::from_value(value).unwrap()
    };

    // 未声明：发送 max_tokens。
    let default = resolve_model_definition(&model(serde_json::json!({})), "m", None).unwrap();
    assert_eq!(default.chat_output_tokens_field, "max_tokens");

    // 声明什么就发什么，不限于两个已知词形。
    for declared in ["max_completion_tokens", "max_new_tokens"] {
        let resolved = resolve_model_definition(
            &model(serde_json::json!({"chat_output_tokens_field": declared})),
            "m",
            None,
        )
        .unwrap();
        assert_eq!(resolved.chat_output_tokens_field, declared);
    }

    // 空串等同于未声明。
    let blank = resolve_model_definition(
        &model(serde_json::json!({"chat_output_tokens_field": ""})),
        "m",
        None,
    )
    .unwrap();
    assert_eq!(blank.chat_output_tokens_field, "max_tokens");

    // 未声明写在 Responses 模型上不是错误；空串等同于未声明，因此同样不是。
    for undeclared in [None, Some(String::new())] {
        let mut responses = model(serde_json::json!({"api_protocol": "responses"}));
        responses.chat_output_tokens_field = undeclared;
        resolve_model_definition(&responses, "m", None).unwrap();
    }

    // 在 Responses 上声明该字段会静默无效，因此明确拒绝。
    let mut responses = model(serde_json::json!({"api_protocol": "responses"}));
    responses.chat_output_tokens_field = Some("max_completion_tokens".into());
    let error = resolve_model_definition(&responses, "m", None)
        .err()
        .expect("Responses must reject the Chat-only field");
    assert_eq!(
        error.code.as_deref(),
        Some("provider_configuration_invalid")
    );

    // Provider 快照把声明带到执行客户端。
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let data = config("openai/plain", true);
    let snapshot = snapshot("openai/plain", true, &runtime);
    assert_eq!(
        snapshot
            .provider_for_selector(Some("openai/plain"))
            .unwrap()
            .model_configuration()
            .max_output_tokens,
        data.config.providers["openai"].models["plain"]
            .max_output_tokens
            .unwrap(),
        "the resolved model keeps its configured output limit"
    );
}

/// Chat 输出上限字段经由配置读写往返，且目录读回后仍能带回表单。
///
/// 表单没有该开关的控件，所以「保存 → 读回 → 再保存」必须原样保留它；被
/// 静默改写的配置会让用户无法在官方 chat 端点上使用推理模型。
#[test]
fn the_chat_output_tokens_field_round_trips_through_saved_configuration() {
    let home = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};
    let model = |field: Option<&str>| ModelConfigurationInput {
        model_id: "reasoner".to_string(),
        display_name: None,
        api_protocol: Some("chat".into()),
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(4_096),
        reasoning_variants: Vec::new(),
        default_variant: None,
        thinking_wire_format: None,
        chat_output_tokens_field: field.map(str::to_string),
    };
    let provider = |field: Option<&str>| ProviderConfigurationInput {
        provider_id: "official".to_string(),
        display_name: None,
        base_url: "https://example.invalid/v1".to_string(),
        models: vec![model(field)],
    };
    owner
        .save_provider(provider(Some("max_completion_tokens")), Some("test-key"))
        .expect("save provider");

    // 表单读回：字段出现在目录里，前端可原样带回。
    let catalog = owner.redacted_catalog();
    let read_back = catalog.providers[0].models[0]
        .chat_output_tokens_field
        .as_deref();
    assert_eq!(read_back, Some("max_completion_tokens"));

    // 表单不加改动地再次保存（不提供该字段的控件，值来自读回的目录）。
    owner
        .save_provider(provider(read_back), Some("test-key"))
        .expect("resave provider");
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path().join(crate::USER_CONFIG_FILE_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(
        config["providers"]["official"]["models"]["reasoner"]["chat_output_tokens_field"],
        "max_completion_tokens"
    );

    // 选择解析把声明带到执行客户端。
    let snapshot = ProviderConfigSnapshot::capture(home.path(), runtime.handle().clone());
    let selection = snapshot
        .provider_for_selector(Some("official/reasoner"))
        .expect("resolved selection");
    assert_eq!(
        selection.model_configuration().max_output_tokens,
        4_096,
        "the declared limit is applied through the same configuration path"
    );
}

/// 保存只改变动的节点：未触碰的供应商与模型保持文件里原本的字段集合。
///
/// 反序列化会把未声明的可选字段补成 `None`；若整份重新序列化，一次删除或保存就会
/// 给所有无关供应商补上 `null`，并抹掉已废弃但仍需读入的键——用户会看到与自己的
/// 操作无关的配置改动。
#[test]
fn saving_leaves_untouched_providers_field_for_field_unchanged() {
    let home = tempfile::tempdir().expect("temporary config home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);

    // 手写一份配置：被保留的供应商故意只写少量字段，未声明的可选字段都不出现。
    let stored = serde_json::json!({
        "version": 1,
        "default_model": "keep/model",
        "providers": {
            "keep": {
                "base_url": "https://keep.invalid/v1",
                "models": {
                    "model": {
                        "api_protocol": "chat",
                        "max_context_tokens": 372000,
                        "max_output_tokens": 131072,
                        "reasoning_variants": {"low": {"enabled": true, "wire_effort": "low"}},
                        "default_variant": "low",
                        "supports_developer_role": false
                    }
                }
            },
            "doomed": {
                "base_url": "https://doomed.invalid/v1",
                "models": {
                    "model": {
                        "api_protocol": "chat",
                        "max_context_tokens": 128000,
                        "max_output_tokens": 8192
                    }
                }
            }
        }
    });
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&stored).expect("encode fixture"),
    )
    .expect("write fixture");
    let before = read_field_keys(&config_path);

    owner
        .remove_provider("doomed")
        .expect("remove the other provider");

    let after = read_field_keys(&config_path);
    assert_eq!(
        after.get("keep"),
        before.get("keep"),
        "removing one provider must not rewrite another provider's model fields"
    );
    assert!(
        !after.contains_key("doomed"),
        "the removed provider is gone"
    );
}

/// 保存新供应商时，既有供应商同样不被顺手重写。
#[test]
fn adding_a_provider_leaves_existing_ones_unchanged() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let mut owner =
        crate::ModelConfigOwner::open(home.path().to_path_buf(), runtime.handle().clone());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "providers": {
                "keep": {
                    "base_url": "https://keep.invalid/v1",
                    "models": {
                        "model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 372000,
                            "max_output_tokens": 131072
                        }
                    }
                }
            }
        }))
        .expect("encode fixture"),
    )
    .expect("write fixture");
    let before = read_field_keys(&config_path);

    owner
        .save_provider(
            ProviderConfigurationInput {
                provider_id: "added".to_string(),
                display_name: None,
                base_url: "https://added.invalid/v1".to_string(),
                models: vec![ModelConfigurationInput {
                    model_id: "model".to_string(),
                    display_name: None,
                    api_protocol: Some("chat".into()),
                    max_context_tokens: Some(128_000),
                    max_output_tokens: Some(8_192),
                    reasoning_variants: Vec::new(),
                    default_variant: None,
                    thinking_wire_format: None,
                    chat_output_tokens_field: None,
                }],
            },
            None,
        )
        .expect("save the new provider");

    let after = read_field_keys(&config_path);
    assert!(after.contains_key("added"), "the new provider is written");
    assert_eq!(
        after.get("keep"),
        before.get("keep"),
        "adding a provider must not rewrite an existing one"
    );
}

/// 每个供应商的每个模型在文件里的字段名集合。
fn read_field_keys(
    config_path: &std::path::Path,
) -> std::collections::BTreeMap<String, Vec<String>> {
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config_path).expect("read config")).expect("parse");
    config["providers"]
        .as_object()
        .expect("providers object")
        .iter()
        .map(|(provider, value)| {
            let mut keys: Vec<String> = value["models"]
                .as_object()
                .expect("models object")
                .values()
                .flat_map(|model| {
                    model
                        .as_object()
                        .expect("model object")
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .collect();
            keys.sort();
            (provider.clone(), keys)
        })
        .collect()
}
