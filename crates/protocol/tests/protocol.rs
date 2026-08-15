//! 收缩后协议合同测试：stdio JSON-RPC registry、wire shape 与稳定状态文本。

use serde_json::json;
use singularity_core::ClientInfo;
use singularity_protocol::{
    EmptyParams, InitializeParams, JsonRpcMessage, JsonRpcPayload, Method, MethodKind,
    SessionReadParams, ThreadStartParams, TurnInjectionParams, TurnStartParams, TurnStatus,
    parse_json_rpc_payload, rpc_methods,
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
    assert_eq!(params.recent_limit, 20);
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
        "server/capabilities",
        "thread/list",
        "thread/start",
        "thread/resume",
        "session/read",
        "session/delete",
        "turn/start",
        "turn/steer",
        "turn/followUp",
        "turn/interrupt",
        "agent/capability",
        "project/trust",
        "server/shutdown",
    ] {
        assert!(
            names.contains(&expected),
            "missing method {expected}: {names:?}"
        );
    }
    for removed in [
        "thread/read",
        "thread/fork",
        "thread/archive",
        "thread/delete",
        "turn/status",
        "turn/pause",
        "turn/resume",
        "turn/input",
        "event/subscribe",
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
fn json_rpc_payload_still_distinguishes_batches_at_parser_boundary() {
    assert_eq!(
        parse_json_rpc_payload("[]").expect("empty"),
        JsonRpcPayload::EmptyBatch
    );
    let mixed = parse_json_rpc_payload(
        r#"[{"jsonrpc":"2.0","method":"thread/list","id":1,"params":{}}, {"jsonrpc":"2.0","method":"unknown","id":2,"params":{}}]"#,
    )
    .expect("batch");
    assert!(matches!(mixed, JsonRpcPayload::Batch(items) if items.len() == 2));
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
        capabilities: None,
    })
    .expect("initialize params");
    assert_eq!(params["clientInfo"]["name"], "sg");
    let _: rpc_methods::Initialize = rpc_methods::Initialize;
    assert!(Method::Initialize.spec().validate_params(params).is_ok());
}
