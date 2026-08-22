use super::*;
use crate::{Provider, USER_AUTH_SCHEMA_VERSION, USER_CONFIG_FILE_NAME};

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
        models: BTreeMap::from([("gpt-test".to_string(), executable_user_model())]),
    }
}

fn user_config_with_two_providers(auth: UserAuthFile) -> UserConfigData {
    UserConfigData {
        directory: PathBuf::from("C:/singularity-test"),
        config: UserConfigFile {
            version: 1,
            default_provider: Some("primary".to_string()),
            default_model: Some("primary/gpt-test".to_string()),
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
    let error = provider_for_selection(&snapshot, Some("secondary/gpt-test"))
        .expect_err("missing auth must fail when selected");
    assert_eq!(error.error.kind, ModelErrorKind::AuthError);
    assert_eq!(error.error.category(), ModelErrorCategory::Authentication);
}

#[test]
fn user_model_without_limits_falls_back_to_builtin_table() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        ..UserConfigModel::default()
    };
    let opencode =
        configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model, None)
            .expect("builtin fallback resolves missing opencode-go limits");
    assert_eq!(opencode.max_context_tokens, Some(1_000_000));
    assert_eq!(opencode.max_output_tokens, 384_000);

    let longcat = configured_model_from_user_file("longcat", "LongCat-2.0", &model, None)
        .expect("builtin fallback resolves missing longcat limits");
    assert_eq!(longcat.max_context_tokens, Some(1_000_000));
    assert_eq!(longcat.max_output_tokens, 131_072);
}

/// models.dev 目录投影的限额值，用于第三级来源的判定测试。
fn directory_limits() -> Option<crate::config::user::metadata::ModelTokenLimits> {
    Some(super::user::metadata::ModelTokenLimits {
        context: 500_000,
        output: 65_536,
    })
}

#[test]
fn directory_metadata_fills_limits_only_after_user_and_builtin_sources() {
    let model = UserConfigModel {
        api_protocol: Some("chat".to_string()),
        ..UserConfigModel::default()
    };

    // 用户与内置表都缺时限额由目录元数据填充。
    let resolved = configured_model_from_user_file(
        "unknown-provider",
        "unknown-model",
        &model,
        directory_limits(),
    )
    .expect("directory metadata is the third limit source");
    assert_eq!(resolved.max_context_tokens, Some(500_000));
    assert_eq!(resolved.max_output_tokens, 65_536);

    // 内置表命中即停，不消费目录值。
    let resolved = configured_model_from_user_file(
        "opencode-go",
        "deepseek-v4-flash",
        &model,
        directory_limits(),
    )
    .expect("builtin entry resolves");
    assert_eq!(resolved.max_context_tokens, Some(1_000_000));
    assert_eq!(resolved.max_output_tokens, 384_000);

    // 用户声明仍最高优先。
    let declared = UserConfigModel {
        api_protocol: Some("chat".to_string()),
        max_context_tokens: Some(64_000),
        max_output_tokens: Some(8_192),
        ..UserConfigModel::default()
    };
    let resolved = configured_model_from_user_file(
        "unknown-provider",
        "unknown-model",
        &declared,
        directory_limits(),
    )
    .expect("user declaration resolves");
    assert_eq!(resolved.max_context_tokens, Some(64_000));
    assert_eq!(resolved.max_output_tokens, 8_192);

    // 无任何来源时仍然 fail closed。
    let error =
        match configured_model_from_user_file("unknown-provider", "unknown-model", &model, None) {
            Ok(_) => panic!("no limit source must keep failing closed"),
            Err(error) => error,
        };
    assert!(error.message.contains("incomplete"));
}

#[test]
fn user_declared_limits_win_over_builtin_table() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        max_context_tokens: Some(64_000),
        max_output_tokens: Some(8_192),
        ..UserConfigModel::default()
    };
    let resolved =
        configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model, None)
            .expect("user declaration resolves");
    assert_eq!(resolved.max_context_tokens, Some(64_000));
    assert_eq!(resolved.max_output_tokens, 8_192);
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
fn user_model_top_level_limits_project_with_builtin_fallback() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        max_context_tokens: Some(400_000),
        ..UserConfigModel::default()
    };
    let resolved =
        configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model, None)
            .expect("top-level and builtin fallback resolve");
    assert_eq!(resolved.max_context_tokens, Some(400_000));
    assert_eq!(resolved.max_output_tokens, 384_000);
}

#[test]
fn models_file_projects_only_from_top_level_fields() {
    let file: ModelsFile = serde_json::from_value(serde_json::json!({
        "default_model": "primary/gpt-test",
        "providers": {
            "primary": {
                "adapter": "openai_compatible",
                "base_url": "https://example.invalid/v1",
                "api_key_env": "PRIMARY_KEY",
                "models": {
                    "gpt-test": {
                        "api_protocol": "chat",
                        "max_context_tokens": 32000,
                        "max_output_tokens": 2048
                    }
                }
            }
        }
    }))
    .expect("models file with top-level limits");
    let (snapshot, _) = capture_models_file(
        file,
        &mut |name| (name == "PRIMARY_KEY").then(|| "test-primary-key".to_string()),
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    )
    .expect("models file capture");
    let provider = provider_for_selection(&snapshot, None).expect("selected provider");
    let contract = provider.protocol_contract();
    assert_eq!(contract.max_context_tokens, Some(32000));
    assert_eq!(contract.max_output_tokens, 2048);
    assert!(contract.supports_tools);
    assert!(contract.supports_developer_message);
}

#[test]
fn models_file_rejects_capabilities_block() {
    let error = serde_json::from_value::<ModelsFile>(serde_json::json!({
        "default_model": "primary/gpt-test",
        "providers": {
            "primary": {
                "adapter": "openai_compatible",
                "base_url": "https://example.invalid/v1",
                "api_key_env": "PRIMARY_KEY",
                "models": {
                    "gpt-test": {
                        "api_protocol": "chat",
                        "max_output_tokens": 2048,
                        "capabilities": {
                            "supports_tools": true
                        }
                    }
                }
            }
        }
    }))
    .expect_err("capabilities block must be rejected as an unknown field");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn persisted_capability_block_is_rejected() {
    let directory = tempfile::tempdir().expect("user config directory");
    let config = serde_json::json!({
        "version": 1,
        "default_provider": "primary",
        "default_model": "primary/gpt-test",
        "providers": {
            "primary": {
                "base_url": "https://example.invalid/v1",
                "models": {
                    "gpt-test": {
                        "api_protocol": "chat",
                        "max_output_tokens": 2048,
                        "capabilities": {
                            "supports_tools": true,
                            "supports_required_tool_choice": false,
                            "supports_json_mode": false
                        }
                    }
                }
            }
        }
    });
    write_json_file(
        &directory.path().join(USER_CONFIG_FILE_NAME),
        &config.to_string(),
        false,
    )
    .expect("write persisted user config");

    let error = match read_user_config_data_from_directory(directory.path().to_path_buf()) {
        Ok(_) => panic!("config with a capabilities block must be rejected"),
        Err(error) => error,
    };
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
    assert!(
        error.message.contains("invalid JSON"),
        "capabilities is an unknown field"
    );
}

#[test]
fn unknown_model_without_limits_still_fails_closed() {
    let model = UserConfigModel {
        api_protocol: Some("chat".to_string()),
        ..UserConfigModel::default()
    };
    let error =
        match configured_model_from_user_file("unknown-provider", "unknown-model", &model, None) {
            Ok(_) => panic!("no builtin fallback for an unknown model"),
            Err(error) => error,
        };
    assert!(error.message.contains("incomplete"));
    assert_eq!(
        error.error.code.as_deref(),
        Some("provider_configuration_invalid")
    );
}

#[test]
fn user_model_override_without_api_protocol_still_fails_closed() {
    let model = UserConfigModel {
        max_context_tokens: Some(128_000),
        max_output_tokens: Some(4_096),
        ..UserConfigModel::default()
    };
    let error =
        match configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model, None) {
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
fn capture_fails_closed_for_override_without_any_limit_source() {
    let data = UserConfigData {
        directory: PathBuf::from("C:/singularity-test"),
        config: UserConfigFile {
            version: 1,
            default_provider: Some("primary".to_string()),
            default_model: Some("primary/gpt-test".to_string()),
            providers: BTreeMap::from([(
                "primary".to_string(),
                UserConfigProvider {
                    base_url: "https://example.invalid/v1".to_string(),
                    models: BTreeMap::from([(
                        "gpt-test".to_string(),
                        UserConfigModel {
                            api_protocol: Some("chat".to_string()),
                            ..UserConfigModel::default()
                        },
                    )]),
                },
            )]),
        },
        auth: UserAuthFile {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::from([(
                "primary".to_string(),
                UserAuthProvider {
                    api_key: "test-primary-key".to_string(),
                },
            )]),
        },
    };
    let error = match capture_user_model_selection(
        &data,
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    ) {
        Ok(_) => panic!("a protocol-only override without any limit source must fail closed"),
        Err(error) => error,
    };
    assert!(error.message.contains("incomplete"));
}

#[test]
fn capture_fills_directory_limits_from_cache_and_keeps_fail_closed_without_it() {
    let protocol_only_model = || UserConfigModel {
        api_protocol: Some("chat".to_string()),
        ..UserConfigModel::default()
    };
    let data_in = |directory: &Path| UserConfigData {
        directory: directory.to_path_buf(),
        config: UserConfigFile {
            version: 1,
            default_provider: Some("primary".to_string()),
            default_model: Some("primary/gpt-test".to_string()),
            providers: BTreeMap::from([(
                "primary".to_string(),
                UserConfigProvider {
                    base_url: "https://example.invalid/v1".to_string(),
                    models: BTreeMap::from([("gpt-test".to_string(), protocol_only_model())]),
                },
            )]),
        },
        auth: UserAuthFile {
            schema_version: USER_AUTH_SCHEMA_VERSION,
            providers: BTreeMap::from([(
                "primary".to_string(),
                UserAuthProvider {
                    api_key: "test-primary-key".to_string(),
                },
            )]),
        },
    };
    let write_metadata_cache = |directory: &Path, fetched_at_unix_seconds: u64| {
        std::fs::write(
            directory.join(crate::METADATA_CACHE_FILE_NAME),
            serde_json::json!({
                "schema_version": crate::METADATA_CACHE_SCHEMA_VERSION,
                "fetched_at_unix_seconds": fetched_at_unix_seconds,
                "providers": {
                    "primary": {
                        "hosts": ["example.invalid"],
                        "models": { "gpt-test": { "context": 500_000u32, "output": 32_768u32 } }
                    }
                }
            })
            .to_string(),
        )
        .expect("write metadata cache");
    };

    // 无任何元数据缓存：同配置维持 fail closed。
    let without_cache = tempfile::tempdir().expect("empty user config directory");
    let error = match capture_user_model_selection(
        &data_in(without_cache.path()),
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    ) {
        Ok(_) => panic!("a missing metadata cache must not fill limits"),
        Err(error) => error,
    };
    assert!(error.message.contains("incomplete"));

    // 过期缓存不参与填充。
    let expired = tempfile::tempdir().expect("expired user config directory");
    write_metadata_cache(
        expired.path(),
        crate::config::user::unix_timestamp_seconds() - crate::USER_MODELS_CACHE_TTL_SECONDS - 1,
    );
    assert!(
        capture_user_model_selection(
            &data_in(expired.path()),
            Some(ProviderConfigSource::UserConfigFile),
            &test_provider_factory(),
        )
        .is_err(),
        "an expired metadata cache must keep the configuration fail closed"
    );

    // 注入新鲜缓存后未知模型捕获成功，contract 携带目录填充值。
    let filled = tempfile::tempdir().expect("filled user config directory");
    write_metadata_cache(filled.path(), crate::config::user::unix_timestamp_seconds());
    let (snapshot, _) = capture_user_model_selection(
        &data_in(filled.path()),
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    )
    .expect("fresh metadata cache fills the missing limits");
    let contract = provider_for_selection(&snapshot, None)
        .expect("selected provider")
        .protocol_contract();
    assert_eq!(contract.max_context_tokens, Some(500_000));
    assert_eq!(contract.max_output_tokens, 32_768);
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
fn selected_provider_without_auth_fails_closed_as_auth_error() {
    let data = user_config_with_two_providers(UserAuthFile::default());
    let result = capture_user_model_selection(
        &data,
        Some(ProviderConfigSource::UserConfigFile),
        &test_provider_factory(),
    );
    let error = match result {
        Ok(_) => panic!("default provider auth is required"),
        Err(error) => error,
    };
    assert_eq!(error.error.kind, ModelErrorKind::AuthError);
    assert_eq!(error.error.category(), ModelErrorCategory::Authentication);
}

#[test]
fn cache_read_failures_are_typed_and_non_blocking() {
    let directory = tempfile::tempdir().expect("cache directory");
    let invalid = directory.path().join("invalid.json");
    std::fs::write(&invalid, b"not-json").expect("write invalid cache");
    assert_eq!(
        load_models_cache(&invalid).status,
        ModelCacheStatus::Invalid
    );

    let missing = directory.path().join("missing.json");
    assert_eq!(
        load_models_cache(&missing).status,
        ModelCacheStatus::NotPresent
    );

    let read_failed = directory.path().join("cache-directory");
    std::fs::create_dir(&read_failed).expect("create cache directory");
    assert_eq!(
        load_models_cache(&read_failed).status,
        ModelCacheStatus::ReadFailed
    );
}

#[test]
fn relative_home_is_rejected_before_path_use() {
    let error = normalize_absolute_path(Path::new("relative-home"))
        .expect_err("relative user home must fail closed");
    assert!(error.message.contains("absolute path"));
}

#[test]
fn repository_boundary_uses_nearest_git_root_and_allows_ancestors() {
    let directory = tempfile::tempdir().expect("repository boundary directory");
    let workspace = directory.path().join("workspace");
    let repository = workspace.join("repository");
    let nested = repository.join("nested");
    std::fs::create_dir_all(&nested).expect("create repository tree");
    std::fs::write(repository.join(".git"), b"gitdir: test").expect("create worktree marker");

    let root = repository_boundary_root(&nested).expect("discover nearest repository root");
    assert_eq!(
        root,
        canonicalize_existing_prefix(&repository).expect("canonical repository root")
    );
    ensure_home_outside_root(&workspace, &root).expect("repository ancestors remain usable");
    let inside = repository.join("missing-home");
    let error = ensure_home_outside_root(&inside, &root)
        .expect_err("repository root descendants must be rejected");
    assert!(error.message.contains("current repository"));
}

#[cfg(windows)]
#[test]
fn repository_boundary_comparison_is_case_insensitive_with_missing_tail() {
    let directory = tempfile::tempdir().expect("repository boundary directory");
    let repository = directory.path().join("CaseSensitiveRepo");
    let nested = repository.join("nested");
    std::fs::create_dir_all(&nested).expect("create repository tree");
    std::fs::create_dir(repository.join(".git")).expect("create repository marker");
    let root = repository_boundary_root(&nested).expect("discover repository root");
    let case_variant =
        PathBuf::from(repository.to_string_lossy().to_ascii_lowercase()).join("missing-home");
    assert!(
        ensure_home_outside_root(&case_variant, &root).is_err(),
        "case variants of repository descendants must be rejected"
    );
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
fn import_selector_rejects_invalid_model_and_variant_identifiers() {
    assert!(parse_import_model_selector("default/model name#high", "default").is_err());
    assert!(parse_import_model_selector("default/model#high variant", "default").is_err());
    assert!(parse_import_model_selector("default/model#high/fast", "default").is_err());
}

#[test]
fn import_selector_accepts_configured_provider_prefix_and_bare_slash_model_ids() {
    assert_eq!(
        parse_import_model_selector("default/models/gpt#high", "default")
            .expect("configured provider selector"),
        (
            "default/models/gpt#high".to_string(),
            "models/gpt".to_string()
        )
    );
    assert_eq!(
        parse_import_model_selector("models/gpt", "default")
            .expect("bare slash-containing model id"),
        ("default/models/gpt".to_string(), "models/gpt".to_string())
    );
}

#[test]
fn import_selector_treats_mismatched_provider_prefix_as_a_bare_model_id() {
    assert_eq!(
        parse_import_model_selector("other/models/gpt", "default")
            .expect("slash-containing model id is not a selector for another provider"),
        (
            "default/other/models/gpt".to_string(),
            "other/models/gpt".to_string()
        )
    );
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
fn split_model_selector_allows_partial_selectors() {
    assert_eq!(split_model_selector("provider/model").effort, None);
    assert_eq!(split_model_selector("model").provider, None);
    assert_eq!(split_model_selector("model").model, Some("model"));
    assert_eq!(
        split_model_selector("provider/model/extra#high").model,
        Some("model/extra")
    );
    assert_eq!(split_model_selector("provider/").model, None);
    assert_eq!(split_model_selector("").provider, None);
    assert_eq!(split_model_selector("").model, None);
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
fn oversized_cache_is_invalid_while_io_failures_remain_read_failed() {
    let directory = tempfile::tempdir().expect("cache directory");
    let oversized = directory.path().join("oversized.json");
    std::fs::write(
        &oversized,
        vec![b'x'; crate::MAX_DISCOVERY_RESPONSE_BYTES + 1],
    )
    .expect("write oversized cache");
    assert_eq!(
        load_models_cache(&oversized).status,
        ModelCacheStatus::Invalid
    );

    let read_failed = directory.path().join("cache-directory");
    std::fs::create_dir(&read_failed).expect("create cache directory");
    assert_eq!(
        load_models_cache(&read_failed).status,
        ModelCacheStatus::ReadFailed
    );
}

#[test]
fn oversized_user_config_and_private_auth_reads_are_rejected() {
    let directory = tempfile::tempdir().expect("user config directory");
    let oversized_contents = "x".repeat(crate::MAX_DISCOVERY_RESPONSE_BYTES + 1);
    let config_path = directory.path().join(USER_CONFIG_FILE_NAME);
    write_json_file(&config_path, &oversized_contents, true).expect("write oversized user config");
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
    write_json_file(&auth_path, &oversized_contents, true).expect("write oversized private auth");
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

#[test]
fn config_writer_lock_is_exclusive_and_releases_cleanly() {
    let directory = tempfile::tempdir().expect("writer lock directory");
    let first = acquire_config_writer_lock(directory.path()).expect("first writer lock");
    let second = match acquire_config_writer_lock(directory.path()) {
        Ok(_) => panic!("second writer must observe the exclusive lock"),
        Err(error) => error,
    };
    assert!(second.message.contains("in progress"));
    drop(first);
    assert!(directory.path().join(".config.lock").exists());
    let third = acquire_config_writer_lock(directory.path()).expect("lock is released");
    drop(third);
    assert!(directory.path().join(".config.lock").exists());
}

#[test]
fn config_json_write_is_atomic() {
    let directory = tempfile::tempdir().expect("temporary user config directory");
    let path = directory.path().join(USER_CONFIG_FILE_NAME);
    write_json_file(&path, r#"{"providers":{}}"#, false).expect("write config file");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read config file"),
        r#"{"providers":{}}"#
    );
    assert!(
        !directory
            .path()
            .read_dir()
            .expect("read temporary directory")
            .any(|entry| entry
                .expect("read directory entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".config.json.tmp-"))
    );
}

#[test]
fn auth_file_replaces_atomically_as_the_single_credential_file() {
    let directory = tempfile::tempdir().expect("user config directory");
    let config = serde_json::json!({
        "version": 1,
        "default_provider": "primary",
        "default_model": "primary/gpt-test",
        "providers": {
            "primary": {
                "base_url": "https://example.invalid/v1",
                "models": {
                    "gpt-test": { "api_protocol": "chat", "max_output_tokens": 2048 }
                }
            }
        }
    });
    write_json_file(
        &directory.path().join(USER_CONFIG_FILE_NAME),
        &config.to_string(),
        false,
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
    write_json_file(&auth_path, &credentials("test-primary-key"), true).expect("write private auth file");
    let data = read_user_config_data_from_directory(directory.path().to_path_buf())
        .expect("read user config")
        .expect("user config present");
    assert_eq!(data.auth.providers["primary"].api_key, "test-primary-key");

    // 同一路径重复写入即原子替换：内容整体翻新，读侧永远看到完整 JSON。
    write_json_file(&auth_path, &credentials("sk-rotated"), true)
        .expect("replace private auth file");
    let data = read_user_config_data_from_directory(directory.path().to_path_buf())
        .expect("read replaced user config")
        .expect("user config present");
    assert_eq!(data.auth.providers["primary"].api_key, "sk-rotated");

    // 凭据目录只保留唯一 auth 文件，且原子替换不残留临时文件。
    let names = directory
        .path()
        .read_dir()
        .expect("read credential directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("directory entries")
        .into_iter()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names
            .iter()
            .filter(|name| name.starts_with("auth."))
            .count(),
        1,
        "exactly one auth file must exist: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name.contains(".tmp-")),
        "atomic replacement must not leave temporary files: {names:?}"
    );
}

#[cfg(unix)]
#[test]
fn auth_permissions_fail_closed_when_group_readable() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary user config directory");
    let path = directory.path().join("auth.json");
    write_json_file(&path, r#"{"providers":{}}"#, true).expect("write auth file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("make auth file group-readable");
    assert!(super::user::ensure_private_secret_file(&path).is_err());
}
