//! 终态、RPC 请求和生成客户端的协议边界。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use singularity_protocol::{TerminalSummary, TurnModelUsage, TurnStatus};

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
            json!({"summary":{"turn":{"status":"completed","threadId":"thread-1","usage":{"cachedInputTokens":0,"inputTokens":10,"outputTokens":20,"reasoningTokens":0,"totalTokens":30,"usagePresent":true}}}}),
        ),
        (
            "truncated completed adds the flag",
            TerminalSummary::new(Some("thread-1"), TurnStatus::Completed, Some(usage), true),
            json!({"summary":{"turn":{"status":"completed","threadId":"thread-1","truncated":true,"usage":{"cachedInputTokens":0,"inputTokens":10,"outputTokens":20,"reasoningTokens":0,"totalTokens":30,"usagePresent":true}}}}),
        ),
        (
            "preparation failure omits thread facts and reports null usage",
            TerminalSummary::new(None, TurnStatus::Failed, None, false),
            json!({"summary":{"turn":{"status":"failed","usage":null}}}),
        ),
        (
            "interrupted with thread, no usage",
            TerminalSummary::new(Some("thread-9"), TurnStatus::Interrupted, None, false),
            json!({"summary":{"turn":{"status":"interrupted","threadId":"thread-9","usage":null}}}),
        ),
    ];
    for (name, summary, expected) in cases {
        assert_eq!(summary.to_line(), expected, "{name}: summary wire drift");
    }
}

#[cfg(feature = "typescript")]
#[test]
fn generated_client_matches_rust_contract() {
    assert_eq!(
        include_str!("../../../apps/desktop/src/protocol.generated.ts").replace("\r\n", "\n"),
        singularity_protocol::typescript::client_types(),
        "Run cargo run -p singularity_protocol --features typescript --example export_types"
    );
}
