#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 合法压缩切点与工具调用/结果配对保持测试。
//!
//! 核心不变量：
//! 1. 切点绝不切在工具结果（ToolResult）中间；ToolCall 与其配对的 ToolResult 必须落在切点同一侧。
//! 2. 保留预算跨过工具结果时，向前退到配对调用，允许保留同一轮次的后半段。
//! 3. 压缩条目记录在 step attempt 预分配的结果条目 id 上，ContextView 据此完整重建历史上下文。

use crate::request_execution::AttemptLedger;
use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    Provider,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

use super::{CompactionConfig, CompactionEngine, CompactionInput, CompactionOutcome};

fn input(entries: &[SessionEntry], tokens_before: u64) -> CompactionInput<'_> {
    CompactionInput {
        entries,
        tokens_before,
        keep_recent_tokens: 1,
        request: singularity_model::ModelTurnRequest::new(
            "summary",
            entries
                .iter()
                .flat_map(crate::session::context::entry_to_llm_messages)
                .collect(),
        ),
    }
}
use crate::message::{AgentMessage, AgentMessageRole, ContentBlock};
use crate::session::context::ContextView;
use crate::session::test_support::SessionFixture;
use crate::session::{CompactionEntry, SessionEntry, SessionError};

fn engine(summary: &str) -> CompactionEngine {
    let scripted = ScriptedProvider::new([ScriptedAttempt::success(summary)]);
    let model = scripted.model_configuration();
    CompactionEngine::new(Arc::new(scripted) as Arc<dyn Provider + Send + Sync>, model)
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::text(AgentMessageRole::User, text)
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::text(AgentMessageRole::Assistant, text)
}

fn assistant_with_call(call_id: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::ToolCall {
            id: call_id.to_string(),
            name: "read".to_string(),
            args: serde_json::json!({"path": "notes.txt"}),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
}

fn tool_result(call_id: &str, text: &str) -> AgentMessage {
    AgentMessage::ToolResult {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        tool_call_id: Some(call_id.to_string()),
        tool_name: Some("read".to_string()),
        is_error: Some(false),
        duration_ms: None,
        diff: None,
    }
}

fn message_text(entry: &SessionEntry) -> Option<String> {
    match entry {
        SessionEntry::Message { message, .. } => Some(message.content_text()),
        _ => None,
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

/// 触发阈值使用用户确认的 90%，保留预算使用窗口的 10%。
#[test]
fn should_compact_triggers_at_ninety_percent() {
    let config = CompactionConfig::default();
    assert!(!config.should_compact(899, 1000));
    assert!(config.should_compact(900, 1000));
    assert_eq!(config.retain_tokens(1000), 100);
    assert!(config.should_compact(901, 1000));
}

/// 切点绝不落在 ToolResult 上：保留预算被超大 ToolResult 跨过时，切点移到
/// 其后的合法条目，整个工具对落在摘要侧。
#[test]
fn cut_point_never_lands_on_a_tool_result() {
    let id = "01914f6b-0000-7000-8000-0000000000f1";
    let messages = [
        user(&"old question ".repeat(100)),
        assistant("old answer"),
        user("question with tool"),
        assistant_with_call("call-1"),
        tool_result("call-1", &"result payload ".repeat(400)),
        user("latest question"),
    ];
    let fixture = fixture_with(id, &messages);
    let session = fixture.open_read_only(id).unwrap();
    let entries = session.entries();

    let cut = CompactionEngine::find_cut_point(entries, 1);
    assert_ne!(
        message_text(&entries[cut]),
        None,
        "cut must land on a message entry"
    );
    assert!(
        !matches!(&entries[cut], SessionEntry::Message { message, .. }
            if message.role() == AgentMessageRole::ToolResult),
        "cut point must never land on a tool result"
    );
    assert_eq!(
        message_text(&entries[cut]),
        Some("latest question".to_string()),
        "the cut moves past the oversized tool result to the next legal entry"
    );
    assert_pairs_intact(&entries[cut..]);
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

    let mut attempts = crate::request_execution::RequestAccounting::default();
    let mut ledger = AttemptLedger::new(&writer, &mut attempts);
    let outcome = engine("## Goal\nkeep going")
        .compact(
            &mut ledger,
            input(&entries_before, 999),
            &mut crate::agent::AgentEvents::default(),
            &CancellationToken::new(),
        )
        .expect("compact");
    match outcome {
        CompactionOutcome::Compacted {
            first_kept_entry_id,
            tokens_before,
        } => {
            assert_eq!(tokens_before, 999);
            assert_eq!(
                first_kept_entry_id,
                entries_before[3].id(),
                "kept region starts at the paired tool call"
            );
        }
        CompactionOutcome::NotNeeded | CompactionOutcome::Pruned => {
            panic!("history exists, compaction must run")
        }
    }

    let session = crate::session::lock_writer(&writer);
    let last = session.entries().last().expect("compaction entry");
    assert_eq!(
        last.id(),
        ledger.result_entry_id(),
        "entry lands on the attempt ledger's reserved id"
    );
    assert!(matches!(
        last,
        SessionEntry::Compaction { compaction, .. }
            if compaction.summary.contains("## Goal")
    ));

    let view = ContextView::derive(&session).expect("context");
    assert!(
        matches!(&view.entries()[0], SessionEntry::Compaction { .. }),
        "the rebuilt view starts at the newest compaction node"
    );
    assert_eq!(
        view.entries()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message { .. }))
            .count(),
        2,
        "kept tail: assistant(tool call), tool result"
    );
    assert_pairs_intact(view.entries());
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
                usage: None,
                details: None,
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
    let mut attempts = crate::request_execution::RequestAccounting::default();
    let mut ledger = AttemptLedger::new(&writer, &mut attempts);
    let outcome = engine("summary")
        .compact(
            &mut ledger,
            input(&entries_before, 500),
            &mut crate::agent::AgentEvents::default(),
            &CancellationToken::new(),
        )
        .expect("compact call");
    assert_eq!(outcome, CompactionOutcome::NotNeeded);
    assert_eq!(
        crate::session::lock_writer(&writer).entries().len(),
        entries_before.len()
    );
}

/// 保留区内每个 ToolResult 都有配对的 ToolCall，反之亦然。
fn assert_pairs_intact(entries: &[SessionEntry]) {
    let mut calls: Vec<String> = Vec::new();
    let mut results: Vec<String> = Vec::new();
    for entry in entries {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };
        match message.role() {
            AgentMessageRole::Assistant => {
                for block in message.content() {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        calls.push(id.clone());
                    }
                }
            }
            AgentMessageRole::ToolResult => {
                if let Some(id) = message.tool_call_id() {
                    results.push(id.clone());
                }
            }
            AgentMessageRole::User => {}
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
fn retained_budget_moves_back_across_the_entire_tool_batch() {
    let id = "01914f6b-0000-7000-8000-0000000000f6";
    let mut call = assistant_with_call("one");
    if let AgentMessage::Assistant { content, .. } = &mut call {
        content.push(ContentBlock::ToolCall {
            id: "two".into(),
            name: "read".into(),
            args: serde_json::json!({"path":"b"}),
        });
    }
    let fixture = fixture_with(
        id,
        &[
            user("earlier"),
            call,
            tool_result("one", &"x".repeat(4000)),
            tool_result("two", "last"),
        ],
    );
    let session = fixture.open_read_only(id).unwrap();
    let cut = CompactionEngine::find_cut_point(session.entries(), 1);
    assert_eq!(cut, 1);
    assert_pairs_intact(&session.entries()[cut..]);
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
                usage: None,
                details: None,
            },
        )
        .unwrap();
    session
        .append_compaction_with_id(
            &uuid::Uuid::new_v4().to_string(),
            CompactionEntry {
                summary: "second checkpoint".into(),
                first_kept_entry_id: latest.clone(),
                usage: None,
                details: None,
            },
        )
        .unwrap();
    let view = ContextView::derive(&session).unwrap();
    assert_eq!(view.entries().len(), 2);
    assert_eq!(view.entries()[1].id(), latest);
    assert!(
        matches!(&view.entries()[0], SessionEntry::Compaction { compaction, .. } if compaction.summary == "second checkpoint")
    );
}

#[test]
fn summary_reuses_system_tools_and_native_messages_without_serializing_tool_output() {
    use singularity_model::{ModelMessage, ModelRole, ModelToolSchema, ModelTurnRequest};
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
    let entries = session.entries().to_vec();
    let writer = Arc::new(std::sync::Mutex::new(session));
    let scripted = Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "checkpoint",
    )]));
    let mut model = scripted.model_configuration();
    model.capabilities.max_context_tokens = Some(12_000);
    let mut engine = CompactionEngine::new(scripted.clone(), model);
    let mut request = ModelTurnRequest::new(
        "original",
        vec![ModelMessage::text(ModelRole::Developer, "system rules")],
    );
    request.messages.extend(
        entries
            .iter()
            .flat_map(crate::session::context::entry_to_llm_messages),
    );
    request.tools = vec![ModelToolSchema {
        name: "read".into(),
        description: "read files".into(),
        parameters_schema: serde_json::json!({"type":"object"}),
    }];
    let original = request.clone();
    let mut attempts = crate::request_execution::RequestAccounting::default();
    let mut ledger = AttemptLedger::new(&writer, &mut attempts);
    engine
        .compact(
            &mut ledger,
            CompactionInput {
                entries: &entries,
                keep_recent_tokens: 1,
                tokens_before: 5000,
                request,
            },
            &mut crate::agent::AgentEvents::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    let requests = scripted.requests();
    let output = requests[0].model_preferences.max_output_tokens.unwrap();
    assert!(output > 0 && output < super::DEFAULT_SUMMARY_MAX_TOKENS);
    assert_eq!(requests[0].tools, original.tools);
    assert_eq!(requests[0].messages[..4], original.messages[..4]);
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

#[test]
fn invalid_or_nonshrinking_summary_leaves_history_unchanged() {
    for summary in ["", " ", &"huge ".repeat(300)] {
        let id = "01914f6b-0000-7000-8000-0000000000f9";
        let fixture = fixture_with(id, &[user("short history"), user("last")]);
        let session = fixture.open_for_repair(id).unwrap();
        let entries = session.entries().to_vec();
        let writer = Arc::new(std::sync::Mutex::new(session));
        let mut attempts = crate::request_execution::RequestAccounting::default();
        let mut ledger = AttemptLedger::new(&writer, &mut attempts);
        assert!(
            engine(summary)
                .compact(
                    &mut ledger,
                    input(&entries, 500),
                    &mut crate::agent::AgentEvents::default(),
                    &CancellationToken::new()
                )
                .is_err()
        );
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

#[test]
fn unicode_pruning_preserves_head_tail_and_original_history_after_reopen() {
    let id = "01914f6b-0000-7000-8000-0000000000fa";
    let text = format!(
        "{}{}{}",
        "😀".repeat(4096),
        "中".repeat(4000),
        "尾".repeat(1024)
    );
    let fixture = fixture_with(id, &[assistant_with_call("one"), tool_result("one", &text)]);
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
    drop(session);
    let reopened = fixture.open_read_only(id).unwrap();
    assert_eq!(reopened.entries()[1], original);
    let view = ContextView::derive(&reopened).unwrap();
    let pruned = message_text(&view.entries()[1]).unwrap();
    assert!(pruned.starts_with(&"😀".repeat(4096)));
    assert!(pruned.ends_with(&"尾".repeat(1024)));
    assert!(!pruned.contains('中'));
    assert_pairs_intact(view.entries());
}

#[test]
fn pressure_keeps_usage_anchor_and_reprices_replacements() {
    let id = "01914f6b-0000-7000-8000-0000000000fb";
    let fixture = fixture_with(
        id,
        &[
            user(&"old ".repeat(500)),
            assistant("answer"),
            user("latest"),
        ],
    );
    let mut session = fixture.open_for_repair(id).unwrap();
    let mut view = ContextView::derive(&session).unwrap();
    view.record_usage(
        &singularity_model::ModelUsage {
            total_tokens: 6000,
            usage_present: true,
            ..Default::default()
        },
        0,
        100,
    );
    let before = view.request_tokens(100);
    let old_estimate: u64 = view
        .entries()
        .iter()
        .map(crate::session::context::entry_token_estimate)
        .sum();
    session
        .append_compaction_with_id(
            &uuid::Uuid::new_v4().to_string(),
            CompactionEntry {
                summary: "small checkpoint".into(),
                first_kept_entry_id: session.entries()[2].id().into(),
                usage: None,
                details: None,
            },
        )
        .unwrap();
    view.rebuild(&session).unwrap();
    let new_estimate: u64 = view
        .entries()
        .iter()
        .map(crate::session::context::entry_token_estimate)
        .sum();
    assert_eq!(
        view.request_tokens(100),
        before - (old_estimate - new_estimate)
    );
    assert!(view.request_tokens(100) > new_estimate + 100);
}
