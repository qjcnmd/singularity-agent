//! 协议 wire 合同 golden：逐事件 envelope/params 形状与终态 summary 形状，由 --json、
//! 桌面工作台与外部评估器共同消费；方法名、键名、嵌套形状和可选字段出现/省略的漂移
//! 都会在此显形。失败词表（stage/cause）与 attempt 状态词形不在此逐条重抄，Display 与
//! wire 词形同出 serde snake_case，不会分叉，消费路径由下方 attempt golden 覆盖。

#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use serde_json::{Value, json};
use singularity_protocol::{
    DiagnosticSeverity, HistoryItem, ItemRef, ProviderAttemptStatus, RequestObservation, Turn,
    TurnErrorDetail, TurnEvent, TurnEventEnvelope, TurnFailureCause, TurnModelUsage, TurnStatus,
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
        purpose: singularity_protocol::RequestPurpose::Generation,
        attempt,
        provider: "openai_compatible".to_string(),
        model: "test-model-a".to_string(),
        status,
        duration_ms,
        ttft_ms: None,
        decode_ms: (duration_ms > 0).then_some(duration_ms / 2),
        total_tokens: input_tokens.zip(output_tokens).map(|(input, output)| input + output),
        input_tokens,
        output_tokens,
        cached_input_tokens,
        error: error.map(str::to_string),
        diagnostic_code: None,
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
        }),
    }
}

/// 事件 wire golden：每行一个事件（--json），按 JSON 结构固定合同。
/// envelope 恰为 {"method","params"}，params 的键名、嵌套形态与可选字段
/// 出现/省略的差异都会先在这张表上显形；方法词表由本表的标签集固定。
#[test]
fn turn_event_wire_goldens() {
    let args = json!({"path": "src/main.rs"});
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
                images: Vec::new(),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "entry-1:text:0".to_string() },
                text: "task".to_string(),
            },
            r#"{"item":{"itemId":"entry-1:text:0"},"text":"task","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        ("turn/controlChanged", TurnEvent::ControlChanged {}, r#"{}"#),
        (
            "item/agentMessage/delta",
            TurnEvent::AssistantDelta {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "item-1".to_string() },
                delta: "hel".to_string(),
            },
            r#"{"delta":"hel","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/start",
            TurnEvent::ToolExecutionStart {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "call-1".to_string() },
                tool_name: "read".to_string(),
                args,
                started_at: "2026-09-08T00:00:00Z".into(),
            },
            r#"{"args":{"path":"src/main.rs"},"item":{"itemId":"call-1"},"startedAt":"2026-09-08T00:00:00Z","threadId":"thread-1","toolName":"read","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/update",
            TurnEvent::ToolExecutionUpdate {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "call-1".to_string() },
                partial_result: "chunk".to_string(),
            },
            r#"{"item":{"itemId":"call-1"},"partialResult":"chunk","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/end",
            TurnEvent::ToolExecutionEnd {
                images: Vec::new(),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "call-1".to_string() },
                output: "done".to_string(),
                is_error: false,
                duration_ms: None,
                read_source: None,
            },
            r#"{"isError":false,"item":{"itemId":"call-1"},"output":"done","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "tool/execution/end",
            TurnEvent::ToolExecutionEnd {
                images: Vec::new(),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "call-2".to_string() },
                output: "line".to_string(),
                is_error: false,
                duration_ms: Some(3),
                read_source: Some(singularity_protocol::ReadSource { start_line: 1, line_count: 1 }),
            },
            r#"{"durationMs":3,"isError":false,"item":{"itemId":"call-2"},"output":"line","readSource":{"lineCount":1,"startLine":1},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/discarded",
            TurnEvent::ItemDiscarded {
                thread_id: "thread-1".into(),
                turn_id: "turn-1".into(),
                item: ItemRef { item_id: "item-1".into() },
            },
            r#"{"item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "item/completed",
            TurnEvent::ItemCompleted {
                content: Some(HistoryItem::Message {
                    images: Vec::new(),
                    id: "item-1".into(),
                    role: "assistant".into(),
                    text: "done".into(),
                }),
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                item: ItemRef { item_id: "item-1".to_string() },
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
                item: ItemRef { item_id: "item-1".to_string() },
                error: "boom".to_string(),
            },
            r#"{"content":{"id":"item-1","text":"partial","type":"thinking"},"error":"boom","item":{"itemId":"item-1"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "agent/diagnostic",
            TurnEvent::Diagnostic {
                thread_id: "thread-1".to_string(),
                turn_id: Some("turn-1".to_string()),
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
                turn_id: Some("turn-1".to_string()),
            },
            r#"{"observation":{"attempt":1,"cachedInputTokens":null,"durationMs":0,"error":null,"inputTokens":null,"model":"test-model-a","outputTokens":null,"provider":"openai_compatible","purpose":"generation","requestId":"attempt-1","status":"started"},"threadId":"thread-1","turnId":"turn-1"}"#,
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
                turn_id: Some("turn-1".to_string()),
            },
            r#"{"observation":{"attempt":2,"cachedInputTokens":20,"decodeMs":210,"totalTokens":150,"diagnosticCode":"provider_retry_scheduled","durationMs":421,"error":"rate_limited","inputTokens":120,"model":"test-model-a","outputTokens":30,"provider":"openai_compatible","purpose":"generation","requestId":"attempt-2","status":"error"},"threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "turn/completed",
            TurnEvent::TurnCompleted {
                turn: execution_turn(TurnStatus::Completed, true),
                finished_at: "2026-09-30T01:00:10Z".to_string(),
            },
            r#"{"finishedAt":"2026-09-30T01:00:10Z","turn":{"status":"completed","threadId":"thread-1","turnId":"turn-1","usage":{"cachedInputTokens":404,"inputTokens":101,"outputTokens":202,"reasoningTokens":505,"totalTokens":303,"usagePresent":true}}}"#,
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
                finished_at: "2026-09-30T01:00:10Z".to_string(),
            },
            r#"{"error":{"cause":"provider_rate_limited","message":"rate limited"},"finishedAt":"2026-09-30T01:00:10Z","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
    ];
    for (method, event, jsonl_params) in &cases {
        let expected_params: Value = serde_json::from_str(jsonl_params).expect("jsonl golden parses");
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
