use super::*;
use crate::Provider;
use crate::{USER_AUTH_SCHEMA_VERSION, USER_CONFIG_FILE_NAME};
use std::path::Path;

/// 测试共享的注入 runtime：provider 异步执行一律由上层提供。
fn test_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test provider runtime")
        })
        .handle()
        .clone()
}

/// 注入共享测试 runtime 的 provider 工厂，替代直接传递构造函数指针。
fn test_provider_factory()
-> impl Fn(OpenAiProviderConfig) -> Result<OpenAiProvider, crate::ProviderError> {
    |config| OpenAiProvider::new(config, test_runtime_handle())
}

fn process_provider_config<F>(get_env: F) -> Result<OpenAiProviderConfig, ProviderError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut get_env = get_env;
    let mut captured = std::collections::HashMap::<String, Option<String>>::new();
    let values = resolve_provider_values(|name| {
        if let Some(value) = captured.get(name) {
            return value.clone();
        }
        let value = get_env(name);
        captured.insert(name.to_string(), value.clone());
        value
    });
    OpenAiProviderConfig::from_resolved_values(values)
}

#[test]
fn process_provider_config_uses_the_default_registered_adapter() {
    let config = process_provider_config(|name| match name {
        ENV_MODEL => Some("test-model".to_string()),
        ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
        ENV_API_KEY => Some("test-key-placeholder".to_string()),
        _ => None,
    })
    .expect("provider config");

    assert_eq!(config.provider_name, DEFAULT_PROVIDER_NAME);
    assert_eq!(config.source, ProviderConfigSource::ProcessEnvironment);
}

#[test]
fn process_provider_config_rejects_an_unregistered_adapter() {
    let error = process_provider_config(|name| match name {
        ENV_PROVIDER => Some("unregistered_provider".to_string()),
        ENV_MODEL => Some("test-model".to_string()),
        ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
        ENV_API_KEY => Some("test-key-placeholder".to_string()),
        _ => None,
    })
    .expect_err("unknown provider must fail closed");

    assert_eq!(error.error.kind, ModelErrorKind::UnsupportedCapability);
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_adapter_unsupported")
    );
    assert_eq!(
        error.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
    assert_eq!(
        error.error.provider_name.as_deref(),
        Some("unregistered_provider")
    );
    assert!(!error.message.contains("test-key-placeholder"));
    assert!(!error.message.contains("provider.example"));
}

#[test]
fn process_provider_limits_are_explicit_and_bounded() {
    let default_config = process_provider_config(|name| match name {
        ENV_MODEL => Some("test-model".to_string()),
        ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
        ENV_API_KEY => Some("test-key-placeholder".to_string()),
        _ => None,
    })
    .expect("provider config");
    assert_eq!(
        default_config.protocol_contract().max_context_tokens,
        Some(crate::DEFAULT_MAX_CONTEXT_TOKENS)
    );
    assert_eq!(
        default_config.protocol_contract().max_output_tokens,
        crate::DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert!(default_config.protocol_contract().supports_system_message);
    assert!(
        !default_config
            .protocol_contract()
            .supports_strict_tool_schema
    );

    let configured = process_provider_config(|name| match name {
        ENV_MODEL => Some("test-model".to_string()),
        ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
        ENV_API_KEY => Some("test-key-placeholder".to_string()),
        ENV_CONTEXT_TOKENS => Some("131072".to_string()),
        ENV_MAX_OUTPUT_TOKENS => Some("8192".to_string()),
        _ => None,
    })
    .expect("configured provider");
    let capabilities = configured.protocol_contract();
    assert_eq!(capabilities.max_context_tokens, Some(131_072));
    assert_eq!(capabilities.max_output_tokens, 8_192);
    assert!(!capabilities.supports_strict_tool_schema);

    let provider = test_provider_factory()(configured).expect("provider");
    assert_eq!(Provider::protocol_contract(&provider), capabilities);
}

#[test]
fn process_provider_limit_errors_are_bounded_and_secret_free() {
    for (name, value) in [
        (ENV_CONTEXT_TOKENS, "zero-limit"),
        (ENV_CONTEXT_TOKENS, "2000001"),
        (ENV_MAX_OUTPUT_TOKENS, "256001"),
        (ENV_MAX_OUTPUT_TOKENS, "not-a-token-limit"),
    ] {
        let result = process_provider_config(|candidate| match candidate {
            ENV_MODEL => Some("test-model".to_string()),
            ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
            ENV_API_KEY => Some("test-key-placeholder".to_string()),
            candidate if candidate == name => Some(value.to_string()),
            _ => None,
        });
        let error = result.expect_err("invalid token limit");

        assert_eq!(error.error.kind, ModelErrorKind::InvalidRequest);
        assert!(error.message.contains(name));
        assert!(!error.message.contains(value));
    }
}

#[test]
fn process_provider_config_ignores_removed_tool_capability_envs() {
    let config = process_provider_config(|name| {
        assert!(!matches!(
            name,
            "SINGULARITY_MODEL_MAX_TOOL_CALLS" | "SINGULARITY_MODEL_STRICT_TOOL_SCHEMA"
        ));
        match name {
            ENV_MODEL => Some("test-model".to_string()),
            ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
            ENV_API_KEY => Some("test-key-placeholder".to_string()),
            _ => None,
        }
    })
    .expect("provider configuration");

    assert!(!config.protocol_contract().supports_strict_tool_schema);
}

#[test]
fn process_provider_rejects_output_limit_equal_to_context_window() {
    let error = process_provider_config(|name| match name {
        ENV_MODEL => Some("test-model".to_string()),
        ENV_BASE_URL => Some("https://provider.example/v1".to_string()),
        ENV_API_KEY => Some("test-key-placeholder".to_string()),
        ENV_CONTEXT_TOKENS => Some("4096".to_string()),
        ENV_MAX_OUTPUT_TOKENS => Some("4096".to_string()),
        _ => None,
    })
    .expect_err("inconsistent provider token limits");

    assert_eq!(error.error.kind, ModelErrorKind::InvalidRequest);
    assert!(error.message.contains(ENV_MAX_OUTPUT_TOKENS));
    assert!(error.message.contains(ENV_CONTEXT_TOKENS));
    assert!(!error.message.contains("test-key-placeholder"));
}

fn executable_user_model() -> UserConfigModel {
    UserConfigModel {
        api_protocol: Some("chat".to_string()),
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(4_096),
        ..UserConfigModel::default()
    }
}

fn user_provider() -> UserConfigProvider {
    UserConfigProvider {
        base_url: "https://example.invalid/v1".to_string(),
        models: BTreeMap::from([("test-model".to_string(), executable_user_model())]),
    }
}

fn user_config_with_two_providers(auth: UserAuthFile) -> UserConfigData {
    UserConfigData {
        config: UserConfigFile {
            version: 1,
            default_provider: Some("primary".to_string()),
            default_model: Some("primary/test-model".to_string()),
            providers: BTreeMap::from([
                ("primary".to_string(), user_provider()),
                ("secondary".to_string(), user_provider()),
            ]),
        },
        auth,
    }
}

#[test]
fn unselected_provider_without_auth_does_not_block_capture() {
    let auth = UserAuthFile {
        schema_version: USER_AUTH_SCHEMA_VERSION,
        providers: BTreeMap::from([(
            "primary".to_string(),
            UserAuthProvider {
                api_key: "test-primary-key".to_string(),
            },
        )]),
    };
    let data = user_config_with_two_providers(auth);
    let (snapshot, redacted) = capture_user_model_selection(
        &data,
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    )
    .expect("selected provider is configured");
    assert!(redacted.api_key_present);
    assert!(snapshot.providers["secondary"].provider.is_none());
    let error = provider_for_selection(&snapshot, Some("secondary/test-model"))
        .expect_err("missing auth must fail when selected");
    assert_eq!(error.error.kind, ModelErrorKind::AuthError);
    assert_eq!(error.error.category(), ModelErrorCategory::Authentication);
}

#[test]
fn user_model_without_limits_falls_back_to_conservative_defaults() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        ..UserConfigModel::default()
    };
    let resolved = configured_model_from_user_file(&model, "test-provider", "test-model")
        .expect("missing limits resolve to conservative defaults");
    assert_eq!(
        resolved.max_context_tokens,
        Some(crate::DEFAULT_MAX_CONTEXT_TOKENS)
    );
    assert_eq!(resolved.max_output_tokens, crate::DEFAULT_MAX_OUTPUT_TOKENS);
}

#[test]
fn user_model_without_limits_uses_the_builtin_table_when_available() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        ..UserConfigModel::default()
    };
    let resolved = configured_model_from_user_file(&model, "openai", "gpt-5")
        .expect("known model resolves from the builtin table");
    assert_eq!(resolved.max_context_tokens, Some(400_000));
    assert_eq!(resolved.max_output_tokens, 128_000);
}

#[test]
fn user_config_rejects_capabilities_block() {
    let error = serde_json::from_value::<UserConfigModel>(serde_json::json!({
        "api_protocol": "responses",
        "max_output_tokens": 2048,
        "capabilities": {
            "max_context_tokens": 32000,
            "max_output_tokens": 2048
        }
    }))
    .expect_err("capabilities must be rejected as an unknown field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn user_model_override_without_api_protocol_still_fails_closed() {
    let model = UserConfigModel {
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(4_096),
        ..UserConfigModel::default()
    };
    let error = match configured_model_from_user_file(&model, "test-provider", "test-model") {
        Ok(_) => panic!("api_protocol cannot be guessed"),
        Err(error) => error,
    };
    assert!(error.message.contains("api_protocol"));
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
}

#[test]
fn provider_config_snapshot_preserves_the_original_configuration_error() {
    let snapshot = ProviderConfigSnapshot::capture_with_provider_and_sources(
        |_| None,
        test_provider_factory(),
        || None,
    );

    assert_eq!(snapshot.source(), None);
    assert!(!snapshot.configuration().configured);
    let first = snapshot.provider().expect_err("missing provider config");
    let second = snapshot
        .provider()
        .expect_err("same missing provider config");
    assert_eq!(first, second);
    assert!(first.message.contains("SINGULARITY_MODEL"));
    assert_eq!(
        first.error.code.as_deref(),
        Some("provider_configuration_missing")
    );
    assert_eq!(
        first.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
}

#[test]
fn partial_process_environment_is_authoritative_over_user_config_layer() {
    let mut user_layer_read = false;
    let snapshot = ProviderConfigSnapshot::capture_with_provider_and_sources(
        |name| match name {
            // A single process setting selects the whole process layer. The
            // remaining settings must not be filled from user config.
            ENV_MODEL => Some("process-model".to_string()),
            _ => None,
        },
        test_provider_factory(),
        || {
            user_layer_read = true;
            Some(ProviderConfigLayer {
                model_name: Some("user-model".to_string()),
                base_url: Some("https://user.example/v1".to_string()),
                api_key: Some("user-key".to_string()),
                ..ProviderConfigLayer::default()
            })
        },
    );

    assert_eq!(
        snapshot.source(),
        Some(ProviderConfigSource::ProcessEnvironment)
    );
    assert!(
        !user_layer_read,
        "process layer must short-circuit user config"
    );

    let error = snapshot
        .provider()
        .expect_err("partial process environment must fail instead of merging user config");
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_missing")
    );
    assert!(error.message.contains("SINGULARITY_BASE_URL"));
}

#[test]
fn relative_home_is_rejected_before_path_use() {
    let error = normalize_absolute_path(Path::new("relative-home"))
        .expect_err("relative user home must fail closed");
    assert!(error.message.contains("absolute path"));
}

#[test]
fn metadata_errors_are_not_treated_as_missing_paths() {
    let directory = tempfile::tempdir().expect("metadata directory");
    let missing = directory.path().join("missing.json");
    assert!(!path_exists_or_missing(&missing, "metadata failed").expect("missing is allowed"));
    let invalid = Path::new("\0");
    let error = path_exists_or_missing(invalid, "metadata failed")
        .expect_err("metadata errors must fail closed");
    assert_eq!(error.message, "metadata failed");
}

#[test]
fn endpoint_validation_rejects_ambiguous_provider_urls() {
    assert!(
        validate_base_url(
            Some("https://provider.example/v1"),
            Some(ProviderConfigSource::UserConfigFile),
        )
        .is_ok()
    );
    for invalid in [
        "",
        "provider.example/v1",
        "ftp://provider.example/v1",
        "https://user:secret@provider.example/v1",
        "https://provider.example/v1?token=secret",
        "https://provider.example/v1#fragment",
    ] {
        assert!(
            validate_base_url(Some(invalid), Some(ProviderConfigSource::UserConfigFile)).is_err(),
            "endpoint must be rejected: {invalid}"
        );
    }
}

#[test]
fn split_model_selector_lazily_splits_each_segment() {
    let parts = split_model_selector("provider/model#high");
    assert_eq!(parts.provider, Some("provider"));
    assert_eq!(parts.model, Some("model"));
    assert_eq!(parts.effort, Some("high"));
}

#[test]
fn compose_model_selector_is_split_inverse_for_qualified_selectors() {
    let selector = compose_model_selector("provider", "model", Some("high"));
    assert_eq!(selector, "provider/model#high");
    let parts = split_model_selector(&selector);
    assert_eq!(parts.provider, Some("provider"));
    assert_eq!(parts.model, Some("model"));
    assert_eq!(parts.effort, Some("high"));
    // 空 effort 不附加 #，round-trip 后 effort 为 None。
    let plain = compose_model_selector("provider", "model", None);
    assert_eq!(plain, "provider/model");
    assert_eq!(split_model_selector(&plain).effort, None);
    assert_eq!(
        compose_model_selector("provider", "model", Some("")),
        "provider/model",
        "空 effort 视为未提供"
    );
}

#[test]
fn selected_invalid_endpoint_precedes_missing_auth() {
    let mut data = user_config_with_two_providers(UserAuthFile::default());
    data.config
        .providers
        .get_mut("primary")
        .expect("default provider")
        .base_url = "not-an-absolute-url".to_string();
    let error = match capture_user_model_selection(
        &data,
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    ) {
        Ok(_) => panic!("invalid endpoint must fail before missing auth"),
        Err(error) => error,
    };
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
    assert!(error.message.contains("absolute URL"));
}

#[test]
fn oversized_user_config_and_private_auth_reads_are_rejected() {
    let directory = tempfile::tempdir().expect("user config directory");
    let oversized_contents = "x".repeat(crate::MAX_CONFIG_AUTH_FILE_BYTES + 1);
    let config_path = directory.path().join(USER_CONFIG_FILE_NAME);
    std::fs::write(&config_path, &oversized_contents).expect("write oversized user config");
    let config_error = match read_user_config_data_from_directory(directory.path().to_path_buf()) {
        Ok(_) => panic!("oversized user config must fail"),
        Err(error) => error,
    };
    assert_eq!(
        config_error.message,
        "user provider config exceeds the size limit"
    );
    assert_eq!(
        config_error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
    assert_eq!(
        config_error.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
    assert!(
        !config_error
            .message
            .contains(&config_path.display().to_string())
    );

    let auth_path = directory.path().join(crate::USER_AUTH_FILE_NAME);
    std::fs::write(&auth_path, &oversized_contents).expect("write oversized private auth");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict private auth to owner");
    }
    let auth_error =
        read_private_auth_file(&auth_path).expect_err("oversized private auth must fail");
    assert_eq!(
        auth_error.message,
        "user provider auth exceeds the size limit"
    );
    assert_eq!(
        auth_error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
    assert_eq!(
        auth_error.error.stage,
        Some(ProviderErrorStage::ClientInitialization)
    );
    assert!(
        !auth_error
            .message
            .contains(&auth_path.display().to_string())
    );
    assert!(!auth_error.message.contains(&oversized_contents));
}

/// 凭据目录只认唯一 `auth.json`：读侧合并 config.json + auth.json，
/// 同一路径内容翻新后读取结果随之收敛。
#[test]
fn auth_file_reads_reflect_the_single_credential_file() {
    let directory = tempfile::tempdir().expect("user config directory");
    let config = serde_json::json!({
        "version": 1,
        "default_provider": "primary",
        "default_model": "primary/test-model",
        "providers": {
            "primary": {
                "base_url": "https://example.invalid/v1",
                "models": {
                    "test-model": { "api_protocol": "chat", "max_output_tokens": 2048 }
                }
            }
        }
    });
    std::fs::write(
        directory.path().join(USER_CONFIG_FILE_NAME),
        config.to_string(),
    )
    .expect("write user config");
    let auth_path = directory.path().join(crate::USER_AUTH_FILE_NAME);
    let credentials = |api_key: &str| {
        serde_json::json!({
            "schema_version": USER_AUTH_SCHEMA_VERSION,
            "providers": { "primary": { "api_key": api_key } }
        })
        .to_string()
    };
    let write_auth = |contents: String| {
        std::fs::write(&auth_path, contents).expect("write private auth file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict private auth to owner");
        }
    };
    write_auth(credentials("test-primary-key"));
    let data = read_user_config_data_from_directory(directory.path().to_path_buf())
        .expect("read user config")
        .expect("user config present");
    assert_eq!(data.auth.providers["primary"].api_key, "test-primary-key");

    // 同一路径整体翻新凭据后，读侧看到替换后的完整 JSON。
    write_auth(credentials("sk-rotated"));
    let data = read_user_config_data_from_directory(directory.path().to_path_buf())
        .expect("read replaced user config")
        .expect("user config present");
    assert_eq!(data.auth.providers["primary"].api_key, "sk-rotated");
}

#[cfg(unix)]
#[test]
fn auth_permissions_fail_closed_when_group_readable() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary user config directory");
    let path = directory.path().join("auth.json");
    std::fs::write(&path, r#"{"providers":{}}"#).expect("write auth file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("make auth file group-readable");
    assert!(super::user::ensure_private_secret_file(&path).is_err());
}
