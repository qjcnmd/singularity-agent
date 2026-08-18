use super::*;
use crate::Provider;

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
            auth_generation: None,
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
                api_key: "sk-primary".to_string(),
            },
        )]),
    };
    let data = user_config_with_two_providers(auth);
    let (snapshot, redacted) = capture_user_model_selection(
        &data,
        Some(ProviderConfigSource::UserConfigFile),
        &OpenAiProvider::new,
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
    let opencode = configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model)
        .expect("builtin fallback resolves missing opencode-go limits");
    assert_eq!(opencode.max_context_tokens, Some(1_000_000));
    assert_eq!(opencode.max_output_tokens, 384_000);

    let longcat = configured_model_from_user_file("longcat", "LongCat-2.0", &model)
        .expect("builtin fallback resolves missing longcat limits");
    assert_eq!(longcat.max_context_tokens, Some(1_000_000));
    assert_eq!(longcat.max_output_tokens, 131_072);
}

#[test]
fn user_declared_limits_win_over_builtin_table() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        max_context_tokens: Some(64_000),
        max_output_tokens: Some(8_192),
        ..UserConfigModel::default()
    };
    let resolved = configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model)
        .expect("user declaration resolves");
    assert_eq!(resolved.max_context_tokens, Some(64_000));
    assert_eq!(resolved.max_output_tokens, 8_192);
}

#[test]
fn capabilities_limits_take_priority_over_builtin_table() {
    let model = UserConfigModel {
        api_protocol: Some("responses".to_string()),
        capabilities: Some(ProviderCapabilityDeclaration {
            max_context_tokens: Some(32_000),
            max_output_tokens: Some(2_048),
            ..ProviderCapabilityDeclaration::default()
        }),
        ..UserConfigModel::default()
    };
    let resolved = configured_model_from_user_file("opencode-go", "deepseek-v4-flash", &model)
        .expect("capabilities declaration resolves");
    assert_eq!(resolved.max_context_tokens, Some(32_000));
    assert_eq!(resolved.max_output_tokens, 2_048);
}

#[test]
fn models_file_capabilities_match_user_capability_projection() {
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
                        "capabilities": {
                            "supports_tools": false,
                            "supports_parallel_tool_calls": true,
                            "supports_required_tool_choice": true,
                            "supports_strict_tool_schema": true,
                            "supports_json_mode": true,
                            "supports_system_message": false,
                            "supports_developer_message": false,
                            "max_parallel_tool_calls": 3,
                            "max_context_tokens": 32000,
                            "max_output_tokens": 2048
                        }
                    }
                }
            }
        }
    }))
    .expect("models file capability declaration");
    let (snapshot, _) = capture_models_file(
        file,
        &mut |name| (name == "PRIMARY_KEY").then(|| "sk-primary".to_string()),
        Some(ProviderConfigSource::UserConfigFile),
        &OpenAiProvider::new,
    )
    .expect("models file capture");
    let provider = provider_for_selection(&snapshot, None).expect("selected provider");
    let contract = provider.protocol_contract();
    assert!(!contract.supports_tools);
    assert!(contract.supports_parallel_tool_calls);
    assert!(contract.supports_required_tool_choice);
    assert!(contract.supports_strict_tool_schema);
    assert!(contract.supports_json_mode);
    assert!(!contract.supports_system_message);
    assert!(!contract.supports_developer_message);
    assert_eq!(contract.max_parallel_tool_calls, 3);
    assert_eq!(contract.max_context_tokens, Some(32000));
    assert_eq!(contract.max_output_tokens, 2048);

    let declared = ProviderCapabilityDeclaration {
        supports_tools: Some(false),
        supports_parallel_tool_calls: Some(true),
        supports_required_tool_choice: Some(true),
        supports_strict_tool_schema: Some(true),
        supports_json_mode: Some(true),
        supports_system_message: Some(false),
        supports_developer_message: Some(false),
        max_parallel_tool_calls: Some(3),
        max_context_tokens: Some(32000),
        max_output_tokens: Some(2048),
        ..ProviderCapabilityDeclaration::default()
    };
    let user_data = UserConfigData {
        directory: PathBuf::from("C:/singularity-test"),
        config: UserConfigFile {
            version: 1,
            default_provider: Some("primary".to_string()),
            default_model: Some("primary/gpt-test".to_string()),
            auth_generation: None,
            providers: BTreeMap::from([(
                "primary".to_string(),
                UserConfigProvider {
                    base_url: "https://example.invalid/v1".to_string(),
                    models: BTreeMap::from([(
                        "gpt-test".to_string(),
                        UserConfigModel {
                            api_protocol: Some("chat".to_string()),
                            capabilities: Some(declared),
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
                    api_key: "sk-primary".to_string(),
                },
            )]),
        },
    };
    let (user_snapshot, _) = capture_user_model_selection(
        &user_data,
        Some(ProviderConfigSource::UserConfigFile),
        &OpenAiProvider::new,
    )
    .expect("user config capture");
    let user_provider =
        provider_for_selection(&user_snapshot, None).expect("user selected provider");
    assert_eq!(contract, user_provider.protocol_contract());
}

#[test]
fn models_file_rejects_unknown_capability_fields() {
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
                        "capabilities": {"unknown_capability": true}
                    }
                }
            }
        }
    }))
    .expect_err("unknown capability must be rejected");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn unknown_model_without_limits_still_fails_closed() {
    let model = UserConfigModel {
        api_protocol: Some("chat".to_string()),
        ..UserConfigModel::default()
    };
    let error = match configured_model_from_user_file("unknown-provider", "unknown-model", &model) {
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
fn provider_config_snapshot_preserves_the_original_configuration_error() {
    let snapshot = ProviderConfigSnapshot::capture_with_provider_and_sources(
        |_| None,
        OpenAiProvider::new,
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
fn selected_provider_without_auth_fails_closed_as_auth_error() {
    let data = user_config_with_two_providers(UserAuthFile::default());
    let result = capture_user_model_selection(
        &data,
        Some(ProviderConfigSource::UserConfigFile),
        &OpenAiProvider::new,
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
        &OpenAiProvider::new,
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

    let auth_path = write_new_auth_generation(
        directory.path(),
        "auth.v1-00000000000000000000000000000000.json",
        &oversized_contents,
    )
    .expect("write oversized private auth");
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

#[cfg(unix)]
#[test]
fn auth_permissions_fail_closed_when_group_readable() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary user config directory");
    let path = directory.path().join("auth.json");
    write_json_file(&path, r#"{"providers":{}}"#, true).expect("write auth file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("make auth file group-readable");
    assert!(ensure_private_secret_file(&path).is_err());
}
