#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

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

fn snapshot(default: &str, credential: bool) -> ProviderConfigSnapshot {
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
    ProviderConfigSnapshot::capture(home.path())
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
    let snapshot = snapshot("openai/gpt-x", false);
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
    let snapshot = snapshot("openai/gpt-x", true);
    let select = |selector| snapshot.resolve(Some(selector)).unwrap().1;

    let plain = select("openai/plain");
    assert_eq!(plain.model_name, "plain");
    assert_eq!(plain.reasoning_variant, None);
    assert_eq!(plain.api_protocol, ProviderApiProtocol::Responses);

    let varianted = select("openai/gpt-x#high");
    assert_eq!(varianted.reasoning_variant.as_deref(), Some("high"));
    assert_eq!(varianted.api_protocol, ProviderApiProtocol::Responses);
    assert_eq!(varianted.max_output_tokens, 4096);
    let disabled = select("openai/gpt-x#off");
    assert_eq!(disabled.reasoning_variant.as_deref(), Some("off"));
}

/// 未声明容量时使用保守下界：不按模型 id 猜容量，也不伪装成提供方默认。
#[test]
fn undeclared_capacity_uses_conservative_defaults() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "default_model": "openai/undeclared",
            "providers": {"openai": {"base_url": "https://example.invalid/v1", "models": {
                "undeclared": {"api_protocol": "responses"}
            }}}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::to_vec(&serde_json::json!({"providers": {"openai": {"api_key": "test-key"}}}))
            .unwrap(),
    )
    .unwrap();
    let selected = ProviderConfigSnapshot::capture(home.path())
        .resolve(Some("openai/undeclared"))
        .unwrap()
        .1;
    assert_eq!(
        selected.max_context_tokens,
        crate::DEFAULT_MAX_CONTEXT_TOKENS
    );
    assert_eq!(selected.max_output_tokens, crate::DEFAULT_MAX_OUTPUT_TOKENS);
}

/// 未知或禁用的变体被拒绝，绝不回退到默认变体。
#[test]
fn selection_rejects_unknown_reasoning_variant() {
    let snapshot = snapshot("openai/gpt-x", true);
    let error = snapshot
        .validate_selector(Some("openai/gpt-x#turbo"))
        .unwrap_err();
    assert_eq!(
        error.code.as_deref(),
        Some("provider_selector_unknown_reasoning_variant")
    );
}

/// 默认 selector 无法解析时，显式 selector 仍然可用：目录与凭据照常读取。
#[test]
fn explicit_selection_works_when_the_default_selector_is_incomplete() {
    let home = tempfile::tempdir().unwrap();
    let mut data = config("unfinished/model", true);
    let provider = data.config.providers["openai"].clone();
    data.config.providers.insert("unfinished".into(), provider);
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::to_vec(&data.auth).unwrap(),
    )
    .unwrap();
    let manager = ModelConfigManager::open(home.path().to_path_buf());
    for default in ["unfinished/model", "unfinished/gpt-x", "malformed"] {
        data.config.default_model = Some(default.into());
        std::fs::write(
            home.path().join("config.json"),
            serde_json::to_vec(&data.config).unwrap(),
        )
        .unwrap();
        let snapshot = manager.snapshot();
        assert!(snapshot.validate_selector(None).is_err());
        snapshot
            .validate_selector(Some("openai/gpt-x#high"))
            .unwrap();
        let catalog = manager.redacted_catalog();
        assert!(
            catalog
                .providers
                .iter()
                .any(|provider| provider.provider_id == "openai" && provider.credential_configured)
        );
    }
}

#[test]
fn model_config_manager_saves_catalog_and_keeps_credentials_write_only() {
    use singularity_protocol::{
        ModelConfigurationInput, ModelConfigurationStatus, ProviderConfigurationInput,
        ReasoningVariant,
    };

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    assert_eq!(
        manager.redacted_catalog().configuration,
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
    manager
        .save_provider(input.clone(), None)
        .expect("save provider");
    let saved = manager.redacted_catalog();
    assert_eq!(saved.configuration, ModelConfigurationStatus::Missing);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));

    manager
        .set_api_key("openai", "top-secret-token")
        .expect("write credential");
    // 证明凭据已生效的是 catalog 读取路径，而非命令回执。
    let catalog = manager.redacted_catalog();
    assert_eq!(catalog.configuration, ModelConfigurationStatus::Ready);
    let frozen = manager.snapshot();
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
        "the credential is persisted only in the private auth manager"
    );
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    for protocol in [None, Some("unsupported")] {
        let mut raw = config.clone();
        let raw_model = &mut raw["providers"]["openai"]["models"]["gpt-x"];
        raw_model["api_protocol"] = serde_json::json!(protocol);
        raw_model["supports_developer_role"] = serde_json::json!(false);
        raw_model["supports_tool_choice"] = serde_json::json!(false);
        raw_model["requires_reasoning_content_for_tool_calls"] = serde_json::json!(true);
        raw_model["requires_assistant_content_for_tool_calls"] = serde_json::json!(true);
        std::fs::write(&config_path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let catalog = manager.redacted_catalog();
        assert_eq!(catalog.configuration, ModelConfigurationStatus::Invalid);
        assert_eq!(
            catalog.providers[0].models[0].api_protocol.as_deref(),
            protocol
        );
        let mut edit = input.clone();
        edit.models = catalog.providers[0].models.clone();
        let before = std::fs::read(&config_path).unwrap();
        assert!(manager.save_provider(edit.clone(), None).is_err());
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        edit.models[0].api_protocol = Some("chat".into());
        manager
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
            manager.redacted_catalog().providers[0].models[0]
                .api_protocol
                .as_deref(),
            Some("chat")
        );
    }
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut alternate = input.clone();
    alternate.provider_id = "alternate".into();
    manager
        .save_provider(alternate, None)
        .expect("save another provider");
    assert_eq!(
        manager.redacted_catalog().default_selector.as_deref(),
        Some("openai/gpt-x"),
        "saving another provider preserves the valid default"
    );
    let before = std::fs::read(&config_path).expect("saved config");
    let mut invalid = input.clone();
    invalid.models[0].max_context_tokens = Some(1024);
    let error = manager
        .save_provider(invalid, None)
        .expect_err("output must fit the context window");
    assert!(error.message.contains("max_output_tokens must be smaller"));
    assert_eq!(std::fs::read(&config_path).unwrap(), before);
    assert_eq!(
        manager.redacted_catalog().configuration,
        ModelConfigurationStatus::Ready
    );

    let mut config: serde_json::Value = serde_json::from_slice(&before).unwrap();
    config["default_model"] = serde_json::json!("openai/gpt-x#missing");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(
        manager.redacted_catalog().configuration,
        ModelConfigurationStatus::Invalid
    );
    assert!(manager.snapshot().resolve(None).is_err());
    assert_eq!(
        frozen.resolve(None).unwrap().1.reasoning_variant.as_deref(),
        Some("high"),
        "external edits only affect later snapshots, including before client construction"
    );

    config["default_model"] = serde_json::json!("openai/gpt-x#high");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let mut edited = input;
    edited.models[0].reasoning_variants.clear();
    edited.models[0].default_variant = None;
    manager
        .save_provider(edited, None)
        .expect("remove selected variant");
    let saved = manager.redacted_catalog();
    assert_eq!(saved.configuration, ModelConfigurationStatus::Ready);
    assert_eq!(saved.default_selector.as_deref(), Some("openai/gpt-x"));
    assert!(manager.snapshot().resolve(None).is_ok());
}

/// 保存只规范输入形状（去空白与结尾斜杠）：写明的端点原样保留，由
/// `openai::wire` 一处解释；目录发现用同一个解释取根。
#[test]
fn saved_base_url_keeps_the_endpoint_the_user_gave() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
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
    manager.save_provider(input, None).expect("save provider");
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path().join(crate::USER_CONFIG_FILE_NAME)).unwrap(),
    )
    .unwrap();
    assert_eq!(config["providers"]["custom"]["base_url"], stored);
    assert_eq!(manager.redacted_catalog().providers[0].base_url, stored);
    // 发现查询的凭据解析留在配置侧：显式输入优先，缺省回退已存储的 key。
    assert_eq!(
        manager
            .discovery_credential("custom", Some("key"))
            .expect("explicit credential"),
        "key"
    );
    assert_eq!(
        manager
            .discovery_credential("custom", None)
            .expect("stored credential fallback"),
        ""
    );
}

#[test]
fn model_config_manager_reports_invalid_persisted_configuration() {
    let home = tempfile::tempdir().expect("temporary config home");
    std::fs::write(home.path().join(crate::USER_CONFIG_FILE_NAME), "{invalid")
        .expect("invalid config fixture");
    let manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    let catalog = manager.redacted_catalog();
    assert_eq!(
        catalog.configuration,
        singularity_protocol::ModelConfigurationStatus::Invalid
    );
    // 解析失败必须作为可见诊断上报；具体文案由 serde 决定，不作为契约。
    assert!(catalog.message.is_some_and(|message| !message.is_empty()));
}

#[test]
fn credentials_and_config_are_read_and_written_per_file() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
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

    manager
        .save_provider(provider("one"), Some("key-one"))
        .expect("save first provider");
    manager
        .save_provider(provider("two"), Some("key-two"))
        .expect("save second provider");
    assert_eq!(stored_key("one").as_deref(), Some("key-one"));
    assert_eq!(stored_key("two").as_deref(), Some("key-two"));

    let config_before = std::fs::read(&config_path).unwrap();
    manager
        .set_api_key("one", "rotated")
        .expect("rotate one key");
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
    manager
        .save_provider(provider("two"), None)
        .expect("edit provider config without a new key");
    assert_eq!(
        std::fs::read(&auth_path).unwrap(),
        auth_before,
        "a config edit without a new key never writes auth.json"
    );

    // auth.json 可以先于 config.json 独立存在：密钥读取只依据 auth 本身。
    std::fs::remove_file(&config_path).unwrap();
    manager
        .set_api_key("one", "rotated-again")
        .expect("update key while config.json is absent");
    assert_eq!(stored_key("two").as_deref(), Some("key-two"));
    std::fs::write(&config_path, "{ not json").unwrap();
    manager
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
    let data = config("openai/plain", true);
    let snapshot = snapshot("openai/plain", true);
    assert_eq!(
        snapshot
            .resolve(Some("openai/plain"))
            .unwrap()
            .1
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
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
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
    manager
        .save_provider(provider(Some("max_completion_tokens")), Some("test-key"))
        .expect("save provider");

    // 表单读回：字段出现在目录里，前端可原样带回。
    let catalog = manager.redacted_catalog();
    let read_back = catalog.providers[0].models[0]
        .chat_output_tokens_field
        .as_deref();
    assert_eq!(read_back, Some("max_completion_tokens"));

    // 表单不加改动地再次保存（不提供该字段的控件，值来自读回的目录）。
    manager
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

    // 选择解析把声明带到已解析模型。
    let snapshot = ProviderConfigSnapshot::capture(home.path());
    let selection = snapshot
        .resolve(Some("official/reasoner"))
        .expect("resolved selection");
    assert_eq!(
        selection.1.max_output_tokens, 4_096,
        "the declared limit is applied through the same configuration path"
    );
}

/// 保存只改变动的节点：未触碰的供应商与模型保持文件里原本的字段集合。
///
/// 反序列化会把未声明的可选字段补成 `None`；若整份重新序列化，一次删除或保存就会
/// 给所有无关供应商补上 `null`，并抹掉已废弃但仍需读入的键——用户会看到与自己的
/// 操作无关的配置改动。
#[test]
fn saving_touches_only_the_provider_being_changed() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
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

    manager
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
    let after_add = read_field_keys(&config_path);
    assert!(
        after_add.contains_key("added"),
        "the new provider is written"
    );
    assert_eq!(
        after_add.get("keep"),
        before.get("keep"),
        "adding a provider must not rewrite an existing one"
    );

    manager
        .remove_provider("doomed")
        .expect("remove the other provider");
    let after_remove = read_field_keys(&config_path);
    assert_eq!(
        after_remove.get("keep"),
        before.get("keep"),
        "removing one provider must not rewrite another provider's model fields"
    );
    assert!(
        !after_remove.contains_key("doomed"),
        "the removed provider is gone"
    );
}

/// 缺省字段由持久化类型自身省略：保存一次新提供方不写出任何 `null`。
#[test]
fn saving_omits_default_fields_without_writing_null() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    manager
        .save_provider(
            ProviderConfigurationInput {
                provider_id: "quiet".to_string(),
                display_name: None,
                base_url: "https://quiet.invalid/v1".to_string(),
                models: vec![ModelConfigurationInput {
                    model_id: "model".to_string(),
                    display_name: None,
                    api_protocol: Some("chat".into()),
                    max_context_tokens: Some(128_000),
                    max_output_tokens: Some(4_096),
                    reasoning_variants: Vec::new(),
                    default_variant: None,
                    thinking_wire_format: None,
                    chat_output_tokens_field: None,
                }],
            },
            None,
        )
        .expect("save provider");

    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_no_null(&saved);
    let model = &saved["providers"]["quiet"]["models"]["model"];
    for absent in [
        "display_name",
        "reasoning_variants",
        "default_variant",
        "supports_developer_role",
        "supports_tool_choice",
        "requires_reasoning_content_for_tool_calls",
        "requires_assistant_content_for_tool_calls",
        "chat_output_tokens_field",
        "thinking_wire_format",
    ] {
        assert!(
            model.get(absent).is_none(),
            "{absent} is omitted when default"
        );
    }
    assert_eq!(model["api_protocol"], "chat");
    assert_eq!(saved["default_model"], "quiet/model");
}

/// 省略规则归属于具体字段：显式 `false` 与已有非空值原样保留，嵌套可选字段
/// 缺省时同样不写 `null`。
#[test]
fn explicit_values_survive_a_save_without_null_keys() {
    use singularity_protocol::ProviderConfigurationInput;

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "default_model": "one/alpha",
            "providers": {
                "one": {
                    "base_url": "https://one.invalid/v1",
                    "models": {
                        "alpha": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128000,
                            "max_output_tokens": 4096,
                            "reasoning_variants": {
                                "low": {"enabled": true, "wire_effort": "low"},
                                "off": {"enabled": false}
                            },
                            "default_variant": "low",
                            "supports_developer_role": false,
                            "supports_tool_choice": false,
                            "requires_reasoning_content_for_tool_calls": true,
                            "chat_output_tokens_field": "max_completion_tokens"
                        }
                    }
                }
            }
        }))
        .expect("encode fixture"),
    )
    .expect("write fixture");

    // 表单读回后原样再保存一次：表单不提供的开关由既有取值带回。
    let catalog = manager.redacted_catalog();
    manager
        .save_provider(
            ProviderConfigurationInput {
                provider_id: "one".to_string(),
                display_name: None,
                base_url: "https://one.invalid/v1".to_string(),
                models: catalog.providers[0].models.clone(),
            },
            None,
        )
        .expect("resave the provider");

    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_no_null(&saved);
    let model = &saved["providers"]["one"]["models"]["alpha"];
    assert_eq!(model["supports_developer_role"], false);
    assert_eq!(model["supports_tool_choice"], false);
    assert_eq!(model["requires_reasoning_content_for_tool_calls"], true);
    assert_eq!(model["chat_output_tokens_field"], "max_completion_tokens");
    assert_eq!(model["default_variant"], "low");
    assert_eq!(
        model["reasoning_variants"]["low"],
        serde_json::json!({"enabled": true, "wire_effort": "low"})
    );
    assert_eq!(
        model["reasoning_variants"]["off"],
        serde_json::json!({"enabled": false}),
        "a variant without a wire effort omits the key instead of writing null"
    );
}

/// 保存按当前类型内容重写整份配置：删除的模型与推理变体随保存消失，空集合不落键。
#[test]
fn removing_a_model_and_its_variants_leaves_no_null_behind() {
    use singularity_protocol::{
        ModelConfigurationInput, ProviderConfigurationInput, ReasoningVariant,
    };

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let model =
        |variants: Vec<ReasoningVariant>, default_variant: Option<&str>| ModelConfigurationInput {
            model_id: "alpha".to_string(),
            display_name: None,
            api_protocol: Some("chat".into()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(4_096),
            reasoning_variants: variants,
            default_variant: default_variant.map(str::to_string),
            thinking_wire_format: None,
            chat_output_tokens_field: None,
        };
    let provider = |models: Vec<ModelConfigurationInput>| ProviderConfigurationInput {
        provider_id: "one".to_string(),
        display_name: None,
        base_url: "https://one.invalid/v1".to_string(),
        models,
    };
    manager
        .save_provider(
            provider(vec![model(
                vec![
                    ReasoningVariant {
                        id: "low".to_string(),
                        enabled: true,
                        wire_effort: Some("low".to_string()),
                    },
                    ReasoningVariant {
                        id: "off".to_string(),
                        enabled: false,
                        wire_effort: None,
                    },
                ],
                Some("low"),
            )]),
            None,
        )
        .expect("save with variants");

    // 清空变体：空集合与缺省 default_variant 都应从文件里消失。
    manager
        .save_provider(provider(vec![model(Vec::new(), None)]), None)
        .expect("remove the variants");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_no_null(&saved);
    let alpha = &saved["providers"]["one"]["models"]["alpha"];
    assert!(alpha.get("reasoning_variants").is_none());
    assert!(alpha.get("default_variant").is_none());

    // 清空模型：被删除的模型不再出现，失效的默认选择也不写成 null。
    manager
        .save_provider(provider(Vec::new()), None)
        .expect("remove the models");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_no_null(&saved);
    assert_eq!(saved["providers"]["one"]["models"], serde_json::json!({}));
    assert!(saved.get("default_model").is_none());
}

/// 密钥是本次请求的纯输入：非法密钥在任何文件读写之前被拒绝，配置、密钥与
/// 内存目录都不受影响；省略与空字符串继续表示本次不改密钥。
#[test]
fn an_invalid_api_key_is_rejected_before_any_file_change() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    let provider = || ProviderConfigurationInput {
        provider_id: "one".to_string(),
        display_name: None,
        base_url: "https://one.invalid/v1".to_string(),
        models: vec![ModelConfigurationInput {
            model_id: "model".to_string(),
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
    manager
        .save_provider(provider(), Some("stored-key"))
        .expect("save provider");

    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let auth_path = home.path().join(crate::USER_AUTH_FILE_NAME);
    let config_before = std::fs::read(&config_path).unwrap();
    let auth_before = std::fs::read(&auth_path).unwrap();
    let catalog_before = manager.redacted_catalog();

    for illegal in ["bad\nkey", "bad\rkey", "bad\0key", " padded-key "] {
        let error = manager
            .save_provider(provider(), Some(illegal))
            .expect_err("an illegal key is rejected");
        assert_eq!(
            error.code.as_deref(),
            Some("provider_configuration_invalid"),
            "{illegal:?}: {error}"
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            config_before,
            "{illegal:?} must not write config.json"
        );
        assert_eq!(
            std::fs::read(&auth_path).unwrap(),
            auth_before,
            "{illegal:?} must not write auth.json"
        );
    }
    assert_eq!(
        manager.redacted_catalog(),
        catalog_before,
        "a rejected input leaves the published catalog unchanged"
    );

    // 省略与空字符串不是「非法」，而是本次不改密钥。
    for omitted in [None, Some("")] {
        manager
            .save_provider(provider(), omitted)
            .expect("an absent or empty key keeps the stored credential");
        assert_eq!(std::fs::read(&auth_path).unwrap(), auth_before);
    }

    // 目录尚不存在时，被拒绝的输入连数据目录都不创建。
    let untouched = home.path().join("not-created-yet");
    let mut fresh = crate::ModelConfigManager::open(untouched.clone());
    let error = fresh
        .save_provider(provider(), Some("bad\nkey"))
        .expect_err("an illegal key is rejected before the first write");
    assert_eq!(
        error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
    assert!(
        !untouched.exists(),
        "a rejected input never creates the data directory"
    );
}

/// 校验前移不吞掉真实的部分成功：auth.json 写入失败时配置已提交，错误仍带可
/// 重试的凭据码，重试后两份文件一致。
#[cfg(windows)]
#[test]
fn a_real_credential_write_failure_still_reports_the_committed_configuration() {
    use singularity_protocol::{ModelConfigurationInput, ProviderConfigurationInput};
    use std::os::windows::fs::OpenOptionsExt;

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let auth_path = home.path().join(crate::USER_AUTH_FILE_NAME);
    manager
        .set_api_key("other", "kept-key")
        .expect("seed the auth file");
    let provider = ProviderConfigurationInput {
        provider_id: "locked".to_string(),
        display_name: None,
        base_url: "https://locked.invalid/v1".to_string(),
        models: vec![ModelConfigurationInput {
            model_id: "model".to_string(),
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
    // 独占写入资格：原子替换失败，auth.json 的内容保持不变。
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x00000001 | 0x00000002)
        .open(&auth_path)
        .unwrap();
    let error = manager
        .save_provider(provider.clone(), Some("new-key"))
        .expect_err("a locked auth file fails the credential write");
    assert_eq!(
        error.code.as_deref(),
        Some(crate::CREDENTIAL_SAVE_FAILED_CODE)
    );
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(
        config["providers"]["locked"]["base_url"], "https://locked.invalid/v1",
        "the configuration half of the save is already committed"
    );
    let auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
    assert!(auth["providers"].get("locked").is_none());
    assert_eq!(auth["providers"]["other"]["api_key"], "kept-key");

    drop(guard);
    manager
        .save_provider(provider, Some("new-key"))
        .expect("retrying the same save completes the credential write");
    let auth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&auth_path).unwrap()).unwrap();
    assert_eq!(auth["providers"]["locked"]["api_key"], "new-key");
    assert_eq!(auth["providers"]["other"]["api_key"], "kept-key");
}

/// 模型集合的转换在提交配置之前完成全部校验：重复模型 id 与重复变体 id 都被
/// 拒绝，且已有配置不被部分改写。
#[test]
fn duplicate_models_and_variants_are_rejected_before_the_config_is_written() {
    use singularity_protocol::{
        ModelConfigurationInput, ProviderConfigurationInput, ReasoningVariant,
    };

    let home = tempfile::tempdir().expect("temporary config home");
    let mut manager = crate::ModelConfigManager::open(home.path().to_path_buf());
    let config_path = home.path().join(crate::USER_CONFIG_FILE_NAME);
    let model = |model_id: &str, variants: Vec<ReasoningVariant>| ModelConfigurationInput {
        model_id: model_id.to_string(),
        display_name: None,
        api_protocol: Some("chat".into()),
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(4_096),
        reasoning_variants: variants,
        default_variant: None,
        thinking_wire_format: None,
        chat_output_tokens_field: None,
    };
    let variant = |id: &str| ReasoningVariant {
        id: id.to_string(),
        enabled: true,
        wire_effort: None,
    };
    let provider = |models| ProviderConfigurationInput {
        provider_id: "one".to_string(),
        display_name: None,
        base_url: "https://one.invalid/v1".to_string(),
        models,
    };
    manager
        .save_provider(provider(vec![model("alpha", Vec::new())]), None)
        .expect("seed the provider");
    let before = std::fs::read(&config_path).expect("saved config");

    let duplicate_model = manager
        .save_provider(
            provider(vec![model("alpha", Vec::new()), model("alpha", Vec::new())]),
            None,
        )
        .expect_err("duplicate model ids are rejected");
    assert!(
        duplicate_model.message.contains("unique"),
        "{duplicate_model}"
    );
    let duplicate_variant = manager
        .save_provider(
            provider(vec![model("beta", vec![variant("low"), variant("low")])]),
            None,
        )
        .expect_err("duplicate variant ids are rejected");
    assert!(
        duplicate_variant.message.contains("unique"),
        "{duplicate_variant}"
    );
    assert_eq!(
        std::fs::read(&config_path).unwrap(),
        before,
        "a rejected model set never rewrites the configuration"
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

/// 递归确认配置树里没有任何 `null` 取值：缺省字段必须被省略，而不是写成 null。
fn assert_no_null(value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => panic!("a default field was written as null"),
        serde_json::Value::Object(map) => map.values().for_each(assert_no_null),
        serde_json::Value::Array(items) => items.iter().for_each(assert_no_null),
        _ => {}
    }
}
