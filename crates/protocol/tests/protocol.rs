//! 收缩后协议合同测试：stdio JSON-RPC registry、wire shape 与稳定状态文本。

use serde_json::json;
use singularity_core::ClientInfo;
use singularity_protocol::{
    AgentDiagnosticParams, AppEvent, EmptyParams, InitializeParams, JsonRpcInbound, JsonRpcMessage,
    Method, MethodKind, ProviderAttemptEventParams, ProviderAttemptSummaryParams,
    SessionReadParams, ThreadStartParams, ThreadStatus, TurnInjectionParams, TurnStartParams,
    TurnStatus, parse_json_rpc_payload, rpc_methods,
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
fn injection_and_session_read_params_are_bounded_by_registry() {
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
    let params: SessionReadParams = serde_json::from_value(read).expect("session read params");
    assert_eq!(params.limit, 20);
    assert_eq!(params.cursor, None);
    assert_eq!(params.sort_direction, None);
    assert_eq!(params.detail, None);
    assert!(params.kinds.is_empty());
    let explicit: SessionReadParams = serde_json::from_value(json!({
        "sessionId":"session-id",
        "cursor":"sg1t3",
        "limit":5,
        "sortDirection":"asc",
        "detail":"summary",
        "kinds":["message","turn"]
    }))
    .expect("explicit session read params");
    let wire = serde_json::to_value(&explicit).expect("serialize session read params");
    assert_eq!(wire["sessionId"], "session-id");
    assert_eq!(wire["cursor"], "sg1t3");
    assert_eq!(wire["sortDirection"], "asc");
    assert_eq!(wire["detail"], "summary");
    assert_eq!(wire["kinds"][0], "message");
    assert!(
        Method::SessionRead
            .spec()
            .validate_params(json!({"sessionId":"x"}))
            .is_ok()
    );
    assert!(
        Method::SessionRead
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
        "thread/resume",
        "thread/settings",
        "session/read",
        "session/delete",
        "turn/start",
        "turn/steer",
        "turn/followUp",
        "turn/interrupt",
        "agent/capability",
        "server/shutdown",
    ] {
        assert!(
            names.contains(&expected),
            "missing method {expected}: {names:?}"
        );
    }
    for removed in [
        "server/capabilities",
        "thread/read",
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
    assert_eq!(wire["thread_id"], "session-1");
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
}

#[test]
fn typed_diagnostic_and_provider_attempt_events_have_safe_params() {
    let diagnostic = AppEvent::agent_diagnostic(
        "thread-1",
        "turn-1",
        "warning",
        "compaction_skipped",
        "automatic context compaction skipped",
    );
    let diagnostic_params: AgentDiagnosticParams =
        serde_json::from_value(diagnostic.params.clone()).expect("diagnostic params");
    assert_eq!(diagnostic.method(), "agent/diagnostic");
    assert_eq!(diagnostic_params.code, "compaction_skipped");
    assert!(!diagnostic.params.to_string().contains("raw"));

    let attempt = AppEvent::provider_attempt(
        "thread-1",
        "turn-1",
        2,
        "completion",
        "openai",
        "gpt-test",
        "open_ai_responses",
        1,
        "error",
        Some(12),
        Some(true),
        Some(4),
        Some("network".to_string()),
        Some("provider_timeout".to_string()),
    );
    let attempt_params: ProviderAttemptEventParams =
        serde_json::from_value(attempt.params.clone()).expect("attempt params");
    assert_eq!(attempt.method(), "provider/attempt");
    assert_eq!(attempt_params.model_turn_ordinal, 2);
    assert_eq!(attempt_params.retry_scheduled, Some(true));
    assert!(attempt.params.get("raw").is_none());

    let summary = AppEvent::provider_attempt_summary("thread-1", "turn-1", 2, 2, 1, 20);
    let summary_params: ProviderAttemptSummaryParams =
        serde_json::from_value(summary.params.clone()).expect("summary params");
    assert_eq!(summary.method(), "provider/attempt/summary");
    assert_eq!(summary_params.attempt_count, 2);
    assert_eq!(summary_params.latency_ms, 20);
}

#[test]
fn event_metadata_has_no_gap_variant() {
    let state = serde_json::to_value(singularity_protocol::EventClass::State).unwrap();
    assert_eq!(state, "state");
    let progress = serde_json::to_value(singularity_protocol::EventClass::Progress).unwrap();
    assert_eq!(progress, "progress");

    assert!(serde_json::from_str::<singularity_protocol::EventClass>("\"gap\"").is_err());
    assert!(serde_json::from_str::<singularity_protocol::EventDelivery>("\"gap\"").is_err());
}
