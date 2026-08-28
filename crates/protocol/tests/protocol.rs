//! 收缩后协议合同测试：stdio JSON-RPC registry、事件两面 wire golden 与稳定状态文本。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use serde_json::json;
use singularity_protocol::{
    ClientInfo, DiagnosticSeverity, EmptyParams, ErrorCode, ExecutionThread, ExecutionTurn,
    ExecutionTurnUsage, InitializeParams, JsonRpcId, JsonRpcInbound, JsonRpcMessage, Method,
    MethodKind, ProviderAttemptStatus, ThreadReadParams, ThreadStartParams, ThreadStatus,
    TurnErrorDetail, TurnEvent, TurnFailureCause, TurnFailureStage, TurnInjectionParams,
    TurnStartParams, TurnStatus, parse_json_rpc_payload, turn_event_jsonl_params,
    turn_event_notification,
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
    assert!(serde_json::from_value::<TurnInjectionParams>(steer).is_ok());

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
    assert!(serde_json::from_value::<ThreadReadParams>(json!({"sessionId":"x"})).is_ok());
    assert!(
        serde_json::from_value::<ThreadReadParams>(json!({"sessionId":"x","unknown":true}))
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
fn thread_settings_reasoning_wire_distinguishes_missing_string_and_null() {
    for (wire, expected_reasoning) in [
        (json!({"threadId": "thread"}), None),
        (
            json!({"threadId": "thread", "reasoning": "high"}),
            Some(json!("high")),
        ),
        (
            json!({"threadId": "thread", "reasoning": null}),
            Some(serde_json::Value::Null),
        ),
    ] {
        let params: singularity_protocol::ThreadSettingsParams =
            serde_json::from_value(wire).expect("settings params");
        let encoded = serde_json::to_value(params).expect("settings params serialize");
        assert_eq!(encoded.get("reasoning").cloned(), expected_reasoning);
    }
}

fn execution_turn(status: TurnStatus, usage: bool) -> ExecutionTurn {
    ExecutionTurn {
        turn_id: "turn-1".to_string(),
        thread_id: "thread-1".to_string(),
        status,
        usage: usage.then_some(ExecutionTurnUsage {
            input_tokens: 101,
            output_tokens: 202,
            total_tokens: 303,
            cached_input_tokens: 404,
            reasoning_tokens: 505,
            usage_present: true,
            usage_complete: true,
        }),
    }
}

/// 事件两面 wire golden：每行一个事件（fixture + JSON-RPC 通知全文 +
/// `--json` params），字面量取自折叠前现行实现的抓取（`edd9f914` 后的
/// wire 形状，字节级合同）。方法名、键名、嵌套形态、可选字段
/// 出现/省略的差异都会先在这张表上显形。
#[test]
fn turn_event_wire_goldens() {
    let args = json!({"path": "src/main.rs", "old_string": "a"});
    let cases: Vec<(&str, TurnEvent, &str, &str)> = vec![
        (
            "turn/started",
            TurnEvent::TurnStarted {
                turn: execution_turn(TurnStatus::Running, false),
            },
            r#"{"jsonrpc":"2.0","method":"turn/started","params":{"turn":{"status":"running","threadId":"thread-1","turnId":"turn-1"}}}"#,
            r#"{"turn":{"status":"running","threadId":"thread-1","turnId":"turn-1"}}"#,
        ),
        (
            "item/started",
            TurnEvent::ItemStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"item/started","params":{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"item_id":"item-1","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "item/agentMessage/delta",
            TurnEvent::AssistantDelta {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                delta: "hel".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"item/agentMessage/delta","params":{"delta":"hel","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"delta":"hel","item_id":"item-1","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "item/agentThinking",
            TurnEvent::AssistantThinking {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                text: "think".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"item/agentThinking","params":{"text":"think","threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"text":"think","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "tool/execution/start",
            TurnEvent::ToolExecutionStart {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "edit".to_string(),
                args: args.clone(),
            },
            r#"{"jsonrpc":"2.0","method":"tool/execution/start","params":{"args":{"old_string":"a","path":"src/main.rs"},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}}"#,
            r#"{"args":{"old_string":"a","path":"src/main.rs"},"thread_id":"thread-1","tool_call_id":"call-1","tool_name":"edit","turn_id":"turn-1"}"#,
        ),
        (
            "tool/execution/update",
            TurnEvent::ToolExecutionUpdate {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "edit".to_string(),
                args,
                partial_result: "chunk".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"tool/execution/update","params":{"args":{"old_string":"a","path":"src/main.rs"},"partialResult":"chunk","threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}}"#,
            r#"{"args":{"old_string":"a","path":"src/main.rs"},"partial_result":"chunk","thread_id":"thread-1","tool_call_id":"call-1","tool_name":"edit","turn_id":"turn-1"}"#,
        ),
        (
            "tool/execution/end",
            TurnEvent::ToolExecutionEnd {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "edit".to_string(),
                result: "done".to_string(),
                is_error: false,
            },
            r#"{"jsonrpc":"2.0","method":"tool/execution/end","params":{"result":{"content":[{"text":"done","type":"text"}],"isError":false},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}}"#,
            r#"{"is_error":false,"result":"done","thread_id":"thread-1","tool_call_id":"call-1","tool_name":"edit","turn_id":"turn-1"}"#,
        ),
        (
            "item/completed",
            TurnEvent::ItemCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"item/completed","params":{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"item_id":"item-1","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "item/failed",
            TurnEvent::ItemFailed {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item_id: "item-1".to_string(),
                error: "boom".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"item/failed","params":{"error":"boom","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"error":"boom","item_id":"item-1","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "agent/diagnostic",
            TurnEvent::Diagnostic {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                severity: DiagnosticSeverity::Warning,
                code: "project_instructions_truncated".to_string(),
                message: "truncated".to_string(),
            },
            r#"{"jsonrpc":"2.0","method":"agent/diagnostic","params":{"code":"project_instructions_truncated","message":"truncated","severity":"warning","threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"code":"project_instructions_truncated","message":"truncated","severity":"warning","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "provider/attempt",
            TurnEvent::ProviderAttempt {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                model_turn_ordinal: 3,
                provider: "opencode-go".to_string(),
                model: "deepseek-v4-flash".to_string(),
                protocol: "openai_chat_completions".to_string(),
                status: ProviderAttemptStatus::Started,
                attempt_duration_ms: None,
                error_category: None,
                diagnostic_code: None,
            },
            r#"{"jsonrpc":"2.0","method":"provider/attempt","params":{"attemptDurationMs":null,"diagnosticCode":null,"errorCategory":null,"model":"deepseek-v4-flash","modelTurnOrdinal":3,"protocol":"openai_chat_completions","provider":"opencode-go","status":"started","threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"model":"deepseek-v4-flash","model_turn_ordinal":3,"protocol":"openai_chat_completions","provider":"opencode-go","status":"started","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "provider/attempt",
            TurnEvent::ProviderAttempt {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                model_turn_ordinal: 3,
                provider: "opencode-go".to_string(),
                model: "deepseek-v4-flash".to_string(),
                protocol: "openai_responses".to_string(),
                status: ProviderAttemptStatus::Error,
                attempt_duration_ms: Some(421),
                error_category: Some("rate_limited".to_string()),
                diagnostic_code: Some("provider_retry_scheduled".to_string()),
            },
            r#"{"jsonrpc":"2.0","method":"provider/attempt","params":{"attemptDurationMs":421,"diagnosticCode":"provider_retry_scheduled","errorCategory":"rate_limited","model":"deepseek-v4-flash","modelTurnOrdinal":3,"protocol":"openai_responses","provider":"opencode-go","status":"error","threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"attempt_duration_ms":421,"diagnostic_code":"provider_retry_scheduled","error_category":"rate_limited","model":"deepseek-v4-flash","model_turn_ordinal":3,"protocol":"openai_responses","provider":"opencode-go","status":"error","thread_id":"thread-1","turn_id":"turn-1"}"#,
        ),
        (
            "turn/completed",
            TurnEvent::TurnCompleted {
                turn: execution_turn(TurnStatus::Completed, true),
            },
            r#"{"jsonrpc":"2.0","method":"turn/completed","params":{"turn":{"modelUsage":{"cachedInputTokens":404,"inputTokens":101,"outputTokens":202,"reasoningTokens":505,"totalTokens":303,"usageComplete":true,"usagePresent":true},"status":"completed","threadId":"thread-1","turnId":"turn-1"}}}"#,
            r#"{"turn":{"status":"completed","threadId":"thread-1","turnId":"turn-1","usage":{"cachedInputTokens":404,"inputTokens":101,"outputTokens":202,"reasoningTokens":505,"totalTokens":303,"usageComplete":true,"usagePresent":true}}}"#,
        ),
        (
            "turn/error",
            TurnEvent::TurnFailed {
                turn: execution_turn(TurnStatus::Failed, false),
                error: TurnErrorDetail {
                    stage: TurnFailureStage::AgentLoop,
                    cause: TurnFailureCause::ProviderRateLimited,
                    message: "rate limited".to_string(),
                },
            },
            r#"{"jsonrpc":"2.0","method":"turn/error","params":{"error":{"cause":"provider_rate_limited","message":"rate limited","stage":"agent_loop"},"threadId":"thread-1","turnId":"turn-1"}}"#,
            r#"{"error":{"cause":"provider_rate_limited","message":"rate limited","stage":"agent_loop"},"turn":{"status":"failed","threadId":"thread-1","turnId":"turn-1"}}"#,
        ),
        (
            "thread/settingsApplied",
            TurnEvent::ThreadSettingsApplied {
                thread: ExecutionThread {
                    thread_id: "thread-1".to_string(),
                    cwd: "C:\\work".to_string(),
                    model: Some("opencode-go/deepseek-v4-flash#max".to_string()),
                    last_turn_status: Some(ThreadStatus::Completed),
                },
            },
            r#"{"jsonrpc":"2.0","method":"thread/settingsApplied","params":{"thread":{"cwd":"C:\\work","lastTurnStatus":"completed","model":"opencode-go/deepseek-v4-flash#max","threadId":"thread-1"}}}"#,
            r#"{"thread":{"cwd":"C:\\work","lastTurnStatus":"completed","model":"opencode-go/deepseek-v4-flash#max","threadId":"thread-1"}}"#,
        ),
    ];
    for (method, event, notification, jsonl_params) in cases {
        assert_eq!(event.method(), method, "method drift");
        let expected_notification: serde_json::Value =
            serde_json::from_str(notification).expect("notification golden parses");
        assert_eq!(
            turn_event_notification(&event).to_wire_value(),
            expected_notification,
            "{method}: json-rpc notification drift"
        );
        let expected_jsonl: serde_json::Value =
            serde_json::from_str(jsonl_params).expect("jsonl golden parses");
        assert_eq!(
            turn_event_jsonl_params(&event),
            expected_jsonl,
            "{method}: --json params drift"
        );
    }
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
            id: Some(singularity_protocol::JsonRpcId(42))
        }
    );

    let array = parse_json_rpc_payload(r#"[]"#).expect("empty array");
    assert_eq!(array, JsonRpcInbound::Invalid { id: None });
}

#[test]
fn json_rpc_id_is_numeric_only_and_rejects_large_u64_and_string_ids() {
    // 数字 id 双向往返。
    let request = JsonRpcMessage::request(Method::ThreadList, JsonRpcId(7), json!({}))
        .expect("numeric id request");
    assert_eq!(request.id(), Some(&JsonRpcId(7)));
    assert_eq!(request.to_wire_value()["id"], 7);

    // 大 u64（超出 i64 范围）id 无法形成请求：解析为 Invalid 且不可恢复 id，
    // 错误响应关联回 null。
    let huge = parse_json_rpc_payload(
        r#"{"jsonrpc":"2.0","method":"thread/list","id":18446744073709551615,"params":{}}"#,
    )
    .expect("huge u64 id frame");
    assert_eq!(huge, JsonRpcInbound::Invalid { id: None });

    // 字符串 id 同样不被接受，无法关联回 null。
    let string_id = parse_json_rpc_payload(
        r#"{"jsonrpc":"2.0","method":"thread/list","id":"request-1","params":{}}"#,
    )
    .expect("string id frame");
    assert_eq!(string_id, JsonRpcInbound::Invalid { id: None });

    // 无法关联的 error response 在 wire 上输出 id: null。
    let error = JsonRpcMessage::parse_error();
    assert_eq!(error.to_wire_value()["id"], serde_json::Value::Null);
}

#[test]
fn thread_status_projects_last_turn_metadata_not_lifecycle() {
    let thread = singularity_protocol::Thread {
        thread_id: "session-1".to_string(),
        model: None,
        cwd: "/tmp/work".to_string(),
        last_turn_status: Some(ThreadStatus::Completed),
    };
    let wire = serde_json::to_value(&thread).expect("thread wire");
    assert_eq!(wire["threadId"], "session-1");
    assert_eq!(wire["lastTurnStatus"], "completed");
    // 尚无 turn：wire 上为 null，而不是伪装成 active。
    let no_turn = singularity_protocol::Thread {
        thread_id: "session-2".to_string(),
        model: None,
        cwd: "/tmp/work".to_string(),
        last_turn_status: None,
    };
    let wire = serde_json::to_value(&no_turn).expect("thread wire");
    assert_eq!(wire["lastTurnStatus"], serde_json::Value::Null);
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
    assert!(serde_json::from_value::<InitializeParams>(params).is_ok());
    assert_eq!(ErrorCode::not_initialized().message(), "Not initialized");
}
