//! JSON-RPC 请求、响应、事件和参数 schema 的协议测试。

use singularity_core::ClientInfo;
use singularity_policy::{ApprovalPolicy, PermissionProfileName};
use singularity_protocol::{
    AgentCapabilityResult, AgentLoopCapabilityStatus, AppEvent, ArtifactFetchParams, ArtifactRef,
    ConversationMessage, ConversationRole, EventClass, EventDelivery, EventGap, EventGapReason,
    EventMetadata, EventRecoveryQuery, EventSubscribeParams, InitializeParams, InitializeResult,
    ItemKind, ItemStatus, JsonRpcBatchItem, JsonRpcId, JsonRpcMessage, JsonRpcPayload,
    METHOD_REGISTRY, Method, MethodKind, ProviderConfigurationStatus, ThreadIdParams,
    ThreadReadParams, ThreadReadResult, ThreadStartParams, ThreadStatus, TraceListParams,
    TraceShowParams, TraceTailParams, TurnIdParams, TurnStartParams, TurnStatus,
    parse_json_rpc_payload,
};

#[test]
fn json_rpc_requires_the_2_0_version_member() {
    let raw = r#"{"method":"turn/start","id":2,"params":{"threadId":"thread_1","input":[{"type":"text","text":"hi"}]}}"#;
    assert!(serde_json::from_str::<JsonRpcMessage>(raw).is_err());

    let wrong_version = r#"{"jsonrpc":"1.0","method":"turn/start","id":2,"params":{"threadId":"thread_1","input":[{"type":"text","text":"hi"}]}}"#;
    assert!(serde_json::from_str::<JsonRpcMessage>(wrong_version).is_err());
}

#[test]
fn json_rpc_rejects_ambiguous_envelopes_and_non_scalar_ids() {
    for raw in [
        r#"{"jsonrpc":"2.0","method":"thread/list","id":1,"params":{},"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603,"message":"boom"}}"#,
        r#"{"jsonrpc":"2.0","method":"thread/list","id":{"nested":true},"params":{}}"#,
        r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"boom"}}"#,
    ] {
        assert!(
            serde_json::from_str::<JsonRpcMessage>(raw).is_err(),
            "invalid envelope was accepted: {raw}"
        );
    }
}

#[test]
fn json_rpc_accepts_and_echoes_every_standard_id_shape() {
    for (raw, expected) in [
        (
            r#"{"jsonrpc":"2.0","method":"thread/list","id":null,"params":{}}"#,
            JsonRpcId::Null,
        ),
        (
            r#"{"jsonrpc":"2.0","method":"thread/list","id":1.5,"params":{}}"#,
            JsonRpcId::Fraction(1.5),
        ),
        (
            r#"{"jsonrpc":"2.0","method":"thread/list","id":18446744073709551615,"params":{}}"#,
            JsonRpcId::Unsigned(u64::MAX),
        ),
    ] {
        let request_value: serde_json::Value = serde_json::from_str(raw).expect("request JSON");
        let message: JsonRpcMessage = serde_json::from_str(raw).expect("standard request id");
        assert_eq!(message.id(), Some(&expected));
        let response = JsonRpcMessage::response(expected, serde_json::json!({})).to_wire_value();
        assert_eq!(response["id"], request_value["id"]);
    }
}

#[test]
fn thread_policy_params_round_trip_and_reject_unknown_public_values() {
    let params: ThreadStartParams = serde_json::from_value(serde_json::json!({
        "model": "gpt-test",
        "cwd": "C:/workspace",
        "sandboxMode": "read-only",
        "approvalPolicy": "never"
    }))
    .expect("thread/start policy params");
    assert_eq!(params.sandbox_mode, Some(PermissionProfileName::ReadOnly));
    assert_eq!(params.approval_policy, Some(ApprovalPolicy::Never));
    assert_eq!(
        serde_json::to_value(params).expect("serialize thread/start policy params"),
        serde_json::json!({
            "model": "gpt-test",
            "cwd": "C:/workspace",
            "sandboxMode": "read-only",
            "approvalPolicy": "never"
        })
    );

    for value in [
        serde_json::json!({"sandboxMode": "unsupported-mode"}),
        serde_json::json!({"approvalPolicy": "untrusted"}),
    ] {
        let result = serde_json::from_value::<ThreadStartParams>(value);
        assert!(result.is_err(), "unknown policy value was accepted");
    }
}

#[test]
fn thread_read_uses_typed_paginated_safe_conversation_history() {
    let params: ThreadReadParams = serde_json::from_value(serde_json::json!({
        "threadId": "thread_1",
        "beforeTurnSequence": 7,
        "limit": 5
    }))
    .expect("thread/read params");
    assert_eq!(params.thread_id, "thread_1");
    assert_eq!(params.before_turn_sequence, Some(7));
    assert_eq!(params.limit, Some(5));

    let result = ThreadReadResult {
        thread: singularity_protocol::Thread {
            thread_id: "thread_1".to_string(),
            model: Some("gpt-test".to_string()),
            cwd: Some("C:/workspace".to_string()),
            status: singularity_protocol::ThreadStatus::Active,
            sandbox_mode: PermissionProfileName::WorkspaceWrite,
            approval_policy: ApprovalPolicy::OnRequest,
        },
        messages: vec![ConversationMessage {
            item_id: "item_1".to_string(),
            turn_id: "turn_1".to_string(),
            turn_sequence: 1,
            item_sequence: 1,
            role: ConversationRole::User,
            content: "hello".to_string(),
            redacted: false,
        }],
        next_before_turn_sequence: Some(1),
    };

    assert_eq!(
        serde_json::to_value(result).expect("thread/read result"),
        serde_json::json!({
            "thread": {
                "thread_id": "thread_1",
                "model": "gpt-test",
                "cwd": "C:/workspace",
                "status": "active",
                "sandboxMode": "workspace-write",
                "approvalPolicy": "on-request"
            },
            "messages": [{
                "itemId": "item_1",
                "turnId": "turn_1",
                "turnSequence": 1,
                "itemSequence": 1,
                "role": "user",
                "content": "hello",
                "redacted": false
            }],
            "nextBeforeTurnSequence": 1
        })
    );
}
#[test]
fn turn_start_params_reject_agent_host_selector() {
    let raw = r#"{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{"threadId":"thread_1","agentHost":"alternate","input":[{"type":"text","text":"hi"}]}}"#;
    let message: JsonRpcMessage = serde_json::from_str(raw).expect("parse json-rpc message");

    let error = message
        .params_as::<TurnStartParams>()
        .expect_err("agentHost is not a public turn/start parameter");

    assert!(error.to_string().contains("unknown field `agentHost`"));
}

#[test]
fn initialize_and_thread_start_params_have_codex_style_wire_shape() {
    let initialize = InitializeParams {
        client_info: ClientInfo::new("test", "Test", "0.1.0"),
        capabilities: None,
    };
    let value = serde_json::to_value(&initialize).expect("serialize initialize params");
    assert_eq!(value["clientInfo"]["name"], "test");

    let initialize_result = serde_json::to_value(InitializeResult::local()).unwrap();
    assert_eq!(
        initialize_result["userAgent"],
        concat!("singularity-app-server/", env!("CARGO_PKG_VERSION"))
    );

    let thread = ThreadStartParams {
        model: Some("gpt-test".to_string()),
        cwd: Some("C:/repo".to_string()),
        sandbox_mode: Some(PermissionProfileName::ReadOnly),
        approval_policy: Some(ApprovalPolicy::Never),
    };
    assert_eq!(
        serde_json::to_value(thread).unwrap(),
        serde_json::json!({
            "model": "gpt-test",
            "cwd": "C:/repo",
            "sandboxMode": "read-only",
            "approvalPolicy": "never"
        })
    );

    assert_eq!(
        AppEvent::item_completed("item_1").method(),
        "item/completed"
    );
    assert_eq!(
        AppEvent::item_agent_message_delta("item_1", "hi").method(),
        "item/agentMessage/delta"
    );
    assert_eq!(
        AppEvent::item_command_execution_output_delta("item_1", "stdout", "hi").method(),
        "item/commandExecution/outputDelta"
    );
}

#[test]
fn event_metadata_is_typed_and_recovery_queries_are_bounded() {
    let event = AppEvent::turn_completed(&singularity_protocol::Turn {
        turn_id: "turn_1".to_string(),
        thread_id: "thread_1".to_string(),
        status: singularity_protocol::TurnStatus::Completed,
        agent_loop_status: "completed".to_string(),
    });
    let value = event
        .to_notification_with_metadata(EventMetadata {
            sequence: 4,
            cursor: 4,
            class: EventClass::State,
            delivery: EventDelivery::Reliable,
            recovery_query: Some(EventRecoveryQuery::TurnStatus {
                turn_id: "turn_1".to_string(),
            }),
            gap: None,
        })
        .to_wire_value();
    assert_eq!(value["params"]["event"]["sequence"], 4);
    assert_eq!(value["params"]["event"]["delivery"], "reliable");
    assert_eq!(
        value["params"]["event"]["recoveryQuery"]["method"],
        "turn/status"
    );

    let gap = EventMetadata {
        sequence: 5,
        cursor: 5,
        class: EventClass::Gap,
        delivery: EventDelivery::Gap,
        recovery_query: None,
        gap: Some(EventGap {
            reason: EventGapReason::ProgressDropped,
            from_cursor: 5,
            to_cursor: 5,
        }),
    };
    assert!(
        serde_json::from_value::<EventMetadata>(serde_json::json!({
            "sequence": 5,
            "cursor": 5,
            "class": "gap",
            "delivery": "gap",
            "gap": serde_json::to_value(gap).unwrap(),
            "recoveryQuery": {"method": "store/resync", "params": {}}
        }))
        .is_err()
    );
}

#[test]
fn agent_capability_uses_the_canonical_agent_loop_wire_name() {
    let result = AgentCapabilityResult {
        agent_loop: AgentLoopCapabilityStatus {
            available: true,
            status: "completed".to_string(),
            reason: "ready".to_string(),
            blockers: Vec::new(),
        },
        provider_configuration: ProviderConfigurationStatus {
            source: None,
            snapshot_id: "provider_snapshot_test".to_string(),
            configured: false,
            configuration_blocker: None,
            api_key_present: false,
            base_url_present: false,
            model_present: false,
        },
    };

    let value = serde_json::to_value(result).unwrap();
    assert_eq!(value["agentLoop"]["available"], true);
    assert_eq!(value.as_object().map(serde_json::Map::len), Some(2));
}

#[test]
fn json_rpc_wire_output_is_a_standard_request_envelope() {
    let request = JsonRpcMessage::request(
        Method::Initialize,
        1,
        serde_json::json!({"clientInfo": {"name": "test", "title": "Test", "version": "0.1.0"}}),
    )
    .expect("request serializes");
    let value = request.to_wire_value();

    assert_eq!(value["method"], "initialize");
    assert_eq!(value["jsonrpc"], "2.0");
    assert!(value.get("result").is_none());
    assert!(value.get("error").is_none());
}

#[test]
fn json_rpc_payload_distinguishes_empty_single_and_mixed_batch() {
    assert_eq!(
        parse_json_rpc_payload("[]").expect("empty batch"),
        JsonRpcPayload::EmptyBatch
    );

    let single = parse_json_rpc_payload(
        r#"{"jsonrpc":"2.0","method":"thread/list","id":"request-1","params":{}}"#,
    )
    .expect("single request");
    assert!(matches!(
        single,
        JsonRpcPayload::Single(JsonRpcBatchItem::Message(message))
            if message.id() == Some(&JsonRpcId::String("request-1".to_string()))
    ));

    let mixed = parse_json_rpc_payload(
        r#"[{"jsonrpc":"2.0","method":"thread/list","id":1,"params":{}},{"jsonrpc":"2.0","method":"initialized","params":{}},false]"#,
    )
    .expect("mixed batch");
    assert!(matches!(mixed, JsonRpcPayload::Batch(items) if items.len() == 3));

    assert!(parse_json_rpc_payload("{").is_err());
    let parse_error = JsonRpcMessage::parse_error().to_wire_value();
    assert_eq!(parse_error["jsonrpc"], "2.0");
    assert_eq!(parse_error["id"], serde_json::Value::Null);
    assert_eq!(parse_error["error"]["code"], -32700);
}

#[test]
fn method_registry_is_the_unique_name_and_contract_source() {
    assert_eq!(METHOD_REGISTRY.len(), 25);
    for spec in METHOD_REGISTRY {
        assert_eq!(Method::parse(spec.name), Some(spec.method));
        assert_eq!(spec.method.as_str(), spec.name);
        assert!(spec.params_schema().is_object());
        assert!(spec.result_schema().is_object());
    }

    let thread_read = Method::ThreadRead.spec();
    assert!(
        thread_read
            .validate_params(serde_json::json!({"threadId":"thread_1"}))
            .is_ok()
    );
    assert!(thread_read.validate_params(serde_json::json!({})).is_err());
    assert_eq!(Method::Initialized.spec().kind, MethodKind::Notification);
    assert_eq!(Method::ThreadRead.spec().kind, MethodKind::Request);
}

#[test]
fn protocol_v1_methods_use_codex_names_without_cancel_or_generic_delta() {
    for method in [
        "thread/list",
        "thread/read",
        "thread/resume",
        "thread/fork",
        "thread/archive",
        "thread/delete",
        "turn/start",
        "turn/interrupt",
        "turn/status",
        "approval/list",
        "approval/center",
        "approval/request",
        "approval/decision",
        "event/subscribe",
        "artifact/fetch",
        "trace/list",
        "trace/show",
        "trace/tail",
        "server/shutdown",
    ] {
        let parsed = Method::parse(method).expect("method is registered");
        assert_eq!(parsed.as_str(), method);
    }

    assert!(Method::parse("turn/cancel").is_none());
    assert_ne!(
        AppEvent::item_agent_message_delta("item_1", "delta").method(),
        "item/delta"
    );
}

#[test]
fn protocol_v1_id_params_are_camel_case_on_wire() {
    assert_eq!(
        serde_json::to_value(ThreadIdParams {
            thread_id: "thread_1".to_string()
        })
        .unwrap(),
        serde_json::json!({"threadId": "thread_1"})
    );
    assert_eq!(
        serde_json::to_value(TurnIdParams {
            turn_id: "turn_1".to_string()
        })
        .unwrap(),
        serde_json::json!({"turnId": "turn_1"})
    );
    assert_eq!(
        serde_json::to_value(TraceListParams {
            run_id: "run_1".to_string(),
            limit: None,
            offset: None
        })
        .unwrap(),
        serde_json::json!({"runId": "run_1"})
    );
    assert_eq!(
        serde_json::to_value(TraceShowParams {
            event_id: "event_1".to_string()
        })
        .unwrap(),
        serde_json::json!({"eventId": "event_1"})
    );
    assert_eq!(
        serde_json::to_value(TraceTailParams {
            run_id: "run_1".to_string(),
            limit: Some(2),
            offset: Some(1)
        })
        .unwrap(),
        serde_json::json!({"runId": "run_1", "limit": 2, "offset": 1})
    );
    assert_eq!(
        serde_json::to_value(EventSubscribeParams {
            event_types: vec!["turn/started".to_string()],
            cursor: None,
        })
        .unwrap(),
        serde_json::json!({"eventTypes": ["turn/started"]})
    );
    assert_eq!(
        serde_json::to_value(ArtifactFetchParams {
            artifact_id: "artifact_1".to_string()
        })
        .unwrap(),
        serde_json::json!({"artifactId": "artifact_1"})
    );
}

#[test]
fn thread_id_params_reject_unknown_fields() {
    let result = serde_json::from_value::<ThreadIdParams>(serde_json::json!({
        "threadId": "thread_1",
        "sandboxMode": "read-only"
    }));
    assert!(result.is_err());
}

#[test]
fn item_kind_uses_codex_style_wire_names() {
    assert_eq!(
        serde_json::to_value(ItemKind::CommandExecution).unwrap(),
        "commandExecution"
    );
}

#[test]
fn storage_enum_text_is_stable_and_rejects_unknown_values() {
    assert_eq!(ThreadStatus::Active.as_storage_text(), "active");
    assert_eq!(TurnStatus::Blocked.as_storage_text(), "blocked");
    assert_eq!(
        ItemKind::CommandExecution.as_storage_text(),
        "commandExecution"
    );
    assert_eq!(ItemStatus::Completed.as_storage_text(), "completed");
    assert_eq!(
        ThreadStatus::from_storage_text("archived"),
        Some(ThreadStatus::Archived)
    );
    assert_eq!(TurnStatus::from_storage_text("unknown"), None);
    assert_eq!(ItemKind::from_storage_text("command_execution"), None);
}

#[test]
fn new_trace_is_unredacted_until_the_store_sanitizes_it() {
    let trace =
        singularity_protocol::TraceEvent::new("trace_1", "run_1", "session_1", "test", "summary");

    assert!(!trace.redaction_applied);
    assert!(trace.payload_hash.is_empty());
}

#[test]
fn turn_trace_has_all_three_binding_fields() {
    let trace = singularity_protocol::TraceEvent::for_turn(
        "trace_turn",
        "thread_1",
        "turn_1",
        "test",
        "summary",
    );

    assert_eq!(trace.run_id, "thread_1");
    assert_eq!(trace.session_id, "turn_1");
    assert_eq!(trace.task_id.as_deref(), Some("turn_1"));
    trace
        .validate_turn_binding("thread_1", "turn_1")
        .expect("turn binding");
}

#[test]
fn provider_configuration_uses_camel_case_redacted_wire_fields() {
    let configuration = ProviderConfigurationStatus {
        source: Some("process_env".to_string()),
        snapshot_id: "provider_snapshot_test".to_string(),
        configured: false,
        configuration_blocker: Some("required_env_missing".to_string()),
        api_key_present: true,
        base_url_present: false,
        model_present: true,
    };

    assert_eq!(
        serde_json::to_value(configuration).unwrap(),
        serde_json::json!({
            "source": "process_env",
            "snapshotId": "provider_snapshot_test",
            "configured": false,
            "configurationBlocker": "required_env_missing",
            "apiKeyPresent": true,
            "baseUrlPresent": false,
            "modelPresent": true
        })
    );
}

#[test]
fn artifact_ref_uses_camel_case_wire_ids_and_redaction_marker() {
    let artifact = ArtifactRef {
        artifact_id: "artifact_1".to_string(),
        run_id: "run_1".to_string(),
        item_id: Some("item_1".to_string()),
        kind: "file".to_string(),
        uri: "artifact://run_1/result.txt".to_string(),
        content_digest: "sha256:abc".to_string(),
        summary: "short result".to_string(),
        metadata: serde_json::json!({"bytes": 12}),
        redacted: true,
    };

    let value = serde_json::to_value(artifact).expect("serialize artifact");

    assert_eq!(value["artifactId"], "artifact_1");
    assert_eq!(value["runId"], "run_1");
    assert_eq!(value["itemId"], "item_1");
    assert_eq!(value["contentDigest"], "sha256:abc");
    assert!(value.get("artifact_id").is_none());
}
