use super::*;

#[test]
fn model_discovery_errors_preserve_recovery_category() {
    let network = model_discovery_error(ProviderError::new(
        ModelErrorKind::NetworkError,
        "network unavailable",
    ));
    assert_eq!(network.code, RpcErrorCode::ProviderUnavailable);
    assert!(network.recovery.contains("稍后重试"));
    assert!(network.recovery.contains("手动添加模型"));

    let configuration = model_discovery_error(
        ProviderError::new(ModelErrorKind::InvalidRequest, "invalid provider")
            .with_code("provider_configuration_invalid"),
    );
    assert_eq!(configuration.code, RpcErrorCode::ConfigurationInvalid);
    assert!(configuration.recovery.contains("模型设置"));

    let authentication = model_discovery_error(ProviderError::new(
        ModelErrorKind::AuthError,
        "invalid credential",
    ));
    assert_eq!(authentication.code, RpcErrorCode::ConfigurationInvalid);
    assert!(authentication.recovery.contains("API 地址和密钥"));
}

#[test]
fn creating_a_session_preserves_its_requested_selector() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let workspace = fixture
        .app_server
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("workspace");
    let snapshot = fixture
        .app_server
        .create_session(
            &workspace.workspace_id,
            Some("openai_compatible/chosen-model".to_string()),
        )
        .expect("session");
    assert_eq!(
        snapshot.runtime.selector.as_deref(),
        Some("openai_compatible/chosen-model")
    );
}

#[cfg(windows)]
#[test]
fn provider_save_publishes_once_and_reports_a_retryable_credential_failure() {
    use singularity_protocol::ModelConfigurationInput;
    use std::os::windows::fs::OpenOptionsExt;

    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.app_server;
    let provider = ProviderConfigurationInput {
        provider_id: "combined".into(),
        display_name: None,
        base_url: "https://example.invalid/v1".into(),
        models: vec![ModelConfigurationInput {
            model_id: "model".into(),
            display_name: None,
            api_protocol: Some("chat".into()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(8192),
            reasoning_variants: Vec::new(),
            default_variant: None,
            thinking_wire_format: None,
            chat_output_tokens_field: None,
        }],
    };
    let auth_guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x00000001 | 0x00000002)
        .open(fixture._sessions.home().join("auth.json"))
        .unwrap();
    let mut stream = host.subscribe();
    let error = host
        .save_provider(provider.clone(), Some("synthetic-key"))
        .unwrap_err();
    assert_eq!(error.code, RpcErrorCode::ConfigurationPartiallySaved);
    let StreamEvent::AppChanged { payload } = stream.try_recv().unwrap().event else {
        panic!("expected the final catalog snapshot");
    };
    assert!(
        payload
            .model_catalog
            .providers
            .iter()
            .any(|entry| { entry.provider_id == "combined" && !entry.credential_configured })
    );
    assert!(
        stream.try_recv().is_err(),
        "one publication per save action"
    );
    assert!(
        host.validate_model_selector(Some("combined/model"))
            .is_err()
    );

    drop(auth_guard);
    // 该命令不返回 payload；结果由发布的快照和读取路径承载。
    host.save_provider(provider, Some("synthetic-key")).unwrap();
    let catalog = host.lock_models().redacted_catalog();
    assert!(
        catalog
            .providers
            .iter()
            .any(|entry| { entry.provider_id == "combined" && entry.credential_configured })
    );
    host.validate_model_selector(Some("combined/model"))
        .unwrap();
    assert!(matches!(
        stream.try_recv().unwrap().event,
        StreamEvent::AppChanged { .. }
    ));
    assert!(
        stream.try_recv().is_err(),
        "retry also publishes only the final snapshot"
    );
    assert!(
        !serde_json::to_string(&catalog)
            .unwrap()
            .contains("synthetic-key")
    );
}

#[cfg(windows)]
#[test]
fn failed_credential_removal_refreshes_future_model_selection() {
    use std::os::windows::fs::OpenOptionsExt;

    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.app_server;
    let selector = "openai_compatible/base-model";
    host.validate_model_selector(Some(selector)).unwrap();
    let auth_path = fixture._sessions.home().join("auth.json");
    let auth_guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x00000001 | 0x00000002)
        .open(&auth_path)
        .unwrap();
    host.remove_provider("openai_compatible")
        .expect_err("credential file cannot be replaced");
    assert!(host.validate_model_selector(Some(selector)).is_err());
    assert!(host.lock_models().redacted_catalog().providers.is_empty());
    drop(auth_guard);
    assert!(
        std::fs::read_to_string(&auth_path)
            .unwrap()
            .contains("openai_compatible")
    );
    host.remove_provider("openai_compatible")
        .expect("retry finishes credential removal");
    assert!(
        !std::fs::read_to_string(auth_path)
            .unwrap()
            .contains("openai_compatible")
    );
}
