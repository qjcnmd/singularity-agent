#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 合法压缩切点与工具调用/结果配对保持测试。
//!
//! 核心不变量：
//! 1. 切点绝不切在工具结果（ToolResult）中间；ToolCall 与其配对的 ToolResult 必须落在切点同一侧。
//! 2. 保留预算跨过工具结果时，向前退到配对调用，允许保留同一轮次的后半段。
//! 3. 压缩条目记录在 step attempt 预分配的结果条目 id 上，ContextView 据此完整重建历史上下文。

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    Provider,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

use super::{CompactionConfig, CompactionOutcome};
use crate::message::{AgentMessage, ContentBlock};
use crate::session::context::ContextView;
use crate::session::test_support::SessionFixture;
use crate::session::test_support::messages::{assistant, tool_result, user};
use crate::session::{CompactionEntry, SessionEntry, SessionError};

fn agent(
    writer: crate::session::SessionWriter,
    provider: Arc<ScriptedProvider>,
) -> crate::agent::Agent {
    let mut model = provider.model_configuration();
    model.max_context_tokens = 8_000;
    crate::agent::Agent::new(
        crate::agent::TurnInbox::default_handle(),
        provider,
        model,
        crate::tools::ToolRegistrySnapshot::default(),
        crate::agent::AgentConfig {
            system_prompt: "system rules".into(),
            instruction_home: None,
            initial_instructions: None,
            compaction: CompactionConfig::default(),
        },
        writer,
    )
    .unwrap()
}

fn assistant_with_call(call_id: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall(singularity_model::ModelToolCall {
            tool_call_id: call_id.to_string(),
            tool_name: "read".to_string(),
            arguments: serde_json::json!({"path": "notes.txt"}),
        })],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
}

/// 构造一个已落盘给定消息序列的隔离会话（写者已交接）。
fn fixture_with(id: &str, messages: &[AgentMessage]) -> SessionFixture {
    let fixture = SessionFixture::new();
    let mut session = fixture.create_session(fixture.home(), id).expect("create");
    for message in messages {
        session.append_message(message.clone()).expect("append");
    }
    drop(session);
    fixture
}

/// compact() 端到端：压缩条目落在 attempt ledger 预分配的结果条目 id 上；
/// ContextView 基于最新压缩节点重建，保留侧的工具对完整。
#[test]
fn compact_persists_at_reserved_id_and_context_view_keeps_pairs() {
    let id = "01914f6b-0000-7000-8000-0000000000f3";
    let messages = [
        user(&"old question ".repeat(100)),
        assistant("old answer"),
        user("question with tool"),
        assistant_with_call("call-1"),
        tool_result("call-1", &"result payload ".repeat(400)),
    ];
    let fixture = fixture_with(id, &messages);
    let session = fixture.open_for_repair(id).unwrap();
    let entries_before: Vec<SessionEntry> = session.entries().to_vec();
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));

    let provider = Arc::new(ScriptedProvider::ok("## Goal\nkeep going"));
    let outcome = agent(writer.clone(), provider.clone())
        .compact_now(&mut |_| {}, &CancellationToken::new())
        .expect("compact");
    assert_eq!(outcome, CompactionOutcome::Reduced);

    let session = crate::session::lock_writer(&writer);
    let last = session.entries().last().expect("compaction entry");
    assert_eq!(
        last.id(),
        provider.requests()[0].request_id,
        "entry lands on the attempt ledger's reserved id"
    );
    assert!(matches!(
        last,
        SessionEntry::Compaction { compaction, .. }
            if compaction.summary.contains("## Goal")
                && compaction.first_kept_entry_id == entries_before[3].id()
    ));

    let view = ContextView::derive(&session).expect("context");
    let visible: Vec<_> = view.original_entries(&session).cloned().collect();
    assert!(
        matches!(&visible.as_slice()[0], SessionEntry::Compaction { .. }),
        "the rebuilt view starts at the newest compaction node"
    );
    assert_eq!(
        visible
            .as_slice()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message { .. }))
            .count(),
        2,
        "kept tail: assistant(tool call), tool result"
    );
    assert_pairs_intact(visible.as_slice());
}

#[test]
fn context_rejects_a_missing_compaction_anchor() {
    let id = "01914f6b-0000-7000-8000-0000000000f5";
    let fixture = fixture_with(id, &[user("older"), assistant("newer")]);
    let mut session = fixture.open_for_repair(id).unwrap();
    session
        .append_compaction_with_id(
            "invalid-compaction",
            CompactionEntry {
                summary: "summary".to_string(),
                first_kept_entry_id: "missing-anchor".to_string(),
            },
        )
        .unwrap();
    let error = ContextView::derive(&session).expect_err("missing anchor must be rejected");
    assert!(matches!(error, SessionError::LedgerCorrupt { reason, .. }
        if reason == "invalid_compaction_anchor"));
}

/// 无历史可摘要时 NotNeeded，不写任何条目（无出站请求，也无 step attempt）。
#[test]
fn compact_without_summarizable_history_is_not_needed() {
    let id = "01914f6b-0000-7000-8000-0000000000f4";
    let fixture = fixture_with(id, &[user("only message")]);
    let session = fixture.open_for_repair(id).unwrap();
    let entries_before: Vec<SessionEntry> = session.entries().to_vec();
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));
    let provider = Arc::new(ScriptedProvider::ok("summary"));
    let outcome = agent(writer.clone(), provider.clone())
        .compact_now(&mut |_| {}, &CancellationToken::new())
        .expect("compact call");
    assert_eq!(outcome, CompactionOutcome::NotNeeded);
    assert!(provider.requests().is_empty());
    assert_eq!(
        crate::session::lock_writer(&writer).entries().len(),
        entries_before.len()
    );
}

/// 摘要请求的计量只经统一请求账本的 observation 落盘。
///
/// 条目自身不再复制一份 usage：会话累计与工作台展示都从该请求的
/// request observation 读取。摘要请求仍然被完整计量（这里断言该观测带 usage）。
#[test]
fn a_summary_request_is_metered_through_the_request_ledger_only() {
    use singularity_model::ModelUsage;
    use singularity_protocol::RequestPurpose;

    let id = "01914f6b-0000-7000-8000-0000000000fd";
    let fixture = fixture_with(
        id,
        &[
            user(&"old question ".repeat(100)),
            assistant("old answer"),
            user("latest"),
        ],
    );
    let session = fixture.open_for_repair(id).unwrap();
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));
    let usage = ModelUsage {
        input_tokens: 120,
        output_tokens: 8,
        total_tokens: 128,
        usage_present: true,
        ..ModelUsage::default()
    };
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success_with_usage("## Goal\nkeep going", usage),
    ]));
    agent(writer.clone(), provider)
        .compact_now(&mut |_| {}, &CancellationToken::new())
        .expect("compact");

    let session = crate::session::lock_writer(&writer);
    let entry = session
        .entries()
        .iter()
        .find(|entry| matches!(entry, SessionEntry::Compaction { .. }))
        .expect("compaction entry");
    let SessionEntry::Compaction { compaction, .. } = entry else {
        unreachable!()
    };
    // 条目的 wire 形状由 jsonl 夹具用例固定；这里只确认摘要与保留锚点确实落盘。
    assert_eq!(compaction.summary, "## Goal\nkeep going");
    assert!(!compaction.first_kept_entry_id.is_empty());

    let observations: Vec<_> = session
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Record {
                record:
                    crate::session::LedgerRecord::ModelRequest {
                        observation:
                            observation @ singularity_protocol::RequestObservation {
                                purpose: RequestPurpose::Compaction,
                                ..
                            },
                        ..
                    },
                ..
            } => Some(observation.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        observations.len(),
        2,
        "one started observation and one terminal observation"
    );
    assert!(
        observations
            .iter()
            .all(|observation| observation.request_id == entry.id()),
        "both observations name the summary request itself: {observations:?}"
    );
    let metered: Vec<_> = observations
        .iter()
        .filter(|observation| observation.input_tokens.is_some())
        .collect();
    assert_eq!(
        metered.len(),
        1,
        "only the terminal observation reports usage"
    );
    assert_eq!(metered[0].input_tokens, Some(120));
    assert_eq!(metered[0].output_tokens, Some(8));
}

/// 保留区内每个 ToolResult 都有配对的 ToolCall，反之亦然。
fn assert_pairs_intact(entries: &[SessionEntry]) {
    let mut calls: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for entry in entries {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        match message {
            AgentMessage::Assistant { .. } => {
                calls.extend(message.tool_calls().map(|call| call.tool_call_id.clone()));
            }
            AgentMessage::ToolResult { tool_call_id, .. } => {
                results.push(tool_call_id.clone());
            }
            AgentMessage::User { .. } => {}
        }
    }
    for id in &results {
        assert!(
            calls.contains(id),
            "tool result {id} kept without its tool call: pair split"
        );
    }
    for id in &calls {
        assert!(
            results.contains(id),
            "tool call {id} kept without its result: pair split"
        );
    }
}

#[test]
fn repeated_compaction_replaces_active_prefix_without_resurrecting_prior_summary() {
    let id = "01914f6b-0000-7000-8000-0000000000f7";
    let fixture = fixture_with(
        id,
        &[
            user(&"old ".repeat(500)),
            assistant(&"tail ".repeat(400)),
            user("latest"),
        ],
    );
    let mut session = fixture.open_for_repair(id).unwrap();
    let tail = session.entries()[1].id().to_string();
    let latest = session.entries()[2].id().to_string();
    session
        .append_compaction_with_id(
            &uuid::Uuid::new_v4().to_string(),
            CompactionEntry {
                summary: "first checkpoint".into(),
                first_kept_entry_id: tail,
            },
        )
        .unwrap();
    session
        .append_compaction_with_id(
            &uuid::Uuid::new_v4().to_string(),
            CompactionEntry {
                summary: "second checkpoint".into(),
                first_kept_entry_id: latest.clone(),
            },
        )
        .unwrap();
    let view = ContextView::derive(&session).unwrap();
    let visible: Vec<_> = view.original_entries(&session).cloned().collect();
    assert_eq!(visible.as_slice().len(), 2);
    assert_eq!(visible.as_slice()[1].id(), latest);
    assert!(
        matches!(&visible.as_slice()[0], SessionEntry::Compaction { compaction, .. } if compaction.summary == "second checkpoint")
    );
}

#[test]
fn summary_reuses_system_messages_and_tools() {
    use singularity_model::{ModelMessage, ModelRole};
    let id = "01914f6b-0000-7000-8000-0000000000f8";
    let mut call = assistant_with_call("one");
    if let AgentMessage::Assistant {
        provider_reasoning_replay,
        ..
    } = &mut call
    {
        *provider_reasoning_replay = Some(singularity_model::ProviderReasoningReplay::Chat {
            provider_name: "test".into(),
            model_name: "test".into(),
            reasoning_effort: None,
            tool_call_ids: vec!["one".into()],
            reasoning_content: "private context".into(),
            reasoning_field: "reasoning_content".into(),
            reasoning_details: vec![],
        });
    }
    let fixture = fixture_with(
        id,
        &[
            user("question"),
            call,
            tool_result("one", &"z".repeat(6000)),
            user("keep this"),
        ],
    );
    let session = fixture.open_for_repair(id).unwrap();
    let messages = ContextView::derive(&session).unwrap().messages(&session);
    let writer = Arc::new(std::sync::Mutex::new(session));
    let scripted = Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "checkpoint",
    )]));
    let mut original = vec![ModelMessage::text(ModelRole::Developer, "system rules")];
    original.extend(messages[..3].iter().cloned());
    agent(writer, scripted.clone())
        .compact_now(&mut |_| {}, &CancellationToken::new())
        .unwrap();
    let requests = scripted.requests();
    let output = requests[0].model_preferences.max_output_tokens.unwrap();
    // 上限只受模型输出上限约束，不再受窗口剩余空间约束。
    assert!(output > 0 && output < super::DEFAULT_SUMMARY_MAX_TOKENS);
    // 摘要请求带上本轮冻结的工具定义：这是它成为上一次真实请求真前缀的前提。
    assert_eq!(
        requests[0].tools,
        crate::tools::ToolRegistrySnapshot::default().provider_schemas()
    );
    assert_eq!(requests[0].messages[..4], original);
    assert!(requests[0].messages[2].provider_reasoning_replay.is_some());
    assert_eq!(
        requests[0].messages.last().unwrap().content,
        super::COMPACTION_INSTRUCTION
    );
    assert!(
        !requests[0]
            .messages
            .iter()
            .any(|message| message.content == "keep this")
    );
}

/// 摘要只要求正文非空：不比被替换的历史小也照样落盘（越压越大交给压缩重试与
/// 最终失败兜底）；被拒绝时不留下任何替换记录。
#[test]
fn summary_acceptance_depends_only_on_non_empty_text() {
    let id = "01914f6b-0000-7000-8000-0000000000f9";
    let huge = "huge ".repeat(300);
    for (summary, accepted) in [("", false), (" ", false), (huge.as_str(), true)] {
        let fixture = fixture_with(id, &[user("short history"), user("last")]);
        let session = fixture.open_for_repair(id).unwrap();
        let entries = session.entries().to_vec();
        let writer = Arc::new(std::sync::Mutex::new(session));
        let result = agent(writer.clone(), Arc::new(ScriptedProvider::ok(summary)))
            .compact_now(&mut |_| {}, &CancellationToken::new());
        assert_eq!(result.is_ok(), accepted, "{summary:?}");
        let committed = crate::session::lock_writer(&writer)
            .entries()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Compaction { .. }))
            .count();
        assert_eq!(committed, usize::from(accepted), "{summary:?}");
        if !accepted {
            assert_eq!(
                crate::session::lock_writer(&writer)
                    .entries()
                    .iter()
                    .filter(|entry| crate::session::context::is_context_entry(entry))
                    .cloned()
                    .collect::<Vec<_>>(),
                entries
            );
        }
    }
}

#[test]
fn unicode_pruning_preserves_head_tail_and_original_history_after_reopen() {
    let id = "01914f6b-0000-7000-8000-0000000000fa";
    let text = format!(
        "{}{}{}",
        "😀".repeat(4096),
        "中".repeat(4000),
        "尾".repeat(1024)
    );
    let mut result = tool_result("one", &text);
    if let AgentMessage::ToolResult { diff, .. } = &mut result {
        *diff = Some("diff remains in the original UI history".into());
    }
    let fixture = fixture_with(id, &[assistant_with_call("one"), result]);
    let mut session = fixture.open_for_repair(id).unwrap();
    let original = session.entries()[1].clone();
    let SessionEntry::Message {
        message: AgentMessage::ToolResult { content, .. },
        ..
    } = &original
    else {
        panic!()
    };
    let replacement = super::prune_tool_content(content).unwrap();
    assert!(super::prune_tool_content(&replacement).is_none());
    session
        .append_record(crate::session::LedgerRecord::ToolResultPruned {
            entry_id: original.id().into(),
            content: replacement,
        })
        .unwrap();
    let live = ContextView::derive(&session).unwrap();
    let live_messages = live.messages(&session);
    let live_tokens = live.request_tokens(0);
    drop(session);
    let reopened = fixture.open_read_only(id).unwrap();
    assert_eq!(reopened.entries()[1], original);
    let view = ContextView::derive(&reopened).unwrap();
    let visible: Vec<_> = view.original_entries(&reopened).cloned().collect();
    let messages = view.messages(&reopened);
    assert_eq!(messages, live_messages);
    assert_eq!(view.request_tokens(0), live_tokens);
    let pruned = &messages[1].content;
    assert!(!pruned.contains("diff remains"));
    assert!(pruned.starts_with(&"😀".repeat(4096)));
    assert!(pruned.ends_with(&"尾".repeat(1024)));
    assert!(!pruned.contains('中'));
    assert_pairs_intact(visible.as_slice());
}

/// 无模型剪枝按字符跨全部文本块累计头尾预算：非文本块位置不变，头尾可以落在
/// 不同块，省略标记只写一次，完全落在裁剪区间的文本块被整块丢弃。
#[test]
fn pruning_spans_text_blocks_and_keeps_non_text_blocks_in_place() {
    let text = |text: &str| ContentBlock::Text {
        text: text.to_string(),
    };
    let marker = "\n\n[... tool result middle pruned ...]\n\n";

    // 阈值边界：字符总数刚好等于下限时不剪。
    let at_threshold = vec![text(&"界".repeat(super::PRUNE_MIN_CHARS))];
    assert!(super::prune_tool_content(&at_threshold).is_none());

    // 按字符而不是 UTF-8 字节计数：每个 CJK 字符占 3 字节，若按字节计算，头部
    // 块会在 4096 字节处被切开，下面的等值断言即失败。
    let head = "头".repeat(super::PRUNE_KEEP_HEAD_CHARS);
    let tail = "尾".repeat(super::PRUNE_KEEP_TAIL_CHARS);
    let thinking = ContentBlock::Thinking {
        thinking: "keep thinking in place".to_string(),
    };
    let content = vec![
        text(&head),
        text(&"剪".repeat(5000)),
        thinking.clone(),
        text(&"丢".repeat(100)),
        text(&"弃".repeat(50)),
        text(&tail),
    ];
    let pruned = super::prune_tool_content(&content).expect("over the pruning threshold");
    assert_eq!(
        pruned,
        vec![text(&head), text(marker), thinking, text(&tail)],
        "头尾跨块保留、省略标记只写一次、整块被裁掉的文本块不进入结果"
    );
    assert!(
        super::prune_tool_content(&pruned).is_none(),
        "剪枝后的内容不再达到阈值"
    );
}

/// 摘要输出上限只受模型输出上限约束，不受生成请求的实测校正影响。
///
/// 生成请求的实测校正描述的是「带工具定义、以系统提示开头」的那份内容，只作用
/// 于生成请求的预算。这里让真实循环记录一次远高于估价的实测 usage，再比较实际
/// 发出的摘要请求预算。
#[test]
fn the_summary_budget_ignores_a_generation_request_correction() {
    use singularity_model::ModelUsage;

    let summary_budget_after_a_generation = |id: &str, reported: Option<u64>| -> u32 {
        let fixture = fixture_with(
            id,
            &[
                user("question"),
                assistant_with_call("one"),
                tool_result("one", &"z".repeat(2000)),
                user("keep this"),
            ],
        );
        let session = fixture.open_for_repair(id).unwrap();
        let usage = ModelUsage {
            total_tokens: reported.unwrap_or_default(),
            input_tokens: reported.unwrap_or_default(),
            usage_present: reported.is_some(),
            ..ModelUsage::default()
        };
        let scripted = Arc::new(ScriptedProvider::new([
            ScriptedAttempt::success_with_usage("answer", usage),
            ScriptedAttempt::success("checkpoint"),
        ]));
        let mut agent = agent(Arc::new(std::sync::Mutex::new(session)), scripted.clone());
        agent
            .run("follow up", &mut |_| {}, &CancellationToken::new())
            .unwrap();
        agent
            .compact_now(&mut |_| {}, &CancellationToken::new())
            .unwrap();
        let requests = scripted.requests();
        assert_eq!(requests.len(), 2, "one generation then one summary");
        requests[1].model_preferences.max_output_tokens.unwrap()
    };

    let unmeasured =
        summary_budget_after_a_generation("01914f6b-0000-7000-8000-0000000000fb", None);
    let measured =
        summary_budget_after_a_generation("01914f6b-0000-7000-8000-0000000000fc", Some(500_000));
    assert!(unmeasured > 0);
    assert_eq!(
        unmeasured, measured,
        "a generation-shape measurement must not shrink the summary budget"
    );
}
