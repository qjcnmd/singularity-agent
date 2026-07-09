use singularity_core::ClientInfo;
use singularity_protocol::{
    AppEvent, ArtifactFetchParams, ArtifactRef, EventSubscribeParams, InitializeParams, ItemKind,
    JsonRpcMessage, Method, ThreadIdParams, ThreadStartParams, TraceListParams, TraceShowParams,
    TraceTailParams, TurnIdParams, TurnStartParams,
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
fn turn_start_params_reject_agent_host_selector() {
    let raw = r#"{"method":"turn/start","id":2,"params":{"threadId":"thread_1","agentHost":"python","input":[{"type":"text","text":"hi"}]}}"#;
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
    assert_eq!(
        serde_json::to_value(ItemKind::McpToolCall).unwrap(),
        "mcpToolCall"
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
