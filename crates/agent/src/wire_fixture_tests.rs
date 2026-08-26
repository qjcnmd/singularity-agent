//! E16 护栏：JSONL 字节级 round-trip 夹具。
//!
//! 这些夹具固定会话线的**逐字节** wire 形状（含键序、camelCase、skip-if-none
//! 行为、枚举词形）。任何对 `AgentMessage`/`SessionEntry`/`SessionMetadata`
//! 的序列化改动都必须先跑本测试：一行字节改变即意味着格式破坏。
//!
//! 键序事实：`serde_json::Map` 无 `preserve_order`，序列化时按键排序，
//! round-trip 字节稳定可断言。

use super::*;
use singularity_model::ModelStopReason;

/// 逐行断言：给定完整会话文件字节，逐条 entry 反向 round-trip 后与原始
/// 行字节完全一致（不含尾随换行）。
fn assert_lines_round_trip(file_bytes: &[u8]) {
    let text = String::from_utf8(file_bytes.to_vec()).expect("fixture is UTF-8");
    let lines = text.lines().collect::<Vec<_>>();
    assert!(!lines.is_empty(), "fixture must have a header");
    // header 形状：type/version/id/timestamp/cwd（仅借用，不 round-trip）。
    let first: serde_json::Value = serde_json::from_str(lines[0]).expect("header parses");
    assert_eq!(first["type"], "session");
    for line in lines.iter().skip(1) {
        let entry: SessionEntry = serde_json::from_str(line).expect("entry parses");
        let rewritten = serde_json::to_string(&entry).expect("entry serializes");
        assert_eq!(
            rewritten, *line,
            "JSONL entry must round-trip byte-for-byte"
        );
    }
}

/// 完整会话夹具：header + user/assistant(thinking+tool_calls+stopReason)/
/// toolResult(成功+失败) + compaction(with usage+details) + orphan-repair
/// synthetic interrupted + usage metadata + thread settings/name。
const COMPLETE_SESSION: &str = r###"{"cwd":"C:/work","id":"01914f6b-0000-7000-8000-0000000000e1","timestamp":"2026-08-20T00:00:00.000Z","type":"session","version":1}
{"id":"m-user-1","message":{"content":[{"text":"hello","type":"text"}],"role":"user"},"timestamp":"2026-08-20T00:00:01.000Z","type":"message"}
{"id":"m-assistant-1","message":{"content":[{"thinking":"reasoning trace","type":"thinking"},{"text":"analysis","type":"text"},{"args":{"command":"cargo test"},"id":"call-1","name":"bash","type":"tool_call"}],"role":"assistant","stopReason":"stop"},"timestamp":"2026-08-20T00:00:02.000Z","type":"message"}
{"id":"m-tr-1","message":{"content":[{"text":"ok","type":"text"}],"isError":false,"role":"toolResult","toolCallId":"call-1","toolName":"bash"},"timestamp":"2026-08-20T00:00:03.000Z","type":"message"}
{"id":"m-tr-2","message":{"content":[{"text":"failed","type":"text"}],"isError":true,"role":"toolResult","toolCallId":"call-2","toolName":"write"},"timestamp":"2026-08-20T00:00:04.000Z","type":"message"}
{"details":{"cut":"from_entry"},"firstKeptEntryId":"m-user-1","id":"c-1","summary":"## Goal\ncompacted history","timestamp":"2026-08-20T00:00:05.000Z","tokensBefore":1234,"type":"compaction","usage":{"cached_input_tokens":10,"input_tokens":100,"output_tokens":50,"reasoning_tokens":0,"total_tokens":150,"usage_present":true}}
{"id":"md-1","metadataType":"turn_interrupted","reason":"orphan repair","synthetic":true,"timestamp":"2026-08-20T00:00:06.000Z","turnId":"turn-1","type":"metadata"}
{"id":"md-2","metadataType":"usage","timestamp":"2026-08-20T00:00:07.000Z","turnId":"turn-1","type":"metadata","usage":{"totalTokens":42}}
{"id":"md-3","metadataType":"thread_settings","model":"opencode-go/deepseek-v4-flash#max","provider":"opencode-go","reasoning":"high","timestamp":"2026-08-20T00:00:08.000Z","type":"metadata"}
{"id":"md-4","metadataType":"thread_name","name":"typed metadata","timestamp":"2026-08-20T00:00:09.000Z","type":"metadata"}"###;

#[test]
fn jsonl_wire_round_trip_fixtures_cover_all_entry_shapes() {
    assert_lines_round_trip(COMPLETE_SESSION.as_bytes());
}

/// assistant 消息的 thinking 块不带 signature 时该键省略（skip none）。
#[test]
fn assistant_thinking_without_signature_omits_the_key() {
    let message = AgentMessage::Assistant {
        content: vec![ContentBlock::Thinking {
            thinking: "trace".to_string(),
            signature: None,
        }],
        stop_reason: Some(ModelStopReason::Stop),
        provider_reasoning_replay: None,
    };
    let serialized = serde_json::to_string(&message).expect("serialize");
    assert!(
        !serialized.contains("signature"),
        "signature omitted: {serialized}"
    );
    assert!(
        serialized.contains("\"stopReason\":\"stop\""),
        "{serialized}"
    );
    assert!(
        serialized.contains("\"role\":\"assistant\""),
        "{serialized}"
    );
}
