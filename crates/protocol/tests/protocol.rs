use singularity_core::ClientInfo;
use singularity_protocol::{
    AgentCapabilityResult, AppEvent, ArtifactFetchParams, ArtifactRef, ConversationMessage,
    ConversationRole, EventSubscribeParams, InitializeParams, InitializeResult, ItemKind,
    JsonRpcMessage, Method, ProviderReadiness, ThreadIdParams, ThreadReadParams, ThreadReadResult,
    ThreadStartParams, TraceListParams, TraceShowParams, TraceTailParams, TurnIdParams,
    TurnStartParams,
};

#[test]
fn json_rpc_accepts_omitted_jsonrpc_header_and_keeps_camel_case_params() {
    let raw = r#"{"method":"turn/start","id":2,"params":{"threadId":"thread_1","input":[{"type":"text","text":"hi"}]}}"#;
    let message: JsonRpcMessage = serde_json::from_str(raw).expect("parse json-rpc message");

    assert_eq!(message.method(), Some(Method::TurnStart));
    assert_eq!(message.id().and_then(|id| id.as_i64()), Some(2));

    let params: TurnStartParams = message.params_as().expect("decode params");
    assert_eq!(params.thread_id, "thread_1");
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
    let raw = r#"{"method":"turn/start","id":2,"params":{"threadId":"thread_1","agentHost":"alternate","input":[{"type":"text","text":"hi"}]}}"#;
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
    assert_eq!(serde_json::to_value(thread).unwrap()["model"], "gpt-test");

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
fn agent_capability_uses_the_canonical_agent_loop_wire_name() {
    let result = AgentCapabilityResult {
        agent_loop: serde_json::json!({"available": true}),
        provider_readiness: ProviderReadiness {
            source: None,
            snapshot_id: "provider_snapshot_test".to_string(),
            ready: false,
            blocker: None,
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
fn json_rpc_wire_output_omits_null_jsonrpc_result_and_error_fields() {
    let request = JsonRpcMessage::request(
        Method::Initialize,
        serde_json::json!(1),
        serde_json::json!({"clientInfo": {"name": "test", "title": "Test", "version": "0.1.0"}}),
    );
    let value = request.to_wire_value();

    assert_eq!(value["method"], "initialize");
    assert!(value.get("jsonrpc").is_none());
    assert!(value.get("result").is_none());
    assert!(value.get("error").is_none());
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
            event_types: vec!["turn/started".to_string()]
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
fn item_kind_uses_codex_style_wire_names() {
    assert_eq!(
        serde_json::to_value(ItemKind::CommandExecution).unwrap(),
        "commandExecution"
    );
}

#[test]
fn new_trace_is_unredacted_until_the_store_sanitizes_it() {
    let trace =
        singularity_protocol::TraceEvent::new("trace_1", "run_1", "session_1", "test", "summary");

    assert!(!trace.redaction_applied);
    assert!(trace.payload_hash.is_empty());
}

#[test]
fn provider_readiness_uses_camel_case_redacted_wire_fields() {
    let readiness = ProviderReadiness {
        source: Some("process_env".to_string()),
        snapshot_id: "provider_snapshot_test".to_string(),
        ready: false,
        blocker: Some("required_env_missing".to_string()),
        api_key_present: true,
        base_url_present: false,
        model_present: true,
    };

    assert_eq!(
        serde_json::to_value(readiness).unwrap(),
        serde_json::json!({
            "source": "process_env",
            "snapshotId": "provider_snapshot_test",
            "ready": false,
            "blocker": "required_env_missing",
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
