use std::collections::{HashMap, HashSet};

use proptest::prelude::*;
use protocol::ProviderApiProtocol;
use serde_json::{Map, Value, json};

use crate::config::selection::SelectedModel;
use crate::error::redact_diagnostic;
use crate::messages::messages_stream_request_payload;
use crate::responses::openai_responses_stream_request_payload;
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
        redacted: bool,
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
            any::<bool>(),
            0usize..=3,
        )
            .prop_map(|(text, thinking, redacted, same_provider, tool_count)| {
                let text = if text.is_none() && thinking.is_none() && tool_count == 0 {
                    Some("fallback reply".to_string())
                } else {
                    text
                };
                TurnStep::Assistant {
                    text,
                    thinking,
                    redacted,
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
                redacted,
                same_provider,
                tool_count,
            } => {
                let mut content = Vec::new();
                if let Some(thought) = thinking {
                    let mut meta = Map::new();
                    match protocol {
                        ProviderApiProtocol::Messages => {
                            if redacted {
                                let data = if same_provider {
                                    "redacted_same"
                                } else {
                                    OTHER_MSG_SIG
                                };
                                meta.insert("type".into(), json!("redacted_thinking"));
                                meta.insert("data".into(), json!(data));
                            } else {
                                let sig = if same_provider { "sig_same" } else { OTHER_MSG_SIG };
                                meta.insert("signature".into(), json!(sig));
                            }
                        }
                        ProviderApiProtocol::Responses => {
                            let enc = if same_provider { "enc_same" } else { OTHER_RESP_ENC };
                            meta.insert("id".into(), json!("rs_prop_1"));
                            meta.insert("encrypted_content".into(), json!(enc));
                        }
                    }
                    content.push(ContentBlock::Thinking {
                        thinking: if protocol == ProviderApiProtocol::Messages && redacted {
                            String::new()
                        } else {
                            thought
                        },
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
        mutate_prefix in any::<bool>(),
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
        let encoded_turn = messages_stream_request_payload(&req, &selection, provider_name, endpoint)
            .expect("messages encoding succeeds");
        let payload = encoded_turn.payload;
        let serialized = payload.to_string();
        prop_assert!(!serialized.contains(OTHER_MSG_SIG), "leaked foreign Messages signature");
        prop_assert!(count_cache_control(&payload) <= 4, "explicit cache_control breakpoints exceed 4");

        let encoded = payload.get("messages").and_then(Value::as_array).expect("messages array");
        prop_assert!(!encoded.is_empty());
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

        let mut next_messages = req.messages;
        if mutate_prefix {
            next_messages.insert(0, Message::developer("mutated prefix"));
        }
        let mk_thinking = |thinking: &str, pairs: &[(&str, &str)]| ContentBlock::Thinking {
            thinking: thinking.into(),
            metadata: Some(ProviderBlockMetadata(
                pairs.iter().map(|(k, v)| ((*k).into(), json!(v))).collect(),
            )),
        };
        next_messages.push(Message::Assistant {
            content: vec![
                mk_thinking("signed thought", &[("signature", "SIG_MATCHED")]),
                mk_thinking("", &[("type", "redacted_thinking"), ("data", "REDACTED_MATCHED")]),
                ContentBlock::text("SIGNED_REPLY_TEXT"),
            ],
            provider_metadata: Some(ProviderMessageMetadata {
                protocol: ProviderApiProtocol::Messages,
                provider_name: provider_name.into(),
                model_name: model_name.into(),
                endpoint: endpoint.into(),
                prefix_digest: Some(encoded_turn.prefix_digest),
                reasoning: None,
            }),
        });
        next_messages.push(Message::user("follow up"));
        let next_str = messages_stream_request_payload(
            &ModelTurnRequest { messages: next_messages, ..req },
            &selection,
            provider_name,
            endpoint,
        )
        .expect("follow-up messages encoding succeeds")
        .payload
        .to_string();
        prop_assert_eq!(next_str.contains("SIG_MATCHED"), !mutate_prefix);
        prop_assert_eq!(next_str.contains("REDACTED_MATCHED"), !mutate_prefix);
        prop_assert!(next_str.contains("SIGNED_REPLY_TEXT"));
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

    #[test]
    fn redact_web_diagnostic_redacts_all_bearer_tokens_and_preserves_word_boundaries(
        tokens in prop::collection::vec("[a-zA-Z0-9._-]{6,20}", 1..=4),
        seps in prop::collection::vec(prop_oneof![Just(" "), Just("   "), Just("\t"), Just(" \t ")], 1..=4),
        cases in prop::collection::vec(prop_oneof![Just("Bearer"), Just("bearer"), Just("BEARER"), Just("BeArEr")], 1..=4),
    ) {
        let mut parts = Vec::new();
        for i in 0..tokens.len() {
            let kw = cases[i % cases.len()];
            let sep = seps[i % seps.len()];
            parts.push(format!("header=\"{kw}{sep}{}\"", tokens[i]));
        }
        let body = format!(
            "standardbearer keep_me Bearer {}-suffix Bearer pre-{} {}",
            tokens[0],
            tokens[0],
            parts.join(", ")
        );
        let redacted = redact_diagnostic(&body, &tokens[0]);
        for tok in &tokens {
            prop_assert!(!redacted.contains(tok), "leaked bearer token `{tok}` in `{redacted}`");
        }
        prop_assert!(!redacted.contains("-suffix"), "leaked bearer suffix in `{redacted}`");
        prop_assert!(!redacted.contains("pre-"), "leaked bearer prefix in `{redacted}`");
        prop_assert!(redacted.contains("standardbearer keep_me"), "word boundary broken: {redacted}");
        prop_assert!(!redacted.contains("[redacted]]"), "double bracket corruption: {redacted}");
    }
}
