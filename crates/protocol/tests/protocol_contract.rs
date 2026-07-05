use singularity_core::ClientInfo;
use singularity_protocol::{
    AppEvent, InitializeParams, JsonRpcMessage, Method, ThreadStartParams, TurnStartParams,
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
