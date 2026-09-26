//! 协议 wire 合同 golden：逐事件 envelope/params 形状与终态 summary 形状。
//! 这些是 --json、桌面工作台与外部评估器共同消费的形状合同；方法名、键名、
//! 嵌套形状、可选字段出现/省略的任一漂移都会先在此显形。
//!
//! 失败词表（stage/cause）与 attempt 状态词形不在这里逐条重抄：它们由
//! serde snake_case 单源投影（Display 与 wire 词形结构上不可能分叉），
//! 消费路径由下方 attempt golden 覆盖。

#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use serde_json::{Value, json};
use singularity_protocol::{
    ActiveCompactionSnapshot, ActiveTurnRuntimeSnapshot, ControlChannel, ControlDisposition,
    ControlSnapshot, DiagnosticSeverity, HistoryItem, ItemRef, ProviderAttemptStatus,
    RequestObservation, SessionPhase, SessionRuntime, SessionTerminalSnapshot,
    SessionTerminalSource, Turn, TurnErrorDetail, TurnEvent, TurnEventEnvelope, TurnFailureCause,
    TurnModelUsage, TurnStatus,
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
        request_id: format!("attempt-{attempt}"),
        request_head: None,
        purpose: Default::default(),
        ordinal: 3,
        attempt,
        provider: "openai_compatible".to_string(),
        model: "test-model-a".to_string(),
        status,
        duration_ms,
        decode_ms: (duration_ms > 0).then_some(duration_ms / 2),
        total_tokens: input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input + output),
        input_tokens,
        output_tokens,
        cached_input_tokens,
        error: error.map(str::to_string),
        diagnostic_code: None,
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

/// 事件 wire golden：每行一个事件（--json），按 JSON 结构固定合同。
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
                started_at: "2026-09-08T00:00:00Z".into(),
            },
            r#"{"startedAt":"2026-09-08T00:00:00Z","turn":{"status":"running","threadId":"thread-1","turnId":"turn-1"}}"#,
        ),
        (
            "turn/userMessage",
            TurnEvent::UserMessage {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "entry-1:text:0".to_string(),
                },
                text: "task".to_string(),
            },
            r#"{"item":{"itemId":"entry-1:text:0"},"text":"task","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "turn/controlChanged",
            TurnEvent::ControlChanged {
                control: ControlSnapshot {
                    control_id: "control-1".to_string(),
                    turn_id: Some("turn-1".to_string()),
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
            "tool/execution/start",
            TurnEvent::ToolExecutionStart {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "call-1".to_string(),
                },
                tool_name: "edit".to_string(),
                args,
                started_at: "2026-09-08T00:00:00Z".into(),
            },
            r#"{"args":{"old_string":"a","path":"src/main.rs"},"item":{"itemId":"call-1"},"startedAt":"2026-09-08T00:00:00Z","threadId":"thread-1","toolName":"edit","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/update",
            TurnEvent::ToolExecutionUpdate {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "call-1".to_string(),
                },
                partial_result: "chunk".to_string(),
            },
            r#"{"item":{"itemId":"call-1"},"partialResult":"chunk","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/end",
            TurnEvent::ToolExecutionEnd {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "call-1".to_string(),
                },
                output: "done".to_string(),
                is_error: false,
                diff: None,
                duration_ms: None,
                read_source: None,
            },
            r#"{"isError":false,"item":{"itemId":"call-1"},"output":"done","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/end",
            TurnEvent::ToolExecutionEnd {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "call-2".to_string(),
                },
                output: "line".to_string(),
                is_error: false,
                diff: None,
                duration_ms: Some(3),
                read_source: Some(singularity_protocol::ReadSource {
                    start_line: 1,
                    line_count: 1,
                }),
            },
            r#"{"durationMs":3,"isError":false,"item":{"itemId":"call-2"},"output":"line","readSource":{"lineCount":1,"startLine":1},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/discarded",
            TurnEvent::ItemDiscarded {
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                item: ItemRef {
                    item_id: "item-1".into(),
                },
            },
            r#"{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/completed",
            TurnEvent::ItemCompleted {
                content: Some(HistoryItem::Message {
                    id: "item-1".into(),
                    role: "assistant".into(),
                    text: "done".into(),
                }),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
            },
            r#"{"content":{"id":"item-1","role":"assistant","text":"done","type":"message"},"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/failed",
            TurnEvent::ItemFailed {
                content: Some(HistoryItem::Thinking {
                    id: "item-1".into(),
                    text: "partial".into(),
                }),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef {
                    item_id: "item-1".to_string(),
                },
                error: "boom".to_string(),
            },
            r#"{"content":{"id":"item-1","text":"partial","type":"thinking"},"error":"boom","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
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
                retry_after_ms: None,
            },
            r#"{"observation":{"attempt":1,"cachedInputTokens":null,"durationMs":0,"error":null,"inputTokens":null,"model":"test-model-a","ordinal":3,"outputTokens":null,"provider":"openai_compatible","purpose":"generation","requestId":"attempt-1","status":"started"},"protocol":"openai_chat_completions","retryAfterMs":null,"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "provider/attempt",
            TurnEvent::ProviderAttempt {
                observation: RequestObservation {
                    diagnostic_code: Some("provider_retry_scheduled".to_string()),
                    ..request_observation(
                        2,
                        ProviderAttemptStatus::Error,
                        421,
                        Some(120),
                        Some(30),
                        Some(20),
                        Some("rate_limited"),
                    )
                },
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                protocol: "openai_responses".to_string(),
                retry_after_ms: Some(750),
            },
            r#"{"observation":{"attempt":2,"cachedInputTokens":20,"decodeMs":210,"totalTokens":150,"diagnosticCode":"provider_retry_scheduled","durationMs":421,"error":"rate_limited","inputTokens":120,"model":"test-model-a","ordinal":3,"outputTokens":30,"provider":"openai_compatible","purpose":"generation","requestId":"attempt-2","status":"error"},"protocol":"openai_responses","retryAfterMs":750,"threadId":"thread-1","turnId":"turn-1"}"#,
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
                    cause: TurnFailureCause::ProviderRateLimited,
                    message: "rate limited".to_string(),
                },
            },
            r#"{"error":{"cause":"provider_rate_limited","message":"rate limited"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
    ];
    for (method, event, jsonl_params) in &cases {
        let expected_params: Value =
            serde_json::from_str(jsonl_params).expect("jsonl golden parses");
        // JSONL 事件行就是事件自身的 tagged 序列化，此处直接按该形状比对
        // （成员顺序由 serde 决定，不构成 schema）。
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({"method": method, "params": expected_params}),
            "{method}: envelope or params drift"
        );
        let expected = json!({"method": method, "params": expected_params, "sessionRevision": 7});
        let turn_event = TurnEventEnvelope {
            event: event.clone(),
            session_revision: 7,
        };
        assert_eq!(serde_json::to_value(turn_event).unwrap(), expected);
    }
}

fn session_runtime() -> SessionRuntime {
    SessionRuntime {
        session_revision: 7,
        phase: SessionPhase::Running,
        selector: Some("openai/gpt-x#high".to_string()),
        model_context_window: Some(128_000),
        pending_controls: vec![ControlSnapshot {
            control_id: "control-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            channel: ControlChannel::FollowUp,
            sequence: 3,
            text: "run checks".to_string(),
            disposition: ControlDisposition::Pending,
        }],
        active_turn: Some(ActiveTurnRuntimeSnapshot {
            turn_id: "turn-1".to_string(),
            started_at: "2026-09-04T01:02:03.000Z".to_string(),
        }),
        active_compaction: Some(ActiveCompactionSnapshot {
            started_at: "2026-09-04T00:00:00.000Z".to_string(),
        }),
        terminal: Some(SessionTerminalSnapshot {
            source: SessionTerminalSource::Turn,
            status: TurnStatus::Failed,
            manually_stopped: false,
            message: Some("provider unavailable".to_string()),
        }),
    }
}

/// fixture 固定流信封和 RPC 响应的实际序列化形状。
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

#[test]
fn stream_payloads_and_rpc_boundaries_match_serialized_fixtures() {
    use singularity_protocol::*;
    let bootstrap = AppBootstrap {
        user_home: Some("C:/Users/test".into()),
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
        },
    };
    let events = vec![
        StreamEvent::Ready {
            payload: EmptyParams {},
        },
        StreamEvent::AppChanged {
            payload: bootstrap.clone(),
        },
        StreamEvent::SessionChanged {
            session_id: "session-1".into(),
            payload: session_runtime(),
        },
        StreamEvent::TurnEvent {
            session_id: "session-1".into(),
            payload: TurnEventEnvelope {
                event: TurnEvent::TurnStarted {
                    turn: execution_turn(TurnStatus::Running, false),
                    started_at: "2026-09-08T00:00:00Z".into(),
                },
                session_revision: 7,
            },
        },
        StreamEvent::SessionSettled {
            session_id: "session-1".into(),
            payload: session_runtime(),
        },
        StreamEvent::ResyncRequired {
            payload: EmptyParams {},
        },
    ];
    let frames: Vec<_> = events
        .into_iter()
        .enumerate()
        .map(|(revision, event)| StreamEnvelope {
            version: PROTOCOL_VERSION,
            generation: "generation-1".into(),
            revision: revision as u64,
            event,
        })
        .collect();
    fixture("stream-frames.json", &frames);
    let request = RpcRequest {
        version: PROTOCOL_VERSION,
        method: RpcMethod::AppBootstrap,
        params: json!({}),
    };
    let success = RpcResponse {
        version: PROTOCOL_VERSION,
        ok: true,
        result: Some(serde_json::to_value(bootstrap).unwrap()),
        error: None,
    };
    let failure = RpcResponse {
        version: PROTOCOL_VERSION,
        ok: false,
        result: None,
        error: Some(RpcError::new(
            RpcErrorCode::InvalidRequest,
            "invalid params",
            "retry",
        )),
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
