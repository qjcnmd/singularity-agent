//! 收缩后协议合同测试：stdio JSON-RPC registry、wire shape 与稳定状态文本。

use serde_json::json;
use singularity_protocol::{
    AppEvent, ClientInfo, EmptyParams, ErrorCode, InitializeParams, JsonRpcInbound, JsonRpcMessage,
    Method, MethodKind, ThreadReadParams, ThreadStartParams, ThreadStatus, TurnInjectionParams,
    TurnStartParams, TurnStatus, parse_json_rpc_payload, rpc_methods,
};

#[test]
fn json_rpc_requires_the_2_0_version_member() {
    let error =
        serde_json::from_str::<JsonRpcMessage>(r#"{"method":"thread/list","id":1,"params":{}}"#)
            .expect_err("missing jsonrpc rejected");
    assert!(
        error.to_string().contains("did not match any variant"),
        "{error}"
    );
}

#[test]
fn json_rpc_accepts_typed_request_and_round_trips() {
    let request =
        JsonRpcMessage::request(Method::ThreadList, 1i64, EmptyParams::default()).expect("request");
    let wire = request.to_wire_value();
    assert_eq!(wire["jsonrpc"], "2.0");
    assert_eq!(wire["method"], "thread/list");
    assert_eq!(wire["id"], 1);
    let parsed: JsonRpcMessage = serde_json::from_value(wire).expect("round trip");
    assert_eq!(parsed.method(), Some(Method::ThreadList));
}

#[test]
fn thread_and_turn_start_params_use_codex_style_wire_shape() {
    let params = serde_json::to_value(ThreadStartParams {
        model: Some("provider/model".to_string()),
        cwd: Some("/tmp/work".to_string()),
    })
    .expect("thread params");
    assert_eq!(params["model"], "provider/model");
    assert_eq!(params["cwd"], "/tmp/work");
    assert!(params.get("threadId").is_none());

    let params = serde_json::to_value(TurnStartParams {
        thread_id: "session-id".to_string(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "hello".to_string(),
        }],
    })
    .expect("turn params");
    assert_eq!(params["threadId"], "session-id");
    assert_eq!(params["input"][0]["type"], "text");
}

#[test]
fn injection_and_thread_read_params_are_bounded_by_registry() {
    let steer = serde_json::to_value(TurnInjectionParams {
        turn_id: "turn-id".to_string(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "steer".to_string(),
        }],
    })
    .expect("steer params");
    assert_eq!(steer["turnId"], "turn-id");
    assert!(
        Method::TurnSteer
            .spec()
            .validate_params(steer.clone())
            .is_ok()
    );
    assert!(
        Method::TurnFollowUp
            .spec()
            .validate_params(steer.clone())
            .is_ok()
    );

    let read = json!({"sessionId":"session-id"});
    let params: ThreadReadParams = serde_json::from_value(read).expect("session read params");
    assert_eq!(params.limit, 20);
    assert_eq!(params.before_item, None);
    let explicit: ThreadReadParams = serde_json::from_value(json!({
        "sessionId":"session-id",
        "limit":5,
        "beforeItem":"entry:item"
    }))
    .expect("explicit session read params");
    let wire = serde_json::to_value(&explicit).expect("serialize session read params");
    assert_eq!(wire["sessionId"], "session-id");
    assert_eq!(wire["limit"], 5);
    assert_eq!(wire["beforeItem"], "entry:item");
    assert!(
        Method::ThreadRead
            .spec()
            .validate_params(json!({"sessionId":"x"}))
            .is_ok()
    );
    assert!(
        Method::ThreadRead
            .spec()
            .validate_params(json!({"sessionId":"x","unknown":true}))
            .is_err()
    );
}

#[test]
fn method_registry_keeps_only_converged_methods() {
    let names = singularity_protocol::METHOD_REGISTRY
        .iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    for expected in [
        "initialize",
        "initialized",
        "thread/list",
        "thread/start",
        "thread/settings",
        "thread/read",
        "session/delete",
        "turn/start",
        "turn/steer",
        "turn/followUp",
        "turn/interrupt",
        "provider/status",
        "server/shutdown",
    ] {
        assert!(
            names.contains(&expected),
            "missing method {expected}: {names:?}"
        );
    }
    for removed in [
        "server/capabilities",
        "thread/fork",
        "thread/archive",
        "thread/delete",
        "turn/status",
        "turn/pause",
        "turn/resume",
        "turn/input",
        "event/subscribe",
        "project/trust",
    ] {
        assert!(
            Method::parse(removed).is_none(),
            "removed method still registered: {removed}"
        );
    }
    assert_eq!(Method::TurnSteer.spec().kind, MethodKind::Request);
    assert_eq!(Method::TurnFollowUp.spec().kind, MethodKind::Request);
}

#[test]
fn item_and_tool_execution_events_carry_thread_and_turn_identity() {
    let started = AppEvent::item_started("thread-1", "turn-1", "item-1");
    assert_eq!(started.method, "item/started");
    assert_eq!(started.params["threadId"], "thread-1");
    assert_eq!(started.params["turnId"], "turn-1");
    assert_eq!(started.params["item"]["itemId"], "item-1");

    let delta = AppEvent::item_agent_message_delta("thread-1", "turn-1", "item-1", "chunk");
    assert_eq!(delta.method, "item/agentMessage/delta");
    assert_eq!(delta.params["threadId"], "thread-1");
    assert_eq!(delta.params["turnId"], "turn-1");
    assert_eq!(delta.params["delta"], "chunk");

    let completed = AppEvent::item_completed("thread-1", "turn-1", "item-1");
    assert_eq!(completed.method, "item/completed");
    assert_eq!(completed.params["threadId"], "thread-1");
    assert_eq!(completed.params["turnId"], "turn-1");

    let failed = AppEvent::item_failed("thread-1", "turn-1", "item-1", "bad item");
    assert_eq!(failed.method, "item/failed");
    assert_eq!(failed.params["threadId"], "thread-1");
    assert_eq!(failed.params["turnId"], "turn-1");
    assert_eq!(failed.params["error"], "bad item");

    let args = json!({"command": "echo hi"});
    let start =
        AppEvent::tool_execution_start("thread-1", "turn-1", "call-1", "bash", args.clone());
    assert_eq!(start.method, "tool/execution/start");
    assert_eq!(start.params["threadId"], "thread-1");
    assert_eq!(start.params["turnId"], "turn-1");
    assert_eq!(start.params["toolCallId"], "call-1");
    assert_eq!(start.params["toolName"], "bash");
    assert_eq!(start.params["args"], args);

    let update = AppEvent::tool_execution_update(
        "thread-1",
        "turn-1",
        "call-1",
        "bash",
        json!({"command": "echo hi"}),
        "hi\n",
    );
    assert_eq!(update.params["threadId"], "thread-1");
    assert_eq!(update.params["turnId"], "turn-1");
    assert_eq!(update.params["partialResult"], "hi\n");
    assert_eq!(update.params["toolName"], "bash");
    assert_eq!(update.params["args"]["command"], "echo hi");

    let end = AppEvent::tool_execution_end("thread-1", "turn-1", "call-1", "bash", "done", false);
    assert_eq!(end.params["threadId"], "thread-1");
    assert_eq!(end.params["turnId"], "turn-1");
    assert_eq!(end.params["result"]["content"][0]["text"], "done");
    assert_eq!(end.params["result"]["isError"], false);
}

#[test]
fn json_rpc_payload_yields_single_message_or_invalid_marker() {
    let message =
        parse_json_rpc_payload(r#"{"jsonrpc":"2.0","method":"thread/list","id":1,"params":{}}"#)
            .expect("single request");
    assert!(matches!(
        message,
        JsonRpcInbound::Message(ref msg) if msg.method() == Some(Method::ThreadList)
    ));

    let invalid =
        parse_json_rpc_payload(r#"{"jsonrpc":"2.0","id":42,"method":[]}"#).expect("invalid object");
    assert_eq!(
        invalid,
        JsonRpcInbound::Invalid {
            id: Some(singularity_protocol::JsonRpcId::Number(42))
        }
    );

    let array = parse_json_rpc_payload(r#"[]"#).expect("empty array");
    assert_eq!(array, JsonRpcInbound::Invalid { id: None });
}

#[test]
fn thread_status_projects_last_turn_metadata_not_lifecycle() {
    let thread = singularity_protocol::Thread {
        thread_id: "session-1".to_string(),
        model: None,
        cwd: Some("/tmp/work".to_string()),
        last_turn_status: Some(ThreadStatus::Completed),
    };
    let wire = serde_json::to_value(&thread).expect("thread wire");
    assert_eq!(wire["threadId"], "session-1");
    assert_eq!(wire["lastTurnStatus"], "completed");
    // 尚无 turn：wire 上为 null，而不是伪装成 active。
    let no_turn = singularity_protocol::Thread {
        thread_id: "session-2".to_string(),
        model: None,
        cwd: None,
        last_turn_status: None,
    };
    let wire = serde_json::to_value(&no_turn).expect("thread wire");
    assert_eq!(wire["lastTurnStatus"], serde_json::Value::Null);
    for status in [
        ThreadStatus::Active,
        ThreadStatus::Completed,
        ThreadStatus::Failed,
        ThreadStatus::Interrupted,
    ] {
        assert_eq!(
            ThreadStatus::from_storage_text(status.as_storage_text()),
            Some(status)
        );
    }
    assert_eq!(ThreadStatus::from_storage_text("archived"), None);
}

#[test]
fn turn_status_has_no_paused_suspended_or_blocked_state() {
    assert_eq!(TurnStatus::Running.as_storage_text(), "running");
    assert_eq!(TurnStatus::Completed.as_storage_text(), "completed");
    assert_eq!(TurnStatus::Failed.as_storage_text(), "failed");
    assert_eq!(TurnStatus::Interrupted.as_storage_text(), "interrupted");
    for value in ["paused", "suspended", "blocked"] {
        assert_eq!(TurnStatus::from_storage_text(value), None);
    }
}

#[test]
fn initialize_params_keep_client_info_contract() {
    let params = serde_json::to_value(InitializeParams {
        client_info: ClientInfo::new("sg", "Singularity CLI", "0.1.0"),
    })
    .expect("initialize params");
    assert_eq!(params["clientInfo"]["name"], "sg");
    assert!(params.get("capabilities").is_none());
    assert!(
        serde_json::from_value::<InitializeParams>(serde_json::json!({
            "clientInfo": {"name": "sg", "title": "Singularity CLI", "version": "0.1.0"},
            "capabilities": {}
        }))
        .is_err()
    );
    let _: rpc_methods::Initialize = rpc_methods::Initialize;
    assert!(Method::Initialize.spec().validate_params(params).is_ok());
    assert_eq!(ErrorCode::not_initialized().message(), "Not initialized");
}

#[test]
fn diagnostic_and_provider_attempt_events_are_safe_and_named() {
    let diagnostic = AppEvent::agent_diagnostic(
        "thread-1",
        "turn-1",
        "warning",
        "compaction_skipped",
        "automatic context compaction skipped",
    );
    assert_eq!(diagnostic.method(), "agent/diagnostic");
    assert_eq!(diagnostic.params["code"], "compaction_skipped");
    assert!(!diagnostic.params.to_string().contains("raw"));

    let attempt = AppEvent::provider_attempt(
        "thread-1",
        "turn-1",
        2,
        "completion",
        "openai",
        "test-model",
        "open_ai_responses",
        1,
        "error",
        Some(12),
        Some(true),
        Some(4),
        Some("network".to_string()),
        Some("provider_timeout".to_string()),
    );
    assert_eq!(attempt.method(), "provider/attempt");
    assert_eq!(attempt.params["modelTurnOrdinal"], 2);
    assert_eq!(attempt.params["retryScheduled"], true);
    assert!(attempt.params.get("raw").is_none());
}
