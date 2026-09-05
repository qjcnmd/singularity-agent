#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 核心执行路径的 ledger 与事件一致性测试。
//!
//! 完整经历 read → modify → validate 工具执行后，验证实时事件流与持久化 ledger
//! 严格描述同一事实：保持相同 turn id、工具批次顺序、终态与 token usage；
//! 且持久化记录严格先于对应事件发布。

use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::events::TurnEvent;
use crate::objects::TurnStatus;
use crate::runner::TurnRunner;
use crate::test_support::{provider_snapshot, temp_sessions};
use singularity_agent::message::AgentMessageRole;
use singularity_agent::session::{LedgerRecord, OperationKind, SessionEntry, SessionManager};
use singularity_model::Provider;
use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};

/// 收集一次旅程的全部事件。
#[derive(Default)]
struct Recorder {
    events: Vec<TurnEvent>,
}

impl Recorder {
    fn sink(&mut self) -> impl FnMut(TurnEvent) + '_ {
        move |event| self.events.push(event)
    }
}

#[test]
fn shared_journey_events_match_ledger_facts_in_order() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    std::fs::write(home.path().join("notes.txt"), "alpha\n").expect("seed file");

    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call(
            "call-read",
            "read",
            serde_json::json!({"path": "notes.txt"}),
        ),
        ScriptedAttempt::tool_call(
            "call-edit",
            "edit",
            serde_json::json!({"path": "notes.txt", "oldString": "alpha", "newString": "beta"}),
        ),
        ScriptedAttempt::tool_call(
            "call-verify",
            "read",
            serde_json::json!({"path": "notes.txt"}),
        ),
        ScriptedAttempt::success("task complete"),
    ]));
    let runner = Arc::new(
        TurnRunner::new(sessions.clone(), provider_snapshot())
            .with_provider_override(provider as Arc<dyn Provider + Send + Sync>),
    );
    let thread = ThreadCatalog::new(&runner)
        .create_thread(home.path().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id.clone();
    let conversation = Conversation::new(runner, thread).expect("open conversation");

    let mut recorder = Recorder::default();
    let outcome = conversation
        .run_turn("read, modify and validate notes.txt", &mut recorder.sink())
        .expect("journey turn completes");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(outcome.final_text, "task complete");
    assert_eq!(
        std::fs::read_to_string(home.path().join("notes.txt")).expect("read back"),
        "beta\n",
        "the modify step really touched the workspace"
    );

    // —— 事件面：单一 turn、工具按 source order、恰好一个终态 ——
    let methods: Vec<&'static str> = recorder.events.iter().map(TurnEvent::method).collect();
    assert_eq!(methods.first(), Some(&"turn/started"));
    assert_eq!(methods.last(), Some(&"turn/completed"));
    assert_eq!(
        methods.iter().filter(|m| **m == "turn/completed").count(),
        1,
        "exactly one terminal event"
    );
    let event_tool_order: Vec<String> = recorder
        .events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolExecutionStart { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        event_tool_order,
        vec!["call-read", "call-edit", "call-verify"],
        "tool events follow the model source order"
    );
    let event_turn_id = recorder
        .events
        .iter()
        .find_map(|event| match event {
            TurnEvent::TurnStarted { turn, .. } => Some(turn.turn_id.clone()),
            _ => None,
        })
        .expect("turn/started carries the turn id");

    // —— ledger 面：同一 turn、同一批次、同一终态 ——
    let session =
        SessionManager::open_existing_read_only(&sessions.join(format!("{thread_id}.jsonl")))
            .expect("reopen");
    let records: Vec<LedgerRecord> = session.ledger_records();

    let started = records
        .iter()
        .find_map(|record| match record {
            LedgerRecord::OperationStarted { kind, turn_id, .. } if *kind == OperationKind::Run => {
                Some(turn_id.clone())
            }
            _ => None,
        })
        .expect("the run operation is durable");
    assert_eq!(
        started.as_deref(),
        Some(event_turn_id.as_str()),
        "the durable run operation and the live events describe the same turn"
    );

    // 每个已启动工具都有配对的模型可见结果，且保持 source order。
    let result_order: Vec<String> = session
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::ToolResult =>
            {
                message.tool_call_id().cloned()
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        result_order,
        vec!["call-read", "call-edit", "call-verify"],
        "tool results are durable in assistant source order"
    );

    // 终态：恰好一条 operation_finished，status/usage/truncated 与终态事件一致。
    let terminals: Vec<&LedgerRecord> = records
        .iter()
        .filter(|record| matches!(record, LedgerRecord::OperationFinished { .. }))
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one durable terminal outcome");
    let LedgerRecord::OperationFinished {
        turn_id,
        outcome: durable_status,
        usage: durable_usage,
        truncated,
        ..
    } = terminals[0]
    else {
        unreachable!("filtered to OperationFinished")
    };
    assert_eq!(turn_id.as_deref(), Some(event_turn_id.as_str()));
    assert_eq!(*durable_status, TurnStatus::Completed);
    assert!(!truncated);
    let terminal_event_usage = recorder
        .events
        .iter()
        .find_map(|event| match event {
            TurnEvent::TurnCompleted { turn } => turn.usage.clone(),
            _ => None,
        })
        .expect("terminal event carries usage");
    assert_eq!(
        Some(&terminal_event_usage),
        durable_usage.as_ref(),
        "the terminal event's usage is the durable terminal record's usage"
    );

    // —— durable-before-publish：物理行序即落盘顺序 ——
    let entry_index_of = |predicate: &dyn Fn(&SessionEntry) -> bool| {
        session
            .entries()
            .iter()
            .position(predicate)
            .expect("entry exists")
    };
    let started_at = entry_index_of(&|entry| {
        matches!(
            entry,
            SessionEntry::Record {
                record: LedgerRecord::OperationStarted { .. },
                ..
            }
        )
    });
    let first_tool_at = entry_index_of(&|entry| {
        matches!(
            entry,
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::ToolResult
        )
    });
    let final_text_at = entry_index_of(&|entry| {
        matches!(entry,
            SessionEntry::Message { message, .. }
                if message.role() == AgentMessageRole::Assistant
                    && message.content_text() == "task complete")
    });
    let finished_at = entry_index_of(&|entry| {
        matches!(
            entry,
            SessionEntry::Record {
                record: LedgerRecord::OperationFinished { .. },
                ..
            }
        )
    });
    assert!(
        started_at < first_tool_at,
        "operation is durable before the first tool step"
    );
    assert!(
        first_tool_at < final_text_at,
        "tool steps precede the final assistant text"
    );
    assert!(
        final_text_at < finished_at,
        "the final assistant text is durable before the terminal record"
    );
    assert_eq!(
        finished_at,
        session.entries().len() - 1,
        "the terminal record is the last durable fact of the turn"
    );
}
