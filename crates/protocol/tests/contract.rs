//! 协议 wire 合同 golden：逐事件 envelope/params 形状与终态 summary 形状。
//! 这些是 --json、Web 工作台与外部评估器共同消费的字节级合同；方法名、键名、
//! 嵌套形状、可选字段出现/省略的任一漂移都会先在此显形。
//!
//! 失败词表（stage/cause）与 attempt 状态词形不在这里逐条重抄：它们由
//! serde snake_case 单源投影（Display 与 wire 词形结构上不可能分叉），
//! 其消费路径由 runtime error::tests::provider_kind_groups_map_to_stable_causes
//! 与下方 attempt golden 覆盖。

#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use serde_json::{Value, json};
use singularity_protocol::{
    ActiveCompactionSnapshot, ActiveTurnSnapshot, ControlChannel, ControlDisposition,
    ControlSnapshot, DiagnosticSeverity, ItemRef, ProviderAttemptStatus, RequestObservation,
    RpcError, RpcErrorCode, RpcMethod, RpcRequest, RpcResponse, SessionPhase, SessionSnapshot,
    SessionTerminalSnapshot, TerminalSummary, ToolResultPayload, Turn, TurnErrorDetail, TurnEvent,
    TurnFailureCause, TurnFailureStage, TurnModelUsage, TurnStatus, WORKBENCH_PROTOCOL_VERSION,
    WorkbenchTurnEvent, turn_event_envelope,
};

#[allow(clippy::too_many_arguments)]
fn request_observation(
    attempt: u32,
    status: ProviderAttemptStatus,
    duration_ms: u64,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    error: Option<&str>,
) -> RequestObservation {
    RequestObservation {
        request_id: String::new(),
        request_head: None,
        purpose: Default::default(),
        ordinal: 3,
        attempt,
        provider: "openai_compatible".to_string(),
        model: "test-model-a".to_string(),
        status,
        duration_ms,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        error: error.map(str::to_string),
        request_error: None,
    }
}

fn execution_turn(status: TurnStatus, usage: bool) -> Turn {
    Turn {
        turn_id: "turn-1".to_string(),
        thread_id: "thread-1".to_string(),
        status,
        usage: usage.then_some(TurnModelUsage {
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

/// 事件 wire golden：每行一个事件（fixture + --json），字节级合同。
/// envelope 恰为 {"method","params"}，params 的键名、嵌套形态与可选字段
/// 出现/省略的差异都会先在这张表上显形；方法词表由本表的标签集固定。
#[test]
fn turn_event_wire_goldens() {
    let args = json!({"path": "src/main.rs", "old_string": "a"});
    let cases: Vec<(&str, TurnEvent, &str)> = vec![
        (
            "turn/started",
            TurnEvent::TurnStarted {
                turn: execution_turn(TurnStatus::Running, false),
            },
            r#"{"turn":{"status":"running","threadId":"thread-1","turnId":"turn-1"}}"#,
        ),
        (
            "turn/userMessage",
            TurnEvent::UserMessage {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                entry_id: "entry-1".to_string(),
                text: "task".to_string(),
            },
            r#"{"entryId":"entry-1","text":"task","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "turn/controlChanged",
            TurnEvent::ControlChanged {
                control: ControlSnapshot {
                    control_id: "control-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    channel: ControlChannel::Steer,
                    sequence: 2,
                    text: "steer".to_string(),
                    disposition: ControlDisposition::Injected,
                },
            },
            r#"{"control":{"channel":"steer","controlId":"control-1","disposition":"injected","sequence":2,"text":"steer","turnId":"turn-1"}}"#,
        ),
        (
            "item/started",
            TurnEvent::ItemStarted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
            },
            r#"{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/agentMessage/delta",
            TurnEvent::AssistantDelta {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
                delta: "hel".to_string(),
            },
            r#"{"delta":"hel","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/agentThinking",
            TurnEvent::AssistantThinking {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
                text: "think".to_string(),
            },
            r#"{"item":{"itemId":"item-1"},"text":"think","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/start",
            TurnEvent::ToolExecutionStart {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "edit".to_string(),
                args: args.clone(),
                started_at: None,
            },
            r#"{"args":{"old_string":"a","path":"src/main.rs"},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}"#,
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
            r#"{"args":{"old_string":"a","path":"src/main.rs"},"partialResult":"chunk","threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/end",
            TurnEvent::ToolExecutionEnd {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "edit".to_string(),
                result: ToolResultPayload::new("done".to_string(), false, None),
                duration_ms: None,
            },
            r#"{"result":{"content":[{"text":"done","type":"text"}],"isError":false},"threadId":"thread-1","toolCallId":"call-1","toolName":"edit","turnId":"turn-1"}"#,
        ),
        (
            "item/completed",
            TurnEvent::ItemCompleted {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
            },
            r#"{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/failed",
            TurnEvent::ItemFailed {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
                error: "boom".to_string(),
            },
            r#"{"error":"boom","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
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
            r#"{"code":"project_instructions_truncated","message":"truncated","severity":"warning","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "provider/attempt",
            TurnEvent::ProviderAttempt {
                observation: request_observation(
                    1,
                    ProviderAttemptStatus::Started,
                    0,
                    None,
                    None,
                    None,
                    None,
                ),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                protocol: "openai_chat_completions".to_string(),
                diagnostic_code: None,
                retry_after_ms: None,
                retry_after_source: None,
            },
            r#"{"diagnosticCode":null,"observation":{"attempt":1,"cachedInputTokens":null,"durationMs":0,"error":null,"inputTokens":null,"model":"test-model-a","ordinal":3,"outputTokens":null,"provider":"openai_compatible","purpose":"generation","requestId":"","status":"started"},"protocol":"openai_chat_completions","retryAfterMs":null,"retryAfterSource":null,"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "provider/attempt",
            TurnEvent::ProviderAttempt {
                observation: request_observation(
                    2,
                    ProviderAttemptStatus::Error,
                    421,
                    Some(120),
                    Some(30),
                    Some(20),
                    Some("rate_limited"),
                ),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                protocol: "openai_responses".to_string(),
                diagnostic_code: Some("provider_retry_scheduled".to_string()),
                retry_after_ms: Some(750),
                retry_after_source: Some(singularity_protocol::RetryAfterSource::ProviderHeader),
            },
            r#"{"diagnosticCode":"provider_retry_scheduled","observation":{"attempt":2,"cachedInputTokens":20,"durationMs":421,"error":"rate_limited","inputTokens":120,"model":"test-model-a","ordinal":3,"outputTokens":30,"provider":"openai_compatible","purpose":"generation","requestId":"","status":"error"},"protocol":"openai_responses","retryAfterMs":750,"retryAfterSource":"provider_header","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "turn/completed",
            TurnEvent::TurnCompleted {
                turn: execution_turn(TurnStatus::Completed, true),
            },
            r#"{"turn":{"status":"completed","threadId":"thread-1","turnId":"turn-1","usage":{"cachedInputTokens":404,"inputTokens":101,"outputTokens":202,"reasoningTokens":505,"totalTokens":303,"usageComplete":true,"usagePresent":true}}}"#,
        ),
        (
            "turn/error",
            TurnEvent::TurnFailed {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                error: TurnErrorDetail {
                    stage: TurnFailureStage::AgentLoop,
                    cause: TurnFailureCause::ProviderRateLimited,
                    message: "rate limited".to_string(),
                },
            },
            r#"{"error":{"cause":"provider_rate_limited","message":"rate limited","stage":"agent_loop"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
    ];
    let mut serialized = Vec::new();
    for (method, event, jsonl_params) in &cases {
        let expected_params: Value =
            serde_json::from_str(jsonl_params).expect("jsonl golden parses");
        assert_eq!(
            turn_event_envelope(event),
            json!({"method": method, "params": expected_params}),
            "{method}: envelope or params drift"
        );
        let mut expected =
            json!({"method": method, "params": expected_params, "sessionRevision": 7});
        if matches!(
            event,
            TurnEvent::TurnStarted { .. } | TurnEvent::ToolExecutionStart { .. }
        ) {
            expected["params"]["startedAt"] = json!("2026-09-08T00:00:00Z");
        }
        let workbench_event = WorkbenchTurnEvent {
            event: event.clone(),
            session_revision: 7,
            started_at: "2026-09-08T00:00:00Z".to_string(),
        };
        let value = serde_json::to_value(workbench_event).unwrap();
        assert_eq!(value, expected);
        serialized.push(value);
    }
    fixture("turn-events.json", &serialized);
}

/// 终态 summary 的 wire golden：thread 已知/未知、usage 有/无、截断标志
/// 出现/省略四种组合的字节级形状。评估器逐行解析依赖此形状。
#[test]
fn terminal_summary_wire_goldens() {
    let usage = TurnModelUsage {
        input_tokens: 10,
        output_tokens: 20,
        total_tokens: 30,
        cached_input_tokens: 0,
        reasoning_tokens: 0,
        usage_present: true,
        usage_complete: true,
    };
    let cases: Vec<(&str, TerminalSummary, Value)> = vec![
        (
            "completed with thread and usage",
            TerminalSummary::new(
                Some("thread-1"),
                TurnStatus::Completed,
                Some(usage.clone()),
                false,
            ),
            json!({"summary":{"thread":{"threadId":"thread-1"},"turn":{"status":"completed","threadId":"thread-1","usage":{"cachedInputTokens":0,"inputTokens":10,"outputTokens":20,"reasoningTokens":0,"totalTokens":30,"usageComplete":true,"usagePresent":true}}}}),
        ),
        (
            "truncated completed adds the flag",
            TerminalSummary::new(Some("thread-1"), TurnStatus::Completed, Some(usage), true),
            json!({"summary":{"thread":{"threadId":"thread-1"},"turn":{"status":"completed","threadId":"thread-1","truncated":true,"usage":{"cachedInputTokens":0,"inputTokens":10,"outputTokens":20,"reasoningTokens":0,"totalTokens":30,"usageComplete":true,"usagePresent":true}}}}),
        ),
        (
            "preparation failure omits thread facts and reports null usage",
            TerminalSummary::new(None, TurnStatus::Failed, None, false),
            json!({"summary":{"turn":{"status":"failed","usage":null}}}),
        ),
        (
            "interrupted with thread, no usage",
            TerminalSummary::new(Some("thread-9"), TurnStatus::Interrupted, None, false),
            json!({"summary":{"thread":{"threadId":"thread-9"},"turn":{"status":"interrupted","threadId":"thread-9","usage":null}}}),
        ),
    ];
    for (name, summary, expected) in cases {
        assert_eq!(summary.to_line(), expected, "{name}: summary wire drift");
    }
}

fn session_snapshot() -> SessionSnapshot {
    SessionSnapshot {
        session_revision: 7,
        phase: SessionPhase::Running,
        selector: Some("openai/gpt-x#high".to_string()),
        model_context_window: Some(128_000),
        pending_controls: vec![ControlSnapshot {
            control_id: "control-1".to_string(),
            turn_id: "turn-1".to_string(),
            channel: ControlChannel::FollowUp,
            sequence: 3,
            text: "run checks".to_string(),
            disposition: ControlDisposition::Pending,
        }],
        active_turn: Some(ActiveTurnSnapshot {
            turn_id: "turn-1".to_string(),
            events: vec![singularity_protocol::WorkbenchTurnEvent {
                event: TurnEvent::TurnStarted {
                    turn: execution_turn(TurnStatus::Running, false),
                },
                session_revision: 7,
                started_at: "2026-09-04T01:02:03.000Z".into(),
            }],
            started_at: "2026-09-04T01:02:03.000Z".to_string(),
        }),
        active_compaction: Some(ActiveCompactionSnapshot {
            started_at: "2026-09-04T00:00:00.000Z".to_string(),
        }),
        terminal: Some(SessionTerminalSnapshot {
            status: TurnStatus::Failed,
            message: Some("provider unavailable".to_string()),
        }),
    }
}

#[test]
fn workbench_snapshot_and_receipt_wire_goldens() {
    assert_eq!(
        serde_json::to_value(session_snapshot()).unwrap(),
        json!({
            "sessionRevision": 7,
            "phase": "running",
            "selector": "openai/gpt-x#high",
            "modelContextWindow": 128000,
            "pendingControls": [{
                "controlId": "control-1",
                "turnId": "turn-1",
                "channel": "follow_up",
                "sequence": 3,
                "text": "run checks",
                "disposition": "pending"
            }],
            "activeTurn": {
                "turnId": "turn-1",
                "events": [{"method": "turn/started", "sessionRevision": 7,
                    "params": {"turn": execution_turn(TurnStatus::Running, false), "startedAt": "2026-09-04T01:02:03.000Z"}}],
                "startedAt": "2026-09-04T01:02:03.000Z"
            },
            "activeCompaction": {"startedAt": "2026-09-04T00:00:00.000Z"},
            "terminal": {"status": "failed", "message": "provider unavailable"}
        })
    );
}

#[test]
fn workbench_rpc_success_error_and_input_rejection_are_closed() {
    let request: RpcRequest = serde_json::from_value(json!({
        "version": WORKBENCH_PROTOCOL_VERSION,
        "requestId": "request-1",
        "method": "session.read",
        "params": {"sessionId": "session-1"}
    }))
    .unwrap();
    assert_eq!(request.method, RpcMethod::SessionRead);

    let success = RpcResponse {
        version: WORKBENCH_PROTOCOL_VERSION,
        request_id: "request-1".to_string(),
        ok: true,
        result: Some(json!({"runtime": session_snapshot()})),
        error: None,
    };
    assert_eq!(
        serde_json::to_value(success).unwrap()["ok"],
        Value::Bool(true)
    );

    let failure = RpcResponse {
        version: WORKBENCH_PROTOCOL_VERSION,
        request_id: "request-2".to_string(),
        ok: false,
        result: None,
        error: Some(
            RpcError::new(
                RpcErrorCode::SessionBusy,
                "session is running",
                "wait or stop the current turn",
            )
            .preserve("keep this"),
        ),
    };
    assert_eq!(
        serde_json::to_value(failure).unwrap(),
        json!({
            "version": 1,
            "requestId": "request-2",
            "ok": false,
            "error": {
                "code": "session_busy",
                "message": "session is running",
                "recovery": "wait or stop the current turn",
                "preservedInput": "keep this"
            }
        })
    );

    for invalid in [
        json!({"version": 2, "requestId": "x", "method": "workbench.bootstrap", "params": {}}),
        json!({"version": 1, "requestId": "x", "method": "workbench.bootstrap", "params": {}, "extra": true}),
    ] {
        assert!(serde_json::from_value::<RpcRequest>(invalid).is_err());
    }
}

/// Fixtures contain actual serialized DTOs, consumed by TypeScript without parsing Rust source.
fn fixture(name: &str, value: &impl serde::Serialize) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let actual = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("UPDATE_PROTOCOL_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(path).unwrap().replace("\r\n", "\n"),
        actual,
        "serialized fixture drift"
    );
}

#[cfg(feature = "typescript")]
#[test]
fn generated_client_matches_rust_contract() {
    assert_eq!(
        include_str!("../..//cli/web/src/protocol.generated.ts").replace("\r\n", "\n"),
        singularity_protocol::typescript::client_types(),
        "Run cargo run -p singularity_protocol --features typescript --example export_types"
    );
}

#[test]
fn stream_payloads_and_rpc_boundaries_match_serialized_fixtures() {
    use singularity_protocol::*;
    let bootstrap = WorkbenchBootstrap {
        session_phases: Default::default(),
        generation: "generation-1".into(),
        revision: 0,
        workspaces: vec![],
        sessions_by_workspace: Default::default(),
        model_catalog: RedactedModelCatalog {
            configuration: ModelConfigurationStatus::Missing,
            message: None,
            default_selector: None,
            providers: vec![],
            presets: vec![],
        },
    };
    let events = vec![
        StreamEvent::Ready {
            payload: EmptyParams {},
        },
        StreamEvent::WorkbenchChanged {
            payload: bootstrap.clone(),
        },
        StreamEvent::SessionChanged {
            session_id: "session-1".into(),
            payload: session_snapshot(),
        },
        StreamEvent::TurnEvent {
            session_id: "session-1".into(),
            payload: WorkbenchTurnEvent {
                event: TurnEvent::TurnStarted {
                    turn: execution_turn(TurnStatus::Running, false),
                },
                session_revision: 7,
                started_at: "2026-09-08T00:00:00Z".into(),
            },
        },
        StreamEvent::SessionSettled {
            session_id: "session-1".into(),
            payload: SessionSettledPayload {
                runtime: session_snapshot(),
            },
        },
        StreamEvent::ResyncRequired {
            payload: ResyncRequiredPayload {
                reason: "client_lagged".into(),
            },
        },
    ];
    let frames: Vec<_> = events
        .into_iter()
        .enumerate()
        .map(|(revision, event)| StreamEnvelope {
            version: WORKBENCH_PROTOCOL_VERSION,
            generation: "generation-1".into(),
            revision: revision as u64,
            event,
        })
        .collect();
    fixture("stream-frames.json", &frames);
    let request = RpcRequest {
        version: 1,
        request_id: "request-1".into(),
        method: RpcMethod::WorkbenchBootstrap,
        params: json!({}),
    };
    let success = RpcResponse {
        version: 1,
        request_id: "request-1".into(),
        ok: true,
        result: Some(serde_json::to_value(bootstrap).unwrap()),
        error: None,
    };
    let failure = RpcResponse {
        version: 1,
        request_id: "request-2".into(),
        ok: false,
        result: None,
        error: Some(
            RpcError::new(RpcErrorCode::InvalidRequest, "invalid params", "retry")
                .preserve("draft"),
        ),
    };
    fixture(
        "rpc.json",
        &json!({ "request": request, "success": success, "failure": failure }),
    );
    assert!(serde_json::from_value::<EmptyParams>(json!({"unexpected": true})).is_err());
    assert!(
        serde_json::from_value::<SessionTextParams>(
            json!({"workspaceId":"w","sessionId":"s","text":"hello","extra":true})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionTextParams>(json!({"workspaceId":"w","sessionId":"s"}))
            .is_err()
    );
}
