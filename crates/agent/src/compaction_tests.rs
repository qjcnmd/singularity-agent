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
        crate::tools::ToolRegistrySnapshot::new(),
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

fn user(text: &str) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn assistant(text: &str) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        stop_reason: None,
        provider_reasoning_replay: None,
    }
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
        .compact_now(
            &mut crate::agent::AgentEvents::default(),
            &CancellationToken::new(),
        )
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
    let visible: Vec<_> = view
        .entries(&session)
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
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
    let provider = Arc::new(ScriptedProvider::ok("summary"));
    let outcome = agent(writer.clone(), provider.clone())
        .compact_now(
            &mut crate::agent::AgentEvents::default(),
            &CancellationToken::new(),
        )
        .expect("compact call");
    assert_eq!(outcome, CompactionOutcome::NotNeeded);
    assert!(provider.requests().is_empty());
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
        match message {
            AgentMessage::Assistant { content, .. } => {
                for block in content {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        calls.push(id.clone());
                    }
                }
            }
            AgentMessage::ToolResult { tool_call_id, .. } => {
                if let Some(id) = tool_call_id {
                    results.push(id.clone());
                }
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
    let visible: Vec<_> = view
        .entries(&session)
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
    assert_eq!(visible.as_slice().len(), 2);
    assert_eq!(visible.as_slice()[1].id(), latest);
    assert!(
        matches!(&visible.as_slice()[0], SessionEntry::Compaction { compaction, .. } if compaction.summary == "second checkpoint")
    );
}

#[test]
fn summary_reuses_system_tools_and_native_messages_without_serializing_tool_output() {
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
    let entries = session.entries().to_vec();
    let writer = Arc::new(std::sync::Mutex::new(session));
    let scripted = Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "checkpoint",
    )]));
    let mut original = vec![ModelMessage::text(ModelRole::Developer, "system rules")];
    original.extend(
        entries[..3]
            .iter()
            .filter_map(crate::session::context::entry_to_llm_message),
    );
    agent(writer, scripted.clone())
        .compact_now(
            &mut crate::agent::AgentEvents::default(),
            &CancellationToken::new(),
        )
        .unwrap();
    let requests = scripted.requests();
    let output = requests[0].model_preferences.max_output_tokens.unwrap();
    assert!(output > 0 && output < super::DEFAULT_SUMMARY_MAX_TOKENS);
    assert_eq!(
        requests[0].tools,
        crate::tools::ToolRegistrySnapshot::new().provider_schemas()
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

#[test]
fn invalid_or_nonshrinking_summary_leaves_history_unchanged() {
    for summary in ["", " ", &"huge ".repeat(300)] {
        let id = "01914f6b-0000-7000-8000-0000000000f9";
        let fixture = fixture_with(id, &[user("short history"), user("last")]);
        let session = fixture.open_for_repair(id).unwrap();
        let entries = session.entries().to_vec();
        let writer = Arc::new(std::sync::Mutex::new(session));
        assert!(
            agent(writer.clone(), Arc::new(ScriptedProvider::ok(summary)))
                .compact_now(
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
    let visible: Vec<_> = view
        .entries(&reopened)
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .collect();
    let pruned = message_text(&visible.as_slice()[1]).unwrap();
    assert!(pruned.starts_with(&"😀".repeat(4096)));
    assert!(pruned.ends_with(&"尾".repeat(1024)));
    assert!(!pruned.contains('中'));
    assert_pairs_intact(visible.as_slice());
}
