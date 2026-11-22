//! 属性测试（Property-based Tests）：用随机输入验证上下文压缩切分、投影与会话日志尾行修复的通用不变量。

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use model::{ContentBlock, Message, ModelToolCall, ProviderMessageMetadata};
use proptest::prelude::*;
use protocol::ModelSelection;
use serde_json::json;

use crate::session::context::{project_messages, resolve_context_entries, select_compactable_split};
use crate::session::{CompactionEntry, SessionData, SessionEntry, SessionManager, session_file_name};

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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 多轮压缩切分与投影不变量：工具配对、retain_tokens==0 留最后 1 原子组、旧检查点不复活。
    #[test]
    fn compaction_split_and_multi_round_projection_invariants(
        rounds in prop::collection::vec(compaction_round_strategy(), 1..=4),
    ) {
        let tmp = TempDir::new("compaction-split");
        let workspace = tmp.0.join("ws");
        let sessions = tmp.0.join("sessions");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let session_id = uuid::Uuid::now_v7().to_string();
        let selection = ModelSelection {
            provider: "p".into(),
            model: "m".into(),
            reasoning: None,
        };
        let mut manager = SessionManager::create_with_id(&workspace, &sessions, &session_id, selection, vec![])
            .expect("create session");
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
                // 即使 retain_tokens == 0 也仅保留最后 1 个完整原子单元（首条消息 + 紧随的 ToolResult）。
                let tail_after_first_is_only_tool_results = kept[1..].iter().all(|entry| {
                    matches!(entry, SessionEntry::Message { message: Message::ToolResult { .. }, .. })
                });
                prop_assert!(
                    tail_after_first_is_only_tool_results,
                    "retain_tokens == 0 must keep exactly the last balanced atomic unit"
                );
            }
            assert_balanced_tool_calls(&project_messages(kept));

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
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 会话日志尾行自愈不变量：任意合法会话末尾追加不含换行的任意残缺字节（含非法 UTF-8 / 残缺 JSON），
    /// 只读打开与写者修复打开都必须完整保留全部已落盘记录，修复后再次打开无残留脏字节。
    #[test]
    fn session_file_tail_repair_invariance(
        user_texts in prop::collection::vec(unicode_seed_strategy(1, 48), 0..=5),
        trailing_bytes in prop::collection::vec(any::<u8>().prop_filter("no newline", |b| *b != b'\n'), 1..=128),
    ) {
        let tmp = TempDir::new("tail-repair");
        let workspace = tmp.0.join("ws");
        let sessions = tmp.0.join("sessions");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        let session_id = uuid::Uuid::now_v7().to_string();
        let selection = ModelSelection {
            provider: "p".into(),
            model: "m".into(),
            reasoning: None,
        };
        let mut manager = SessionManager::create_with_id(&workspace, &sessions, &session_id, selection, vec![])
            .expect("create session");
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

    /// 任意随机字节内容绝不能使会话日志解析器 panic。
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
