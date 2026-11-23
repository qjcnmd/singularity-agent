use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use protocol::ProviderApiProtocol;
use serde_json::{Map, Value, json};

use crate::config::selection::{ProviderConfig, SelectedModel};
use crate::messages::{messages_stream_request_payload, stream::decode_messages_events_for_test};
use crate::responses::{openai_responses_stream_request_payload, parse_openai_responses_response};
use crate::transport::stream::decode_sse_chunks;
use crate::types::{
    ContentBlock, Message, ModelToolCall, ModelTurnRequest, ProviderBlockMetadata, ProviderMessageMetadata,
};

const OTHER_MSG_SIG: &str = "PRIVATE_OTHER_MESSAGES_SIG_DO_NOT_LEAK";
const OTHER_RESP_ENC: &str = "PRIVATE_OTHER_RESPONSES_ENC_DO_NOT_LEAK";

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
                let text = if text.is_none() && thinking.is_none() && tool_count == 0 {
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
                let provider_metadata = Some(if same_provider {
                    ProviderMessageMetadata {
                        protocol,
                        provider_name: provider_name.into(),
                        model_name: model_name.into(),
                        endpoint: endpoint.into(),
                        prefix_digest: None,
                        reasoning: None,
                    }
                } else {
                    let suffix = match protocol {
                        ProviderApiProtocol::Messages => "/v1/messages",
                        ProviderApiProtocol::Responses => "/v1/responses",
                    };
                    ProviderMessageMetadata {
                        protocol,
                        provider_name: "other-provider".into(),
                        model_name: "other-model".into(),
                        endpoint: format!("https://other.example{suffix}"),
                        prefix_digest: None,
                        reasoning: None,
                    }
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
    messages.push(Message::user("final user prompt"));
    messages
}

fn count_cache_control(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            usize::from(map.contains_key("cache_control"))
                + map.values().map(count_cache_control).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

fn trailing_tool_turn_stripped_in_steps(steps: &[TurnStep]) -> bool {
    let mut iter = steps.iter().rev().skip_while(|s| match s {
        TurnStep::Assistant { text, tool_count, .. } => text.is_none() && *tool_count == 0,
        _ => true,
    });
    let Some(TurnStep::Assistant { thinking, same_provider, tool_count, .. }) = iter.next() else {
        return false;
    };
    *tool_count > 0
        && (thinking.is_some() && *same_provider
            || iter.take_while(|s| matches!(s, TurnStep::Assistant { tool_count: 0, .. })).any(|s| {
                matches!(
                    s,
                    TurnStep::Assistant {
                        thinking: Some(_),
                        same_provider: true,
                        ..
                    }
                )
            }))
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

    #[test]
    fn sse_frame_decoder_never_panics_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
        chunk_sizes in prop::collection::vec(1usize..=17, 1..=32),
    ) {
        let chunks = split_by_sizes(&bytes, &chunk_sizes);
        let _ = decode_sse_chunks(&chunks);
    }

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
        let trailing_stripped = trailing_tool_turn_stripped_in_steps(&steps);
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
            prompt_cache_key: None,
        };
        let payload = messages_stream_request_payload(&req, &selection, provider_name, endpoint)
            .expect("messages encoding succeeds")
            .payload;
        let serialized = payload.to_string();
        prop_assert!(!serialized.contains(OTHER_MSG_SIG), "leaked foreign Messages signature");
        prop_assert!(count_cache_control(&payload) <= 4, "explicit cache_control breakpoints exceed 4");

        let encoded = payload.get("messages").and_then(Value::as_array).expect("messages array");
        prop_assert!(!encoded.is_empty());
        prop_assert!(encoded[..encoded.len() - 1].iter().all(|m| count_cache_control(m) == 0));
        prop_assert_eq!(
            encoded.last().and_then(|m| m.get("content")).and_then(Value::as_array).and_then(|b| b.last()).and_then(|b| b.pointer("/cache_control/type")).and_then(Value::as_str),
            Some("ephemeral")
        );
        if trailing_stripped {
            prop_assert!(payload.get("thinking").is_none() && payload.get("output_config").is_none());
        } else {
            prop_assert_eq!(payload.pointer("/thinking/type").and_then(Value::as_str), Some("adaptive"));
        }

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

    #[test]
    fn responses_encoding_preserves_protocol_invariants(
        leading_devs in prop::collection::vec(non_empty_text(), 0..=3),
        steps in prop::collection::vec(turn_step_strategy(), 0..=10),
        prompt_cache_key in prop::option::of(prop_oneof![
            Just(String::new()),
            Just("   ".to_string()),
            non_empty_text(),
        ]),
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
            prompt_cache_key,
        };
        let payload = openai_responses_stream_request_payload(&req, &selection, provider_name, endpoint)
            .expect("responses encoding succeeds");
        let serialized = payload.to_string();
        prop_assert!(!serialized.contains(OTHER_RESP_ENC), "leaked foreign Responses encrypted_content");
        let expected_cache_key = req.prompt_cache_key.as_deref().filter(|key| !key.trim().is_empty());
        prop_assert_eq!(payload.get("prompt_cache_key").and_then(Value::as_str), expected_cache_key);

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

#[test]
fn messages_context_window_exceeded_and_responses_refusal_contract() {
    let config = ProviderConfig {
        provider_name: "p".into(),
        base_url: "https://example.invalid".into(),
        api_key: "k".into(),
    };
    let messages_model = SelectedModel {
        model_name: "claude-test".into(),
        protocol: ProviderApiProtocol::Messages,
        max_context_tokens: 64_000,
        max_output_tokens: 4096,
        reasoning_variant: Some("high".into()),
    };
    let stream_events = vec![
        json!({ "type": "message_start", "message": { "usage": { "input_tokens": 100 } } }),
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "partial summary" } }),
        json!({ "type": "content_block_stop", "index": 0 }),
        json!({ "type": "message_delta", "delta": { "stop_reason": "model_context_window_exceeded" }, "usage": { "output_tokens": 20 } }),
        json!({ "type": "message_stop" }),
    ];
    let resp = decode_messages_events_for_test(
        &stream_events,
        &config,
        &messages_model,
        "https://example.invalid/v1/messages",
    );
    assert!(resp.error().is_some_and(crate::ProviderError::is_context_overflow));
    assert!(!resp.is_length_truncated());
    assert!(!resp.is_refusal());

    let responses_payload = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "refusal", "refusal": "cannot summarize" }]
        }],
        "usage": { "input_tokens": 100, "output_tokens": 10, "total_tokens": 110 }
    });
    let refusal_resp = parse_openai_responses_response(
        &config,
        responses_payload,
        "gpt-test",
        "https://example.invalid/v1/responses",
        None,
    )
    .expect("responses refusal parses cleanly");
    assert!(refusal_resp.is_refusal());
    assert_eq!(refusal_resp.assistant_message.content_text(), "cannot summarize");

    let endpoint = "https://example.invalid/v1/messages";
    let encode = |dev: &str, user: &str, tail: Vec<Message>| {
        let mut messages = vec![Message::developer(dev), Message::user(user)];
        messages.extend(tail);
        let req = ModelTurnRequest {
            messages,
            images: HashMap::new(),
            tools: Vec::new(),
            max_output_tokens: 1024,
            prompt_cache_key: None,
        };
        messages_stream_request_payload(&req, &messages_model, "p", endpoint).expect("messages encode")
    };
    fn ptr<'a>(p: &'a Value, path: &str) -> Option<&'a str> {
        p.pointer(path).and_then(Value::as_str)
    }
    let turn1 = encode("sys v1", "first prompt", vec![]);
    assert_eq!(ptr(&turn1.payload, "/messages/0/content/0/cache_control/type"), Some("ephemeral"));
    let turn1_digest = turn1.prefix_digest;
    let mk_assistant = |content| Message::Assistant {
        content,
        provider_metadata: Some(ProviderMessageMetadata {
            protocol: ProviderApiProtocol::Messages,
            provider_name: "p".into(),
            model_name: "claude-test".into(),
            endpoint: endpoint.into(),
            prefix_digest: Some(turn1_digest.clone()),
            reasoning: None,
        }),
    };
    let signed_block = ContentBlock::Thinking {
        thinking: "signed thought".into(),
        metadata: Some(ProviderBlockMetadata(Map::from_iter([(
            "signature".into(),
            json!("SIG_BOUND_TO_V1"),
        )]))),
    };
    let assistant_v1 = mk_assistant(vec![
        signed_block.clone(),
        ContentBlock::Thinking {
            thinking: String::new(),
            metadata: Some(ProviderBlockMetadata(Map::from_iter([
                ("type".into(), json!("redacted_thinking")),
                ("data".into(), json!("REDACTED_PAYLOAD")),
            ]))),
        },
        ContentBlock::Thinking {
            thinking: "UNSIGNED_THOUGHT_KEPT".into(),
            metadata: None,
        },
        ContentBlock::text("ASSISTANT_BODY_KEPT"),
    ]);

    let same_payload =
        encode("sys v1", "first prompt", vec![assistant_v1.clone(), Message::user("follow up")]).payload;
    let same_str = same_payload.to_string();
    assert!(same_str.contains("SIG_BOUND_TO_V1") && same_str.contains("REDACTED_PAYLOAD"));
    assert!(same_str.contains("UNSIGNED_THOUGHT_KEPT"));
    assert!(same_payload.pointer("/messages/0/content/0/cache_control").is_none());
    assert_eq!(ptr(&same_payload, "/messages/2/content/0/cache_control/type"), Some("ephemeral"));

    let changed_str =
        encode("sys v2 changed", "compacted summary", vec![assistant_v1, Message::user("follow up")])
            .payload
            .to_string();
    assert!(!changed_str.contains("SIG_BOUND_TO_V1") && !changed_str.contains("REDACTED_PAYLOAD"));
    assert!(changed_str.contains("UNSIGNED_THOUGHT_KEPT") && changed_str.contains("ASSISTANT_BODY_KEPT"));

    let call = ContentBlock::ToolCall(ModelToolCall {
        tool_call_id: "c1".into(),
        tool_name: "read".into(),
        arguments: json!({}),
        metadata: None,
    });
    let res = Message::ToolResult {
        content: vec![ContentBlock::text("ok")],
        tool_call_id: "c1".into(),
        is_error: false,
    };
    let normal_tool =
        encode("sys v1", "first prompt", vec![mk_assistant(vec![call.clone()]), res.clone()]).payload;
    assert_eq!(ptr(&normal_tool, "/thinking/type"), Some("adaptive"));
    let stripped_tool =
        encode("sys v2 changed", "compacted summary", vec![mk_assistant(vec![signed_block, call]), res])
            .payload;
    assert!(stripped_tool.get("thinking").is_none() && stripped_tool.get("output_config").is_none());
}
