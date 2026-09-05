#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 合法压缩切点与工具调用/结果配对保持测试。
//!
//! 核心不变量：
//! 1. 切点绝不切在工具结果（ToolResult）中间；ToolCall 与其配对的 ToolResult 必须落在切点同一侧。
//! 2. 当保留预算被 ToolResult 跨过且后续无合法切点时，回退至所属轮次起点。
//! 3. 压缩条目记录在 step attempt 预分配的结果条目 id 上，ContextView 据此完整重建历史上下文。

use std::sync::Arc;

use singularity_core::CancellationToken;
use singularity_model::{
    Provider,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

use super::{CompactionConfig, CompactionEngine, CompactionError, CompactionOutcome};
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

/// 触发阈值是严格大于「窗口 − 预留」：恰好等于不压缩。
#[test]
fn should_compact_triggers_only_above_threshold() {
    let engine = engine("summary");
    let config = CompactionConfig {
        reserve_tokens: 100,
        keep_recent_tokens: 50,
    };
    assert!(!engine.should_compact(900, 1000, &config));
    assert!(engine.should_compact(901, 1000, &config));
}

/// 切点绝不落在 ToolResult 上：保留预算被超大 ToolResult 跨过时，切点移到
/// 其后的合法条目，整个工具对落在摘要侧。
#[test]
fn cut_point_never_lands_on_a_tool_result() {
    let id = "01914f6b-0000-7000-8000-0000000000f1";
    let messages = [
        user("old question"),
        assistant("old answer"),
        user("question with tool"),
        assistant_with_call("call-1"),
        tool_result("call-1", &"result payload ".repeat(400)),
        user("latest question"),
    ];
    let fixture = fixture_with(id, &messages);
    let session = fixture.open_read_only(id).unwrap();
    let entries = session.entries();

    let engine = engine("summary");
    let cut = engine.find_cut_point_in_range(entries, 0, entries.len(), 1);
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
        user("old question"),
        assistant("old answer"),
        user("question with tool"),
        assistant_with_call("call-1"),
        tool_result("call-1", &"result payload ".repeat(400)),
    ];
    let fixture = fixture_with(id, &messages);
    let session = fixture.open_for_repair(id).unwrap();
    let entries_before: Vec<SessionEntry> = session.entries().to_vec();
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));

    let config = CompactionConfig {
        reserve_tokens: 100_000,
        keep_recent_tokens: 1,
    };
    let mut attempts = 0u32;
    let mut ledger = crate::agent::AttemptLedger::new(&writer, &mut attempts);
    let outcome = engine("## Goal\nkeep going")
        .compact(
            &mut ledger,
            &entries_before,
            &config,
            999,
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
                entries_before[2].id(),
                "kept region starts at the turn-start fallback cut"
            );
        }
        CompactionOutcome::NotNeeded => panic!("history exists, compaction must run"),
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
        3,
        "kept tail: user, assistant(tool call), tool result"
    );
    assert_pairs_intact(view.entries());
}

#[test]
fn compact_rejects_a_missing_previous_anchor_before_requesting_a_summary() {
    let id = "01914f6b-0000-7000-8000-0000000000f5";
    let fixture = fixture_with(id, &[user("older"), assistant("newer")]);
    let session = fixture.open_for_repair(id).unwrap();
    let mut entries = session.entries().to_vec();
    entries.insert(
        0,
        SessionEntry::Compaction {
            id: "invalid-compaction".to_string(),
            timestamp: "2026-09-01T00:00:00.000Z".to_string(),
            compaction: CompactionEntry {
                summary: "summary".to_string(),
                first_kept_entry_id: "missing-anchor".to_string(),
                usage: None,
                details: None,
            },
        },
    );
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));
    let mut attempts = 0u32;
    let mut ledger = crate::agent::AttemptLedger::new(&writer, &mut attempts);

    let error = engine("must not be requested")
        .compact(
            &mut ledger,
            &entries,
            &CompactionConfig::default(),
            500,
            &CancellationToken::new(),
        )
        .expect_err("a missing anchor is corrupt ledger state");
    assert!(matches!(
        error,
        CompactionError::Session(SessionError::LedgerCorrupt { reason, .. })
            if reason == "invalid_compaction_anchor"
    ));
    assert_eq!(
        attempts, 0,
        "validation happens before any provider attempt"
    );
}

/// 无历史可摘要时 NotNeeded，不写任何条目（无出站请求，也无 step attempt）。
#[test]
fn compact_without_summarizable_history_is_not_needed() {
    let id = "01914f6b-0000-7000-8000-0000000000f4";
    let fixture = fixture_with(id, &[user("only message")]);
    let session = fixture.open_for_repair(id).unwrap();
    let entries_before: Vec<SessionEntry> = session.entries().to_vec();
    let writer: crate::session::SessionWriter = std::sync::Arc::new(std::sync::Mutex::new(session));
    let config = CompactionConfig {
        reserve_tokens: 100,
        keep_recent_tokens: 1,
    };
    let mut attempts = 0u32;
    let mut ledger = crate::agent::AttemptLedger::new(&writer, &mut attempts);
    let outcome = engine("summary")
        .compact(
            &mut ledger,
            &entries_before,
            &config,
            500,
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
