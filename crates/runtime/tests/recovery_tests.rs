#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! runner seam 的 durable-before-publish 与恢复验证（T016/T017 端到端）。
//!
//! 用进程停止钩子把 turn 精确停在「首个 provider 请求已发出」处：此刻
//! `operation_started` 与 `step_attempt` 必须已落盘，而终态记录尚未产生；
//! 放行后 turn 收敛，终态记录才出现。物理行序即 durable 顺序。

use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::runner::TurnRunner;
use crate::test_support::{GatedProvider, provider_snapshot, temp_sessions};
use singularity_agent::session::{
    LedgerRecord, SessionManager, open_operations, reduce_operations,
};
use singularity_model::Provider;

#[test]
fn operation_start_is_durable_before_the_provider_call_and_terminal_after() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");

    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);

    let runner = Arc::new(
        TurnRunner::new(sessions.clone(), provider_snapshot())
            .with_provider_override(gate as Arc<dyn Provider + Send + Sync>),
    );
    let thread = ThreadCatalog::new(&runner)
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id.clone();
    let conversation = Conversation::new(runner, thread);

    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event| {};
            conversation.run_turn("go", &mut sink)
        })
    };

    // turn 停在 provider 边界：起始记录已 durable，终态尚未产生。
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("turn reaches the provider");
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let mid = SessionManager::open_existing_read_only(&path).expect("read-only open mid-turn");
    let operations = reduce_operations(mid.entries());
    let open = open_operations(&operations);
    assert_eq!(
        open.len(),
        1,
        "exactly one open run while the turn is executing"
    );
    assert!(
        open[0].turn_id.is_some(),
        "a run operation carries its turn id"
    );
    assert!(
        mid.ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::StepAttempt { .. })),
        "the assistant step attempt is durable before the provider call"
    );
    assert!(
        !mid.ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { .. })),
        "no terminal record is published before the turn converges"
    );
    drop(mid);

    // 放行：provider 返回，turn 收敛，终态记录落盘。
    release_tx.send(()).expect("release the gate");
    let outcome = worker.join().expect("worker").expect("turn ok");

    let after = SessionManager::open_existing_read_only(&path).expect("reopen");
    let operations = reduce_operations(after.entries());
    assert!(open_operations(&operations).is_empty(), "run converged");
    let finished_turn_id = after
        .ledger_records()
        .iter()
        .find_map(|record| match record {
            LedgerRecord::OperationFinished {
                turn_id,
                outcome: singularity_protocol::TurnStatus::Completed,
                ..
            } => turn_id.clone(),
            _ => None,
        })
        .expect("a completed terminal record is durable");
    assert_eq!(
        finished_turn_id, outcome.turn_id,
        "the durable terminal record is the same turn the runner reported"
    );
}

/// T036 [US3]：进程在终态提交前死亡。durable 前缀（operation 起始、声明了
/// never-replay 工具的 assistant、tool_started 而结果未落盘）经 `resume_thread`
/// 从 ledger 事实收敛：先补模型可见失败闭合配对，再落唯一 interrupted 终态；
/// 绝不重放副作用；收敛后的 Thread 可直接继续新 turn。
#[test]
fn crash_before_terminal_commit_converges_from_ledger_on_resume() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let runner = Arc::new(TurnRunner::new(sessions.clone(), provider_snapshot()));
    let thread = ThreadCatalog::new(&runner)
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));

    // 进程死亡时刻的 durable 前缀（写者 drop = 锁释放，终态未落盘）。
    let mut writer = SessionManager::open_existing_with_access(
        &path,
        runner.coordinator(),
        &thread_id,
        singularity_agent::session::SessionAccess::Append,
    )
    .expect("writer open");
    writer
        .append_record(LedgerRecord::OperationStarted {
            operation_id: "op-crash".to_string(),
            kind: singularity_agent::session::OperationKind::Run,
            turn_id: Some("turn-crash".to_string()),
            intent: singularity_agent::session::OperationIntent::Run {
                model: crate::test_support::test_model_configuration(),
                input: String::new(),
            },
        })
        .expect("operation started");
    writer
        .append_message(singularity_agent::message::AgentMessage::Assistant {
            content: vec![singularity_agent::message::ContentBlock::ToolCall {
                id: "call-1".to_string(),
                name: "edit".to_string(),
                args: serde_json::json!({"path": "x.txt", "oldString": "a", "newString": "b"}),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
        })
        .expect("assistant with tool call");
    writer
        .append_record(LedgerRecord::ToolStarted {
            operation_id: "op-crash".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "edit".to_string(),
            source_order: 0,
            effective_args: serde_json::json!({"path": "x.txt", "oldString": "a", "newString": "b"}),
            result_entry_id: "result-call-1".to_string(),
            replay: singularity_agent::session::ToolReplayClass::Never,
        })
        .expect("tool started");
    drop(writer);

    let resumed = ThreadCatalog::new(&runner)
        .resume_thread(&thread_id)
        .expect("resume converges the open operation");
    assert_eq!(
        resumed.last_turn_status,
        Some(singularity_protocol::TurnStatus::Interrupted),
        "the crashed turn projects as interrupted from ledger facts"
    );

    let session = SessionManager::open_existing_read_only(&path).expect("reopen");
    let repair_at = session
        .entries()
        .iter()
        .position(|entry| {
            matches!(entry, singularity_agent::session::SessionEntry::Message { message, .. }
                if message.content_text() == singularity_agent::session::REPAIR_UNKNOWN_OUTCOME)
        })
        .expect("the unresolved tool call closes with a model-visible failure");
    let finished_at = session
        .entries()
        .iter()
        .position(|entry| {
            matches!(entry,
                singularity_agent::session::SessionEntry::Record {
                    record: LedgerRecord::OperationFinished {
                        operation_id,
                        outcome: singularity_protocol::TurnStatus::Interrupted,
                        ..
                    },
                    ..
                } if operation_id == "op-crash")
        })
        .expect("exactly one interrupted terminal record converges the operation");
    assert!(
        repair_at < finished_at,
        "the repair record is durable before the recovered terminal outcome"
    );
    let started_tools = session
        .ledger_records()
        .iter()
        .filter(|record| matches!(record, LedgerRecord::ToolStarted { .. }))
        .count();
    assert_eq!(
        started_tools, 1,
        "convergence never starts a second tool execution"
    );
    let terminals = session
        .ledger_records()
        .iter()
        .filter(|record| matches!(record, LedgerRecord::OperationFinished { .. }))
        .count();
    assert_eq!(
        terminals, 1,
        "exactly one terminal outcome for the crashed turn"
    );
    drop(session);

    // 收敛后的 Thread 在同一条执行链上继续新 turn。
    let provider = Arc::new(singularity_model::test_support::ScriptedProvider::ok(
        "recovered continuation",
    ));
    let runner = Arc::new(
        TurnRunner::new(sessions, provider_snapshot())
            .with_provider_override(provider as Arc<dyn singularity_model::Provider + Send + Sync>),
    );
    let conversation = Conversation::new(runner, resumed);
    let mut sink = |_event| {};
    let outcome = conversation
        .run_turn("continue after crash", &mut sink)
        .expect("the next turn runs on the converged ledger");
    assert_eq!(
        outcome.turn_status,
        singularity_protocol::TurnStatus::Completed
    );
}

/// T036 [US3]：撕裂尾部与未终结 operation 同时存在。打开写路径先丢弃不完整
/// 尾行（durable 前缀保持完整），再按 ledger 事实收敛未终结 operation；
/// ContextView 只由完整条目派生。
#[test]
fn torn_tail_is_repaired_before_recovery_decisions() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let runner = Arc::new(TurnRunner::new(sessions.clone(), provider_snapshot()));
    let thread = ThreadCatalog::new(&runner)
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));

    let mut writer = SessionManager::open_existing_with_access(
        &path,
        runner.coordinator(),
        &thread_id,
        singularity_agent::session::SessionAccess::Append,
    )
    .expect("writer open");
    writer
        .append_record(LedgerRecord::OperationStarted {
            operation_id: "op-torn".to_string(),
            kind: singularity_agent::session::OperationKind::Run,
            turn_id: Some("turn-torn".to_string()),
            intent: singularity_agent::session::OperationIntent::Run {
                model: crate::test_support::test_model_configuration(),
                input: String::new(),
            },
        })
        .expect("operation started");
    writer
        .append_message(singularity_agent::message::AgentMessage::text(
            singularity_agent::message::AgentMessageRole::User,
            "question before the crash",
        ))
        .expect("user message");
    drop(writer);

    // 进程在写入中途死亡：最后一行没有换行符也不是合法 JSON。
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for torn tail")
        .write_all(b"{\"type\":\"message\",\"id\":\"__incomplete_tail__")
        .expect("write torn tail");

    let resumed = ThreadCatalog::new(&runner)
        .resume_thread(&thread_id)
        .expect("resume repairs the tail and converges the operation");
    assert_eq!(
        resumed.last_turn_status,
        Some(singularity_protocol::TurnStatus::Interrupted)
    );

    let content = std::fs::read_to_string(&path).expect("read file");
    assert!(content.ends_with('\n'), "the file ends with a full line");
    assert!(
        !content.contains("__incomplete_tail__"),
        "the incomplete tail is dropped, never parsed as a fact"
    );

    let session = SessionManager::open_existing_read_only(&path).expect("reopen");
    assert!(
        session.entries().iter().any(|entry| {
            matches!(entry, singularity_agent::session::SessionEntry::Message { message, .. }
                if message.content_text() == "question before the crash")
        }),
        "the durable prefix survives the tail repair"
    );
    let view =
        singularity_agent::session::context::ContextView::derive(&session).expect("derive context");
    assert_eq!(
        view.entries()
            .iter()
            .filter(|entry| matches!(
                entry,
                singularity_agent::session::SessionEntry::Message { .. }
            ))
            .count(),
        1,
        "the context view derives only from complete durable entries"
    );
}

/// T036 [US3]：终态已提交后重启——恢复不猜测、不补写：resume 后 ledger 与
/// 重启前逐条目一致，终态仍恰好一条 completed。
#[test]
fn committed_terminal_survives_reopen_without_repair() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let provider = Arc::new(singularity_model::test_support::ScriptedProvider::ok(
        "finished work",
    ));
    let runner = Arc::new(
        TurnRunner::new(sessions.clone(), provider_snapshot())
            .with_provider_override(provider as Arc<dyn singularity_model::Provider + Send + Sync>),
    );
    let thread = ThreadCatalog::new(&runner)
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let thread_id = thread.thread_id.clone();
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let conversation = Conversation::new(Arc::clone(&runner), thread);
    let mut sink = |_event| {};
    conversation
        .run_turn("do the work", &mut sink)
        .expect("turn completes");

    let before = SessionManager::open_existing_read_only(&path).expect("reopen before resume");
    let entries_before = before.entries().len();
    let ids_before: Vec<String> = before
        .entries()
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    drop(before);

    let resumed = ThreadCatalog::new(&runner)
        .resume_thread(&thread_id)
        .expect("resume a cleanly finished thread");
    assert_eq!(
        resumed.last_turn_status,
        Some(singularity_protocol::TurnStatus::Completed)
    );

    let after = SessionManager::open_existing_read_only(&path).expect("reopen after resume");
    assert_eq!(
        after.entries().len(),
        entries_before,
        "a committed terminal outcome is never re-repaired"
    );
    let ids_after: Vec<String> = after
        .entries()
        .iter()
        .map(|entry| entry.id().to_string())
        .collect();
    assert_eq!(ids_before, ids_after, "no entry is rewritten or appended");
    let terminals = after
        .ledger_records()
        .iter()
        .filter(|record| matches!(record, LedgerRecord::OperationFinished { .. }))
        .count();
    assert_eq!(terminals, 1);
}
