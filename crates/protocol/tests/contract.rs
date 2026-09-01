//! 协议 wire 合同 golden：逐事件 envelope/params 形状与终态 summary 形状。
//! 这些是 `--json`、TUI 与外部评估器共同消费的字节级合同；方法名、键名、
//! 嵌套形状、可选字段出现/省略的任一漂移都会先在此显形。
//!
//! 失败词表（stage/cause）与 attempt 状态词形不在这里逐条重抄：它们由
//! serde snake_case 单源投影（Display 与 wire 词形结构上不可能分叉），
//! 其消费路径由 runtime `error::tests::provider_kind_groups_map_to_stable_causes`
//! 与下方 attempt golden 覆盖。

#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use serde_json::{Value, json};
use singularity_protocol::{
    DiagnosticSeverity, ItemRef, ProviderAttemptStatus, TerminalSummary, ToolResultPayload, Turn,
    TurnErrorDetail, TurnEvent, TurnFailureCause, TurnFailureStage, TurnModelUsage, TurnStatus,
    turn_event_envelope,
};

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

/// 事件 wire golden：每行一个事件（fixture + `--json`），字节级合同。
/// envelope 恰为 `{"method","params"}`，params 的键名、嵌套形态与可选字段
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
                text: "think".to_string(),
            },
            r#"{"text":"think","threadId":"thread-1","turnId":"turn-1"}"#,
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
                result: ToolResultPayload::text("done".to_string(), false),
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
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                attempt: 1,
                model_turn_ordinal: 3,
                provider: "openai_compatible".to_string(),
                model: "test-model-a".to_string(),
                protocol: "openai_chat_completions".to_string(),
                status: ProviderAttemptStatus::Started,
                attempt_duration_ms: None,
                error_category: None,
                diagnostic_code: None,
                retry_after_ms: None,
                retry_after_source: None,
            },
            r#"{"attempt":1,"attemptDurationMs":null,"diagnosticCode":null,"errorCategory":null,"model":"test-model-a","modelTurnOrdinal":3,"protocol":"openai_chat_completions","provider":"openai_compatible","retryAfterMs":null,"retryAfterSource":null,"status":"started","threadId":"thread-1","turnId":"turn-1"}"#,
        ),
        (
            "provider/attempt",
            TurnEvent::ProviderAttempt {
                thread_id: "thread-1".to_string(),
                turn_id: "turn-1".to_string(),
                attempt: 2,
                model_turn_ordinal: 3,
                provider: "openai_compatible".to_string(),
                model: "test-model-a".to_string(),
                protocol: "openai_responses".to_string(),
                status: ProviderAttemptStatus::Error,
                attempt_duration_ms: Some(421),
                error_category: Some("rate_limited".to_string()),
                diagnostic_code: Some("provider_retry_scheduled".to_string()),
                retry_after_ms: Some(750),
                retry_after_source: Some(singularity_protocol::RetryAfterSource::ProviderHeader),
            },
            r#"{"attempt":2,"attemptDurationMs":421,"diagnosticCode":"provider_retry_scheduled","errorCategory":"rate_limited","model":"test-model-a","modelTurnOrdinal":3,"protocol":"openai_responses","provider":"openai_compatible","retryAfterMs":750,"retryAfterSource":"provider_header","status":"error","threadId":"thread-1","turnId":"turn-1"}"#,
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
    for (method, event, jsonl_params) in &cases {
        let expected_params: Value =
            serde_json::from_str(jsonl_params).expect("jsonl golden parses");
        assert_eq!(
            turn_event_envelope(event),
            json!({"method": method, "params": expected_params}),
            "{method}: envelope or params drift"
        );
    }
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
