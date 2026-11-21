//! 属性测试（Property-based Tests）：用随机输入验证通用协议与传输不变量，不绑定具体实现细节。

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use serde_json::{Map, Value, json};
use singularity_protocol::ProviderApiProtocol;

use crate::config::selection::SelectedModel;
use crate::messages::messages_stream_request_payload;
use crate::responses::openai_responses_stream_request_payload;
use crate::transport::stream::decode_sse_chunks;
use crate::types::{
    ContentBlock, Message, ModelToolCall, ModelTurnRequest, ProviderBlockMetadata, ProviderMessageMetadata,
};

const OTHER_MSG_SIG: &str = "PRIVATE_OTHER_MESSAGES_SIG_DO_NOT_LEAK";
const OTHER_RESP_ENC: &str = "PRIVATE_OTHER_RESPONSES_ENC_DO_NOT_LEAK";

/// 按随机切分长度把字节流拆成多段 chunk（含 1 字节边界切片）。
fn split_by_sizes<'a>(bytes: &'a [u8], sizes: &[usize]) -> Vec<&'a [u8]> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut idx = 0;
    while offset < bytes.len() {
        let step = sizes.get(idx).copied().unwrap_or(1).max(1);
        let end = (offset + step).min(bytes.len());
        chunks.push(&bytes[offset..end]);
        offset = end;
        idx += 1;
    }
    chunks
}

#[derive(Debug, Clone)]
enum TurnStep {
    User(String),
    Notification(String),
    Developer(String),
    Assistant {
        text: Option<String>,
        thinking: Option<String>,
        same_provider: bool,
        tool_count: usize,
    },
}

fn non_empty_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just('a'),
            Just('Z'),
            Just('0'),
            Just(' '),
            Just('\n'),
            Just('中'),
            Just('文'),
            Just('🚀'),
        ],
        1..24,
    )
    .prop_map(|chars| {
        let s: String = chars.into_iter().collect();
        if s.trim().is_empty() { "x".to_string() } else { s }
    })
}

fn turn_step_strategy() -> impl Strategy<Value = TurnStep> {
    prop_oneof![
        non_empty_text().prop_map(TurnStep::User),
        non_empty_text().prop_map(TurnStep::Notification),
        non_empty_text().prop_map(TurnStep::Developer),
        (
            prop::option::of(non_empty_text()),
            prop::option::of(non_empty_text()),
            any::<bool>(),
            0usize..=3,
        )
            .prop_map(|(text, thinking, same_provider, tool_count)| {
                // 保证助手消息至少有一个非空可见块或工具调用，符合真实响应形态。
                let text = if text.is_none() && tool_count == 0 {
                    Some("fallback reply".to_string())
                } else {
                    text
                };
                TurnStep::Assistant {
                    text,
                    thinking,
                    same_provider,
                    tool_count,
                }
            }),
    ]
}

fn build_conversation(
    leading_devs: Vec<String>,
    steps: Vec<TurnStep>,
    protocol: ProviderApiProtocol,
    provider_name: &str,
    model_name: &str,
    endpoint: &str,
) -> Vec<Message> {
    let mut messages: Vec<Message> = leading_devs.into_iter().map(Message::developer).collect();
    let mut call_seq = 0usize;
    for step in steps {
        match step {
            TurnStep::User(t) => messages.push(Message::user(t)),
            TurnStep::Notification(t) => {
                messages.push(Message::Notification { content: vec![ContentBlock::text(t)] });
            }
            TurnStep::Developer(t) => messages.push(Message::developer(t)),
            TurnStep::Assistant {
                text,
                thinking,
                same_provider,
                tool_count,
            } => {
                let mut content = Vec::new();
                if let Some(thought) = thinking {
                    let mut meta = Map::new();
                    match protocol {
                        ProviderApiProtocol::Messages => {
                            let sig = if same_provider { "sig_same" } else { OTHER_MSG_SIG };
                            meta.insert("signature".into(), json!(sig));
                        }
                        ProviderApiProtocol::Responses => {
                            let enc = if same_provider { "enc_same" } else { OTHER_RESP_ENC };
                            meta.insert("id".into(), json!("rs_prop_1"));
                            meta.insert("encrypted_content".into(), json!(enc));
                        }
                    }
                    content.push(ContentBlock::Thinking {
                        thinking: thought,
                        metadata: Some(ProviderBlockMetadata(meta)),
                    });
                }
                if let Some(body) = text {
                    content.push(ContentBlock::text(body));
                }
                let mut call_ids = Vec::new();
                for _ in 0..tool_count {
                    call_seq += 1;
                    let id = format!("call_{call_seq}");
                    content.push(ContentBlock::ToolCall(ModelToolCall {
                        tool_call_id: id.clone(),
                        tool_name: "demo_tool".to_string(),
                        arguments: json!({ "seq": call_seq }),
                        metadata: None,
                    }));
                    call_ids.push(id);
                }
                let provider_metadata = Some(match (protocol, same_provider) {
                    (ProviderApiProtocol::Messages, true) => ProviderMessageMetadata::Messages {
                        provider_name: provider_name.into(),
                        model_name: model_name.into(),
                        endpoint: endpoint.into(),
                    },
                    (ProviderApiProtocol::Messages, false) => ProviderMessageMetadata::Messages {
                        provider_name: "other-provider".into(),
                        model_name: "other-model".into(),
                        endpoint: "https://other.example/v1/messages".into(),
                    },
                    (ProviderApiProtocol::Responses, true) => ProviderMessageMetadata::Responses {
                        provider_name: provider_name.into(),
                        model_name: model_name.into(),
                        endpoint: endpoint.into(),
                    },
                    (ProviderApiProtocol::Responses, false) => ProviderMessageMetadata::Responses {
                        provider_name: "other-provider".into(),
                        model_name: "other-model".into(),
                        endpoint: "https://other.example/v1/responses".into(),
                    },
                });
                messages.push(Message::Assistant { content, provider_metadata });
                for id in call_ids {
                    messages.push(Message::ToolResult {
                        content: vec![ContentBlock::text(format!("result for {id}"))],
                        tool_call_id: id,
                        is_error: false,
                    });
                }
            }
        }
    }
    // 保证至少有一条非 leading Developer 消息，与真实请求前提一致。
    messages.push(Message::user("final user prompt"));
    messages
}

fn sse_line_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just('a'),
            Just('Z'),
            Just('0'),
            Just('{'),
            Just('}'),
            Just('"'),
            Just(':'),
            Just(','),
            Just(' '),
            Just('中'),
            Just('🚀'),
        ],
        1..20,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// SSE 分帧不变量：任意合法事件流（含多行 data、UTF-8、BOM、注释、CRLF/CR/LF，
    /// 以及最后一个事件有无结尾空行直接 EOF）按任意字节粒度切分 chunk 后，
    /// 解出的事件序列必须与原始序列完全一致。
    #[test]
    fn sse_frame_decoder_chunk_split_invariance(
        with_bom in any::<bool>(),
        terminate_last_event in any::<bool>(),
        eol_kind in 0u8..3,
        events in prop::collection::vec(
            prop::collection::vec(sse_line_strategy(), 1..=3),
            1..=8
        ),
        chunk_sizes in prop::collection::vec(1usize..=11, 1..=64),
    ) {
        let eol: &[u8] = match eol_kind {
            0 => b"\n",
            1 => b"\r\n",
            _ => b"\r",
        };
        let mut wire = Vec::new();
        if with_bom {
            wire.extend_from_slice(b"\xef\xbb\xbf");
        }
        let mut expected = Vec::new();
        for (idx, lines) in events.iter().enumerate() {
            // 穿插注释与非 data 字段，验证分帧器正确忽略。
            wire.extend_from_slice(b": keepalive");
            wire.extend_from_slice(eol);
            wire.extend_from_slice(b"event: message");
            wire.extend_from_slice(eol);
            for line in lines {
                wire.extend_from_slice(b"data: ");
                wire.extend_from_slice(line.as_bytes());
                wire.extend_from_slice(eol);
            }
            if idx + 1 < events.len() || terminate_last_event {
                wire.extend_from_slice(eol);
            }
            expected.push(lines.join("\n").into_bytes());
        }

        let chunks = split_by_sizes(&wire, &chunk_sizes);
        let (decoded, has_unterminated) = decode_sse_chunks(&chunks);
        prop_assert_eq!(decoded, expected);
        prop_assert!(!has_unterminated);
    }

    /// 任意随机字节流与任意 chunk 边界切分绝不能使 SSE 分帧器 panic。
    #[test]
    fn sse_frame_decoder_never_panics_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        chunk_sizes in prop::collection::vec(1usize..=17, 1..=32),
    ) {
        let chunks = split_by_sizes(&bytes, &chunk_sizes);
        let _ = decode_sse_chunks(&chunks);
    }

    /// Messages 协议编码不变量：任意对话历史经编码后，角色严格交替、无空块、
    /// tool_use/tool_result 严格 1:1 配对，且绝不泄露其他提供方的私有思考签名。
    #[test]
    fn messages_encoding_preserves_protocol_invariants(
        leading_devs in prop::collection::vec(non_empty_text(), 0..=3),
        steps in prop::collection::vec(turn_step_strategy(), 0..=10),
    ) {
        let provider_name = "primary";
        let model_name = "demo";
        let endpoint = "https://primary.example/v1/messages";
        let selection = SelectedModel {
            model_name: model_name.into(),
            protocol: ProviderApiProtocol::Messages,
            max_context_tokens: 64_000,
            max_output_tokens: 4096,
            reasoning_variant: Some("high".into()),
        };
        let messages = build_conversation(
            leading_devs,
            steps,
            ProviderApiProtocol::Messages,
            provider_name,
            model_name,
            endpoint,
        );
        let req = ModelTurnRequest {
            messages,
            images: HashMap::new(),
            tools: Vec::new(),
            max_output_tokens: 1024,
        };
        let payload = messages_stream_request_payload(&req, &selection, provider_name, endpoint)
            .expect("messages encoding succeeds");
        let serialized = payload.to_string();
        prop_assert!(!serialized.contains(OTHER_MSG_SIG), "leaked foreign Messages signature");

        let encoded = payload.get("messages").and_then(Value::as_array).expect("messages array");
        prop_assert!(!encoded.is_empty());

        let mut prev_role: Option<&str> = None;
        let mut expected_tools: HashSet<String> = HashSet::new();
        for msg in encoded {
            let role = msg.get("role").and_then(Value::as_str).expect("role");
            prop_assert!(matches!(role, "user" | "assistant"), "unsupported Messages role: {role}");
            if let Some(prev) = prev_role {
                prop_assert_ne!(prev, role, "roles must strictly alternate");
            }
            prev_role = Some(role);
            let blocks = msg.get("content").and_then(Value::as_array).expect("content array");
            prop_assert!(!blocks.is_empty());
            if role == "user" {
                let mut seen = HashSet::new();
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = b.get("tool_use_id").and_then(Value::as_str).expect("tool_use_id");
                        prop_assert!(expected_tools.contains(id), "orphaned tool_result");
                        prop_assert!(seen.insert(id.to_string()), "duplicate tool_result id: {id}");
                    }
                }
                prop_assert_eq!(seen, expected_tools.clone(), "tool_use/tool_result mismatch");
                expected_tools.clear();
            } else {
                prop_assert!(expected_tools.is_empty());
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let id = b.get("id").and_then(Value::as_str).expect("tool_use id");
                        prop_assert!(expected_tools.insert(id.to_string()), "duplicate tool_use id: {id}");
                    }
                }
            }
        }
        prop_assert!(expected_tools.is_empty(), "dangling tool_use at end");
    }

    /// Responses 协议编码不变量：任意对话历史经编码后，input 非空、
    /// function_call/function_call_output 严格配对，且绝不泄露其他提供方的加密推理内容。
    #[test]
    fn responses_encoding_preserves_protocol_invariants(
        leading_devs in prop::collection::vec(non_empty_text(), 0..=3),
        steps in prop::collection::vec(turn_step_strategy(), 0..=10),
    ) {
        let provider_name = "primary";
        let model_name = "demo";
        let endpoint = "https://primary.example/v1/responses";
        let selection = SelectedModel {
            model_name: model_name.into(),
            protocol: ProviderApiProtocol::Responses,
            max_context_tokens: 64_000,
            max_output_tokens: 4096,
            reasoning_variant: Some("high".into()),
        };
        let messages = build_conversation(
            leading_devs,
            steps,
            ProviderApiProtocol::Responses,
            provider_name,
            model_name,
            endpoint,
        );
        let req = ModelTurnRequest {
            messages,
            images: HashMap::new(),
            tools: Vec::new(),
            max_output_tokens: 1024,
        };
        let payload = openai_responses_stream_request_payload(&req, &selection, provider_name, endpoint)
            .expect("responses encoding succeeds");
        let serialized = payload.to_string();
        prop_assert!(!serialized.contains(OTHER_RESP_ENC), "leaked foreign Responses encrypted_content");

        let input = payload.get("input").and_then(Value::as_array).expect("input array");
        prop_assert!(!input.is_empty());
        let mut calls = HashSet::new();
        let mut outputs = HashSet::new();
        for item in input {
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    let id = item.get("call_id").and_then(Value::as_str).expect("call_id");
                    prop_assert!(calls.insert(id.to_string()), "duplicate function_call id: {id}");
                }
                Some("function_call_output") => {
                    let id = item.get("call_id").and_then(Value::as_str).expect("call_id");
                    prop_assert!(calls.contains(id), "orphaned function_call_output");
                    prop_assert!(outputs.insert(id.to_string()), "duplicate function_call_output id: {id}");
                }
                _ => {}
            }
        }
        prop_assert_eq!(calls, outputs, "function_call/output mismatch");
    }
}
