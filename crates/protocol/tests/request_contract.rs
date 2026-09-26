//! 终态、RPC 请求和生成客户端的协议边界。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use singularity_protocol::{
    PROTOCOL_VERSION, RpcRequest, TerminalSummary, TurnModelUsage, TurnStatus,
};

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

#[test]
fn app_rpc_rejects_incompatible_requests() {
    for invalid in [
        json!({"version": PROTOCOL_VERSION + 1, "method": "app.bootstrap", "params": {}}),
        json!({"version": PROTOCOL_VERSION, "method": "app.bootstrap", "params": {}, "extra": true}),
    ] {
        assert!(serde_json::from_value::<RpcRequest>(invalid).is_err());
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
