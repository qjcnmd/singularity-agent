//! 属性测试（Property-based Tests）：用随机输入验证上下文压缩/切块与会话日志尾行修复的通用不变量。

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use proptest::prelude::*;
use serde_json::json;
use singularity_model::{ContentBlock, Message, ModelToolCall, ProviderMessageMetadata};
use singularity_protocol::ModelSelection;

use crate::compaction::{
    compact_assistant_messages, compact_text, compact_tool_results, partition_into_chunks,
};
use crate::session::{SessionData, SessionManager, session_file_name};

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
    User(String),
    Notification(String),
    RotationHandoff(String, String),
    AssistantWithTools {
        text: String,
        thinking: Option<String>,
        tool_outputs: Vec<String>,
        large_args: bool,
    },
}

fn history_step_strategy() -> impl Strategy<Value = HistoryStep> {
    prop_oneof![
        unicode_text_strategy(4).prop_map(|s| HistoryStep::User(if s.trim().is_empty() {
            "u".into()
        } else {
            s
        })),
        unicode_text_strategy(3).prop_map(|s| HistoryStep::Notification(if s.trim().is_empty() {
            "n".into()
        } else {
            s
        })),
        (unicode_text_strategy(3), unicode_text_strategy(4)).prop_map(|(dev, summary)| {
            HistoryStep::RotationHandoff(
                if dev.trim().is_empty() {
                    "handoff".into()
                } else {
                    dev
                },
                if summary.trim().is_empty() {
                    "summary".into()
                } else {
                    summary
                },
            )
        }),
        (
            unicode_text_strategy(30),
            prop::option::of(unicode_text_strategy(8)),
            prop::collection::vec(unicode_text_strategy(45), 0..=3),
            any::<bool>(),
        )
            .prop_map(|(text, thinking, tool_outputs, large_args)| {
                HistoryStep::AssistantWithTools {
                    text: if text.trim().is_empty() {
                        "reply".into()
                    } else {
                        text
                    },
                    thinking,
                    tool_outputs,
                    large_args,
                }
            }),
    ]
}

fn build_history(with_leading_handoff: bool, steps: Vec<HistoryStep>) -> Vec<Message> {
    let mut messages = Vec::new();
    if with_leading_handoff {
        messages.push(Message::developer("prior rotation handoff"));
        messages.push(Message::user("prior organized summary"));
    }
    let mut seq = 0usize;
    for step in steps {
        match step {
            HistoryStep::User(t) => messages.push(Message::user(t)),
            HistoryStep::Notification(t) => {
                messages.push(Message::Notification { content: vec![ContentBlock::text(t)] });
            }
            HistoryStep::RotationHandoff(dev, summary) => {
                messages.push(Message::developer(dev));
                messages.push(Message::user(summary));
            }
            HistoryStep::AssistantWithTools { text, thinking, tool_outputs, large_args } => {
                let mut content = Vec::new();
                if let Some(thought) = thinking {
                    content.push(ContentBlock::Thinking { thinking: thought, metadata: None });
                }
                content.push(ContentBlock::text(text));
                let mut results = Vec::new();
                for output in tool_outputs {
                    seq += 1;
                    let id = format!("call_{seq}");
                    let args = if large_args {
                        json!({ "blob": "x".repeat(700), "seq": seq })
                    } else {
                        json!({ "seq": seq })
                    };
                    content.push(ContentBlock::ToolCall(ModelToolCall {
                        tool_call_id: id.clone(),
                        tool_name: "tool".into(),
                        arguments: args,
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
                    provider_metadata: Some(ProviderMessageMetadata::Messages {
                        provider_name: "p".into(),
                        model_name: "m".into(),
                        endpoint: "https://example.invalid/v1/messages".into(),
                    }),
                });
                messages.extend(results);
            }
        }
    }
    messages
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// 文本折叠不变量：任意 UTF-8 字符串（含中英文、emoji、换行）与任意头尾字节预算下，
    /// 永不因 UTF-8 字符边界 panic，且若包含 `Full output:` 溢写路径行则必须保留。
    #[test]
    fn compact_text_utf8_and_full_output_invariance(
        body in unicode_text_strategy(60),
        with_spill_line in any::<bool>(),
        head_bytes in 0usize..=1500,
        tail_bytes in 0usize..=1500,
    ) {
        let spill_line = "Full output: D:/tmp/session_spill.log";
        let input = if with_spill_line {
            format!("{body}\n{spill_line}")
        } else {
            body
        };
        let compacted = compact_text(&input, head_bytes, tail_bytes, "compacted");
        if with_spill_line {
            prop_assert!(
                compacted.contains(spill_line),
                "Full output spill path must be preserved after compaction"
            );
        }
    }

    /// 压缩与分块不变量：任意对话历史经降级压缩与原子组分块后，
    /// 1. 绝不残留 Thinking 或 provider_metadata；
    /// 2. 每个批次非空，且不会产出仅含 Developer 消息的空壳批次；
    /// 3. 每个工具调用和对应工具结果始终位于同一批次且严格 1:1 配对。
    #[test]
    fn compaction_and_partition_preserve_invariants(
        with_leading_handoff in any::<bool>(),
        steps in prop::collection::vec(history_step_strategy(), 1..=10),
        chunk_budget in 150u64..=4000,
    ) {
        let mut history = build_history(with_leading_handoff, steps);
        compact_tool_results(&mut history);
        compact_assistant_messages(&mut history);

        for msg in &history {
            if let Message::Assistant { content, provider_metadata } = msg {
                prop_assert!(provider_metadata.is_none(), "provider_metadata must be stripped");
                prop_assert!(
                    content.iter().all(|b| !matches!(b, ContentBlock::Thinking { .. })),
                    "thinking blocks must be stripped"
                );
            }
        }

        let chunks = partition_into_chunks(history, chunk_budget);
        prop_assert!(!chunks.is_empty());
        for chunk in chunks {
            prop_assert!(!chunk.is_empty(), "chunk must not be empty");
            prop_assert!(
                chunk.iter().any(|m| !matches!(m, Message::Developer { .. })),
                "chunk must contain at least one non-Developer message"
            );
            let mut open_calls: HashSet<String> = HashSet::new();
            for msg in &chunk {
                match msg {
                    Message::Assistant { .. } => {
                        prop_assert!(
                            open_calls.is_empty(),
                            "unanswered tool_call before next assistant in chunk"
                        );
                        for call in msg.tool_calls() {
                            open_calls.insert(call.tool_call_id.clone());
                        }
                    }
                    Message::ToolResult { tool_call_id, .. } => {
                        prop_assert!(
                            open_calls.remove(tool_call_id),
                            "orphaned tool_result {tool_call_id} in chunk"
                        );
                    }
                    _ => {
                        prop_assert!(
                            open_calls.is_empty(),
                            "non-tool message interrupted tool_call/tool_result group"
                        );
                    }
                }
            }
            prop_assert!(open_calls.is_empty(), "dangling tool_call at end of chunk: {open_calls:?}");
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
        let expected_entries = manager.entries().len();
        let path = sessions.join(session_file_name(&session_id));
        let valid_len = std::fs::metadata(&path).expect("metadata").len();
        drop(manager);

        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&path).expect("open for append");
            file.write_all(&trailing_bytes).expect("append truncated tail");
        }

        let readonly = SessionData::open(&path).expect("readonly open ignores incomplete tail");
        prop_assert_eq!(readonly.entries().len(), expected_entries);

        let repaired = SessionManager::open_existing(&path, &session_id).expect("writer repairs tail");
        prop_assert_eq!(repaired.entries().len(), expected_entries);
        drop(repaired);

        let repaired_len = std::fs::metadata(&path).expect("metadata after repair").len();
        prop_assert_eq!(repaired_len, valid_len, "repair must truncate back to exact valid boundary");
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
