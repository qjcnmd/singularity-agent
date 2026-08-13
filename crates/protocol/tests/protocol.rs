//! JSON-RPC 请求、响应、事件和参数 schema 的协议测试。

use singularity_core::{ClientInfo, ErrorCode};
use singularity_protocol::{
    AgentCapabilityResult, AgentLoopCapabilityStatus, AppEvent, ConversationMessage,
    ConversationRole, EventClass, EventDelivery, EventMetadata, EventSubscribeParams,
    InitializeParams, InitializeResult, ItemKind, ItemStatus, JsonRpcBatchItem, JsonRpcId,
    JsonRpcMessage, JsonRpcPayload, METHOD_REGISTRY, Method, MethodKind,
    ProviderConfigurationStatus, ThreadIdParams, ThreadReadParams, ThreadReadResult,
    ThreadStartParams, ThreadStatus, TurnIdParams, TurnStartParams, TurnStatus,
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
fn json_rpc_accepts_and_echoes_every_supported_id_shape() {
    for (raw, expected) in [
        (
            r#"{"jsonrpc":"2.0","method":"thread/list","id":"request-1","params":{}}"#,
            JsonRpcId::String("request-1".to_string()),
        ),
        (
            r#"{"jsonrpc":"2.0","method":"thread/list","id":-7,"params":{}}"#,
            JsonRpcId::Number(-7),
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
fn json_rpc_rejects_explicit_null_ids_for_request_and_notification() {
    for raw in [
        r#"{"jsonrpc":"2.0","method":"thread/list","id":null,"params":{}}"#,
        r#"{"jsonrpc":"2.0","method":"initialized","id":null,"params":{}}"#,
    ] {
        assert!(serde_json::from_str::<JsonRpcMessage>(raw).is_err());
        assert_eq!(
            parse_json_rpc_payload(raw).expect("valid JSON frame"),
            JsonRpcPayload::Single(JsonRpcBatchItem::Invalid { id: None })
        );
    }

    let response = JsonRpcMessage::invalid_request(None).to_wire_value();
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(response["error"]["code"], -32600);
    assert_eq!(response["error"]["message"], "Invalid Request");
}

#[test]
fn json_rpc_rejects_fractional_ids_as_invalid_request_with_null_id() {
    let raw = r#"{"jsonrpc":"2.0","method":"thread/list","id":1.5,"params":{}}"#;

    assert!(serde_json::from_str::<JsonRpcMessage>(raw).is_err());
    assert_eq!(
        parse_json_rpc_payload(raw).expect("valid JSON frame"),
        JsonRpcPayload::Single(JsonRpcBatchItem::Invalid { id: None })
    );

    let response = JsonRpcMessage::invalid_request(None).to_wire_value();
    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(response["error"]["code"], -32600);
    assert_eq!(response["error"]["message"], "Invalid Request");
}

#[test]
fn json_rpc_unassociated_error_responses_round_trip_with_null_id() {
    for message in [
        JsonRpcMessage::parse_error(),
        JsonRpcMessage::invalid_request(None),
        JsonRpcMessage::error(None, ErrorCode::new(-32603, "Internal error")),
    ] {
        let wire = message.to_wire_value();
        assert_eq!(wire["id"], serde_json::Value::Null);
        let parsed = serde_json::from_value::<JsonRpcMessage>(wire).expect("error response");
        assert_eq!(parsed.id(), Some(&JsonRpcId::Null));
    }
}

#[test]
fn thread_start_params_round_trip_and_reject_unknown_public_values() {
    let params: ThreadStartParams = serde_json::from_value(serde_json::json!({
        "model": "gpt-test",
        "cwd": "C:/workspace",
    }))
    .expect("thread/start params");
    assert_eq!(params.model.as_deref(), Some("gpt-test"));
    assert_eq!(params.cwd.as_deref(), Some("C:/workspace"));
    assert_eq!(
        serde_json::to_value(params).expect("serialize thread/start params"),
        serde_json::json!({
            "model": "gpt-test",
            "cwd": "C:/workspace",
        })
    );

    for value in [
        serde_json::json!({"sandboxMode": "read-only"}),
        serde_json::json!({"approvalPolicy": "on-request"}),
    ] {
        let result = serde_json::from_value::<ThreadStartParams>(value);
        assert!(result.is_err(), "removed policy value was accepted");
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
                "status": "active"
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
    };
    assert_eq!(
        serde_json::to_value(thread).unwrap(),
        serde_json::json!({
            "model": "gpt-test",
            "cwd": "C:/repo",
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
}

#[test]
fn event_metadata_is_class_and_delivery_only() {
    let event = AppEvent::turn_completed(&singularity_protocol::Turn {
        turn_id: "turn_1".to_string(),
        thread_id: "thread_1".to_string(),
        status: singularity_protocol::TurnStatus::Completed,
        agent_loop_status: "completed".to_string(),
    });
    let value = event
        .to_notification_with_metadata(EventMetadata {
            class: EventClass::State,
            delivery: EventDelivery::Reliable,
        })
        .to_wire_value();
    assert_eq!(value["params"]["event"]["class"], "state");
    assert_eq!(value["params"]["event"]["delivery"], "reliable");
    assert!(value["params"]["event"].get("sequence").is_none());
    assert!(value["params"]["event"].get("cursor").is_none());
    assert!(value["params"]["event"].get("recoveryQuery").is_none());
    assert!(value["params"]["event"].get("gap").is_none());

    // 已删除的 cursor/gap/recovery 字段不再属于事件信封。
    for metadata in [
        serde_json::json!({
            "sequence": 4,
            "cursor": 4,
            "class": "state",
            "delivery": "reliable",
            "recoveryQuery": {"method": "turn/status", "params": {"turnId": "turn_1"}}
        }),
        serde_json::json!({
            "class": "gap",
            "delivery": "gap",
            "gap": {"reason": "progress_dropped", "fromCursor": 5, "toCursor": 5}
        }),
        serde_json::json!({"class": "state"}),
    ] {
        assert!(
            serde_json::from_value::<EventMetadata>(metadata).is_err(),
            "removed event envelope was accepted"
        );
    }
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
    assert_eq!(METHOD_REGISTRY.len(), 19);
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
    assert_eq!(Method::parse("eval/run"), None);
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
        "turn/input",
        "turn/pause",
        "turn/resume",
        "turn/interrupt",
        "turn/status",
        "event/subscribe",
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
        serde_json::to_value(EventSubscribeParams {
            event_types: vec!["turn/started".to_string()],
            cursor: None,
        })
        .unwrap(),
        serde_json::json!({"eventTypes": ["turn/started"]})
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
