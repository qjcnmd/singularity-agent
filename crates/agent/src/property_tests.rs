use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use model::{ContentBlock, Message, ModelConfigurationSnapshot, ModelToolCall, ProviderMessageMetadata};
use proptest::prelude::*;
use protocol::{ModelSelection, ModelUsage, ProviderAttemptStatus, RequestObservation, RequestPurpose};
use serde_json::json;

use crate::request_execution::{min_compaction_output_tokens, output_budget_tokens};
use crate::session::context::{
    frame_compacted_summary, input_tokens, message_token_estimate, plan_compaction_step, project_messages,
    resolve_context_entries, select_compactable_split,
};
use crate::session::{
    CompactionEntry, LedgerRecord, SessionData, SessionEntry, SessionManager, session_file_name,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("singularity-prop-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unicode_seed_strategy(min_len: usize, max_len: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just('a'),
            Just('Z'),
            Just('0'),
            Just(' '),
            Just('\n'),
            Just('中'),
            Just('文'),
            Just('测'),
            Just('试'),
            Just('🚀'),
            Just('é'),
        ],
        min_len..max_len,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

fn unicode_text_strategy(max_repeats: usize) -> impl Strategy<Value = String> {
    (unicode_seed_strategy(1, 48), 1usize..=max_repeats).prop_map(|(seed, times)| seed.repeat(times))
}

#[derive(Debug, Clone)]
enum HistoryStep {
    User {
        text: String,
        with_image: bool,
    },
    Notification(String),
    AssistantWithTools {
        text: String,
        thinking: Option<String>,
        tool_outputs: Vec<String>,
    },
}

fn build_history(steps: Vec<HistoryStep>) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut seq = 0usize;
    for step in steps {
        match step {
            HistoryStep::User { text, with_image } => {
                let mut content = vec![ContentBlock::text(text)];
                if with_image {
                    seq += 1;
                    content.push(ContentBlock::Image(protocol::ImageAttachment {
                        id: format!("img_{seq}.png"),
                        name: "img.png".into(),
                        mime_type: "image/png".into(),
                        width: 64,
                        height: 64,
                    }));
                }
                messages.push(Message::User { content });
            }
            HistoryStep::Notification(t) => {
                messages.push(Message::Notification { content: vec![ContentBlock::text(t)] });
            }
            HistoryStep::AssistantWithTools { text, thinking, tool_outputs } => {
                let mut content = Vec::new();
                if let Some(thought) = thinking {
                    content.push(ContentBlock::Thinking {
                        thinking: thought,
                        metadata: serde_json::from_value(json!({ "signature": "sig_test" })).ok(),
                    });
                }
                content.push(ContentBlock::text(text));
                let mut results = Vec::new();
                for output in tool_outputs {
                    seq += 1;
                    let id = format!("call_{seq}");
                    content.push(ContentBlock::ToolCall(ModelToolCall {
                        tool_call_id: id.clone(),
                        tool_name: "tool".into(),
                        arguments: json!({ "seq": seq }),
                        metadata: None,
                    }));
                    results.push(Message::ToolResult {
                        content: vec![ContentBlock::text(output)],
                        tool_call_id: id,
                        is_error: false,
                    });
                }
                messages.push(Message::Assistant {
                    content,
                    provider_metadata: Some(ProviderMessageMetadata {
                        protocol: protocol::ProviderApiProtocol::Messages,
                        provider_name: "p".into(),
                        model_name: "m".into(),
                        endpoint: "https://example.invalid/v1/messages".into(),
                        prefix_digest: None,
                        reasoning: None,
                    }),
                });
                messages.extend(results);
            }
        }
    }
    messages
}

fn assert_balanced_tool_calls(messages: &[Message]) {
    let mut open_calls: HashSet<String> = HashSet::new();
    for msg in messages {
        match msg {
            Message::Assistant { .. } => {
                assert!(open_calls.is_empty(), "unanswered tool_call before next assistant");
                for call in msg.tool_calls() {
                    assert!(open_calls.insert(call.tool_call_id.clone()), "duplicate tool_call id");
                }
            }
            Message::ToolResult { tool_call_id, .. } => {
                assert!(open_calls.remove(tool_call_id), "orphaned tool_result {tool_call_id}");
            }
            _ => {
                assert!(open_calls.is_empty(), "non-tool message interrupted tool_call/tool_result group");
            }
        }
    }
    assert!(open_calls.is_empty(), "dangling tool_call at end: {open_calls:?}");
}

#[derive(Debug, Clone)]
struct CompactionRoundInput {
    steps: Vec<HistoryStep>,
    drop_last_tool_result: bool,
    retain_tokens: u64,
}

fn compaction_round_strategy() -> impl Strategy<Value = CompactionRoundInput> {
    (
        prop::collection::vec(
            prop_oneof![
                (unicode_text_strategy(10), any::<bool>()).prop_map(|(s, with_image)| {
                    HistoryStep::User {
                        text: if s.trim().is_empty() { "u".into() } else { s },
                        with_image,
                    }
                }),
                unicode_text_strategy(6).prop_map(|s| HistoryStep::Notification(if s.trim().is_empty() {
                    "n".into()
                } else {
                    s
                })),
                (
                    unicode_text_strategy(15),
                    prop::option::of(unicode_text_strategy(8)),
                    prop::collection::vec(unicode_text_strategy(120), 0..=3),
                )
                    .prop_map(|(text, thinking, tool_outputs)| {
                        HistoryStep::AssistantWithTools {
                            text: if text.trim().is_empty() {
                                "reply".into()
                            } else {
                                text
                            },
                            thinking: thinking
                                .map(|t| if t.trim().is_empty() { "thought".into() } else { t }),
                            tool_outputs,
                        }
                    }),
            ],
            1..=8,
        ),
        any::<bool>(),
        prop_oneof![Just(0u64), 1u64..=800],
    )
        .prop_map(|(steps, drop_last_tool_result, retain_tokens)| CompactionRoundInput {
            steps,
            drop_last_tool_result,
            retain_tokens,
        })
}

fn new_test_session(tag: &str, provider: &str, model: &str) -> (TempDir, PathBuf, String, SessionManager) {
    let tmp = TempDir::new(tag);
    let (workspace, sessions) = (tmp.0.join("ws"), tmp.0.join("sessions"));
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let sid = uuid::Uuid::now_v7().to_string();
    let sel = ModelSelection {
        provider: provider.into(),
        model: model.into(),
        reasoning: None,
    };
    let mgr =
        SessionManager::create_with_id(&workspace, &sessions, &sid, sel, vec![]).expect("create session");
    (tmp, sessions, sid, mgr)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn compaction_split_and_multi_round_projection_invariants(
        rounds in prop::collection::vec(compaction_round_strategy(), 1..=4),
    ) {
        let (_tmp, _sessions, _sid, mut manager) = new_test_session("compaction-split", "p", "m");
        let mut seq = 0usize;

        for (round_idx, round) in rounds.into_iter().enumerate() {
            let mut round_msgs = build_history(round.steps);
            if round.drop_last_tool_result
                && matches!(round_msgs.last(), Some(Message::ToolResult { .. }))
            {
                round_msgs.pop();
            }
            for msg in round_msgs {
                seq += 1;
                let id = format!("msg_{seq}");
                manager
                    .append_message_with_id(&id, msg, None, None)
                    .expect("append round message");
            }

            let data = manager.data();
            let active_before = resolve_context_entries(data);
            let Some((split_idx, first_kept_id)) =
                select_compactable_split(&active_before, data, round.retain_tokens)
            else {
                continue;
            };
            prop_assert!(split_idx >= 1 && split_idx < active_before.len());
            let prefix = &active_before[..split_idx];
            let kept = &active_before[split_idx..];
            prop_assert!(
                prefix.iter().any(|e| matches!(e, SessionEntry::Message { .. })),
                "compacted prefix must contain at least one raw message"
            );

            assert_balanced_tool_calls(&project_messages(prefix));

            prop_assert!(!kept.is_empty());
            prop_assert_eq!(first_kept_id.as_str(), kept[0].id());
            prop_assert!(
                !matches!(
                    kept[0],
                    SessionEntry::Message { message: Message::ToolResult { .. }, .. }
                ),
                "kept tail must not start with an orphaned ToolResult"
            );
            if round.retain_tokens == 0 {
                let tail_after_first_is_only_tool_results = kept[1..].iter().all(|entry| {
                    matches!(entry, SessionEntry::Message { message: Message::ToolResult { .. }, .. })
                });
                prop_assert!(
                    tail_after_first_is_only_tool_results,
                    "retain_tokens == 0 must keep exactly the last balanced atomic unit"
                );
            }
            assert_balanced_tool_calls(&project_messages(kept));

            let model = ModelConfigurationSnapshot { max_context_tokens: 4_000, max_output_tokens: 512 };
            let overflow =
                (round.retain_tokens == 0).then(|| input_tokens(data, &active_before, ("p", "m"), 400));
            if let Ok(Some(step)) =
                plan_compaction_step(data, &first_kept_id, ("p", "m"), 400, 300, model, overflow)
            {
                let budget = u64::from(output_budget_tokens(model, step.estimated_request_input));
                prop_assert!(budget >= min_compaction_output_tokens(model) && budget > 0);
                if let Some(over) = overflow {
                    prop_assert!(step.estimated_request_input < over);
                }
                assert_balanced_tool_calls(&step.messages);
            }

            let expected_kept_ids: Vec<String> = kept.iter().map(|e| e.id().to_owned()).collect();
            let compact_id = format!("compact_{round_idx}");
            let summary = format!("summary_{round_idx}");
            manager
                .append_compaction_with_id(
                    &compact_id,
                    CompactionEntry {
                        summary: summary.clone(),
                        first_kept_entry_id: Some(first_kept_id),
                    },
                )
                .expect("append compaction entry");

            let active_after = resolve_context_entries(manager.data());
            prop_assert_eq!(active_after[0].id(), compact_id.as_str());
            prop_assert!(
                active_after[1..]
                    .iter()
                    .all(|e| matches!(e, SessionEntry::Message { .. })),
                "older compaction entries must never reappear in active tail"
            );
            let actual_kept_ids: Vec<String> =
                active_after[1..].iter().map(|e| e.id().to_owned()).collect();
            prop_assert_eq!(actual_kept_ids, expected_kept_ids);
            assert_balanced_tool_calls(&project_messages(&active_after));
        }
    }

    #[test]
    fn turn_calibration_and_foreign_thinking_invariants(
        reply_text in unicode_seed_strategy(1, 32),
        thinking_extra in unicode_text_strategy(60),
        turn2_user in unicode_seed_strategy(1, 32),
        reported_total in 500u64..=50_000,
    ) {
        let (_tmp, _sessions, _sid, mut mgr) = new_test_session("turn-calibration", "pa", "ma");
        let usage = ModelUsage {
            input_tokens: reported_total - 50,
            output_tokens: 50,
            total_tokens: reported_total,
            cached_input_tokens: None,
        };
        let obs = RequestObservation {
            purpose: RequestPurpose::Generation,
            provider: "pa".into(),
            model: "ma".into(),
            status: ProviderAttemptStatus::Ok,
            ttft_ms: None,
            decode_ms: None,
            usage: Some(usage),
        };
        let redacted = serde_json::from_value(json!({ "type": "redacted_thinking", "data": "x" })).ok();
        let meta = ProviderMessageMetadata {
            protocol: protocol::ProviderApiProtocol::Messages,
            provider_name: "pa".into(),
            model_name: "ma".into(),
            endpoint: "https://example.invalid/v1/messages".into(),
            prefix_digest: None,
            reasoning: None,
        };
        let assistant = Message::Assistant {
            content: vec![
                ContentBlock::Thinking { thinking: thinking_extra, metadata: None },
                ContentBlock::Thinking { thinking: String::new(), metadata: redacted },
                ContentBlock::text(reply_text.clone()),
            ],
            provider_metadata: Some(meta),
        };
        mgr.append_message_with_id("a1", assistant, Some(obs), None).expect("append a1");
        mgr.append_record(LedgerRecord::OperationStarted).expect("append turn boundary");
        mgr.append_message_with_id("u2", Message::user(turn2_user.as_str()), None, None).expect("append u2");
        let tail = message_token_estimate(&Message::user(turn2_user.as_str()));
        let active = resolve_context_entries(mgr.data());
        let plain = message_token_estimate(&Message::Assistant {
            content: vec![ContentBlock::text(reply_text)],
            provider_metadata: None,
        });
        prop_assert_eq!(input_tokens(mgr.data(), &active, ("pa", "ma"), 300), reported_total + tail);
        prop_assert_eq!(input_tokens(mgr.data(), &active, ("pb", "mb"), 300), plain + tail + 300);
        let cp = CompactionEntry { summary: "s".into(), first_kept_entry_id: Some("u2".into()) };
        mgr.append_compaction_with_id("cp", cp).expect("append compaction");
        let after = resolve_context_entries(mgr.data());
        let expected = message_token_estimate(&frame_compacted_summary("s")) + tail + 300;
        prop_assert_eq!(input_tokens(mgr.data(), &after, ("pa", "ma"), 300), expected);
    }

    #[test]
    fn session_file_tail_repair_invariance(
        user_texts in prop::collection::vec(unicode_seed_strategy(1, 48), 0..=5),
        trailing_bytes in prop::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 1..=128),
    ) {
        let (_tmp, sessions, session_id, mut manager) = new_test_session("tail-repair", "p", "m");
        for (idx, text) in user_texts.iter().enumerate() {
            manager
                .append_message_with_id(&format!("msg_{idx}"), Message::user(text.clone()), None, None)
                .expect("append user message");
        }
        let expected_entries = manager.data().entries().len();
        let path = sessions.join(session_file_name(&session_id));
        let valid_bytes = std::fs::read(&path).expect("read valid log");
        drop(manager);

        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open for append");
            file.write_all(&trailing_bytes).expect("append truncated tail");
        }
        let tampered_bytes = std::fs::read(&path).expect("read tampered log");

        let readonly = SessionData::open(&path).expect("readonly open ignores incomplete tail");
        prop_assert_eq!(readonly.entries().len(), expected_entries);
        prop_assert_eq!(
            std::fs::read(&path).expect("read after readonly open"),
            tampered_bytes,
            "readonly open must never modify session file on disk"
        );

        let repaired = SessionManager::open_existing(&path, &session_id).expect("writer repairs tail");
        prop_assert_eq!(repaired.data().entries().len(), expected_entries);
        drop(repaired);

        prop_assert_eq!(
            std::fs::read(&path).expect("read after repair"),
            valid_bytes,
            "repair must restore exact valid prefix bytes"
        );
    }

    #[test]
    fn session_file_never_panics_on_arbitrary_bytes(
        bytes in prop::collection::vec(any::<u8>(), 0..512),
    ) {
        let tmp = TempDir::new("fuzz-parse");
        let path = tmp.0.join("random.jsonl");
        std::fs::write(&path, &bytes).expect("write random bytes");
        let _ = SessionData::open(&path);
    }
}
