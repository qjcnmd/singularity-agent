#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 运行期控制输入在竞态和失败边界的持久化测试。
//!
//! GatedProvider 在模型边界固定运行中的窗口，复现控制输入与失败的先后顺序。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crate::ThreadCatalog;
use crate::test_support::{GatedProvider, SessionsFixture, conversation_with, input_sequence};
use crate::{Conversation, ConversationControlError};
use singularity_agent::session::{LedgerRecord, SessionData, SessionEntry};
use singularity_model::{
    ModelErrorKind, ModelRole, Provider,
    test_support::{ScriptedAttempt, ScriptedProvider},
};
use singularity_protocol::TurnEvent;
use singularity_protocol::TurnStatus;

/// 在「turn 已注册、模型未返回」的窗口内执行控制注入，随后释放收敛。
/// 注入必须在 join 前完成：借用协调器的闭包在 worker 存续期内调用。
fn run_with_control_window(
    gate: &Arc<GatedProvider>,
    started_rx: Receiver<()>,
    conversation: &Arc<Conversation>,
    goal: &str,
    inject: impl FnOnce(&Arc<Conversation>),
) -> (crate::TurnOutcome, Vec<TurnEvent>) {
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let worker_conversation = Arc::clone(conversation);
    let control_conversation = Arc::clone(conversation);
    let goal = goal.to_string();
    let worker = std::thread::spawn(move || {
        let mut events = Vec::new();
        let outcome = crate::test_support::run_async(
            worker_conversation.run_turn(&goal, &mut |event| events.push(event)),
        );
        (outcome, events)
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the model");
    inject(&control_conversation);
    let _ = release_tx.send(());
    let (outcome, events) = worker.join().expect("worker");
    (outcome.expect("every control run converges to a trusted terminal outcome"), events)
}

/// 单条排队输入可撤回；多次 steer 按接受顺序进入下一份模型请求。
#[test]
fn controls_preserve_input_order_and_withdrawal() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("c1", "read", serde_json::json!({"path": "missing-a"})),
        ScriptedAttempt::success("adjusted course"),
        ScriptedAttempt::success("f1 done"),
    ]));
    let (gate, started_rx) = GatedProvider::new(script.clone() as Arc<dyn Provider + Send + Sync>);
    let (conversation, path) = conversation_with(&fixture, Arc::clone(&gate) as _, None);
    let (outcome, _) = run_with_control_window(&gate, started_rx, &conversation, "initial goal", |c| {
        c.steer("steer left").unwrap();
        c.submit_follow_up("withdrawn").unwrap();
        c.steer("steer right").unwrap();
        assert!(matches!(c.submit_follow_up("f2"), Err(ConversationControlError::PendingInputExists)));
        let queued = c.snapshot().pending_input.unwrap();
        assert!(
            c.withdraw_follow_up(&queued.control_id).is_ok(),
            "the queued input is withdrawable before start"
        );
        c.submit_follow_up("f1").unwrap();
    });
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    assert!(conversation.snapshot().pending_input.is_none());
    assert!(
        !SessionData::open(&path).unwrap().entries().iter().any(
            |entry| matches!(entry, SessionEntry::Message { message, .. } if message.content_text() == "withdrawn")
        )
    );
    let requests = script.requests();
    assert_eq!(requests.len(), 3, "two model steps + one queued turn");
    assert_eq!(input_sequence(&requests[2..]), ["f1"], "the queued input runs as its own turn");
    let second_request_users: Vec<String> = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == ModelRole::User)
        .map(|message| message.content.clone())
        .collect();
    let left = second_request_users
        .iter()
        .position(|text| text.contains("steer left"))
        .expect("first steer precedes the next assistant response");
    let right = second_request_users
        .iter()
        .position(|text| text.contains("steer right"))
        .expect("second steer precedes the next assistant response");
    assert!(left < right, "injection follows acceptance order");
}

/// 已接受的停止与真实失败同时存在：终态保持真实失败原因并记录
/// user_stopped，但链条不再启动下一条队列输入，后续输入原样留队。
#[test]
fn an_accepted_stop_stops_the_chain_even_when_the_turn_fails() {
    use singularity_protocol::{ProviderAttemptStatus, TurnFailureCause};

    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([ScriptedAttempt::failure_kind(
        ModelErrorKind::AuthError,
        "invalid api key",
    )]));
    let (conversation, path) =
        conversation_with(&fixture, Arc::clone(&script) as Arc<dyn Provider + Send + Sync>, None);
    let mut queued = None;
    let outcome = {
        let conversation = Arc::clone(&conversation);
        crate::test_support::run_async(conversation.run_turn("go", &mut |event| {
            if let TurnEvent::ProviderAttempt { observation, .. } = &event
                && observation.status == ProviderAttemptStatus::Error
            {
                // 真实失败已经确定、终态尚未裁决：此时接受停止。
                conversation
                    .submit_follow_up("must stay queued")
                    .expect("a queued input is accepted before the terminal");
                queued = conversation.snapshot().pending_input;
                conversation.abort().expect("the stop is accepted");
            }
        }))
        .expect("a real failure still converges to a trusted terminal")
    };

    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    let session = SessionData::open(&path).expect("reopen stopped turn");
    assert!(
        session
            .ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { user_stopped: true, .. }))
    );
    assert_eq!(
        outcome.error.as_ref().map(|error| error.cause),
        Some(TurnFailureCause::ProviderAuth),
        "the real failure reason is preserved"
    );
    assert_eq!(script.requests().len(), 1, "an accepted stop never starts the next queued turn");
    let pending = conversation.snapshot().pending_input.unwrap();
    assert_eq!(pending.control_id, queued.as_ref().unwrap().control_id);
}

/// 接受停止同时关闭本轮注入窗口：其后的 steer 与 send-now 都被拒绝，
/// 原队列项保持原位（它们属于下一轮，不属于本轮的取消集合）。
#[test]
fn an_accepted_stop_closes_the_injection_window_without_losing_queued_input() {
    let fixture = SessionsFixture::new();
    let (gate, started_rx) = GatedProvider::new(Arc::new(ScriptedProvider::ok("done")));
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let (conversation, path) = conversation_with(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            crate::test_support::run_async(conversation.run_turn("initial", &mut sink))
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the provider");
    conversation.submit_follow_up("kept for the next turn").expect("queue a follow-up");
    let queued = conversation.snapshot().pending_input.unwrap();
    conversation.abort().expect("stop the running turn");

    assert!(matches!(conversation.steer("late steer"), Err(ConversationControlError::NotRunning)));
    assert!(matches!(
        conversation.promote_pending(&queued.control_id),
        Err(ConversationControlError::NotRunning)
    ));
    let pending = conversation.snapshot().pending_input.unwrap();
    assert_eq!(pending.control_id, queued.control_id);
    assert_eq!(conversation.phase(), singularity_protocol::SessionPhase::Stopping);

    let _ = release_tx.send(());
    let outcome = worker.join().expect("worker").expect("interruption converges durably");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    let session = SessionData::open(&path).expect("reopen stopped turn");
    assert!(
        session
            .ledger_records()
            .iter()
            .any(|record| matches!(record, LedgerRecord::OperationFinished { user_stopped: true, .. }))
    );
    assert!(
        conversation.snapshot().pending_input.is_some(),
        "the stopped turn leaves the queued follow-up for the next explicit input"
    );
}

/// 执行失败不会把 steer 变回普通排队消息，原先的后续输入保持原位。
#[test]
fn a_failed_turn_leaves_only_the_explicitly_queued_input() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::failure_kind(ModelErrorKind::AuthError, "request rejected"),
        ScriptedAttempt::success("continued"),
    ]));
    let (gate, started_rx) = GatedProvider::new(script.clone() as Arc<dyn Provider + Send + Sync>);
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let (conversation, _path) = conversation_with(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            crate::test_support::run_async(conversation.run_turn("initial", &mut sink))
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the provider");
    conversation.submit_follow_up("first accepted").expect("queue the follow-up first");
    let follow_up = conversation.snapshot().pending_input.unwrap();
    conversation.steer("second accepted").expect("steer second");
    let _ = release_tx.send(());
    let outcome = worker.join().expect("worker").expect("a model error has a saved terminal");
    assert_eq!(outcome.turn_status, TurnStatus::Failed);

    let pending = conversation.snapshot().pending_input.unwrap();
    assert_eq!(pending.text, "first accepted");
    assert_eq!(pending.control_id, follow_up.control_id);
    assert_eq!(script.requests().len(), 1, "failure stops the current execution");

    let compaction = conversation.reserve_compaction().unwrap();
    let edited = conversation.take_follow_up(&pending.control_id).unwrap();
    assert_eq!(edited.text, "first accepted");
    drop(compaction);
    crate::test_support::run_async(conversation.run_turn("continue", &mut |_| {})).unwrap();
    let requests = script.requests();
    let users = requests[1]
        .messages
        .iter()
        .filter(|message| message.role == ModelRole::User)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    assert!(users.ends_with(&["initial", "second accepted", "continue"]));
    assert_eq!(users.iter().filter(|text| **text == "second accepted").count(), 1);
}

/// 失败 turn 的细节随 operation 终态落盘，历史重读带同一份错误概念：后续成功轮次和
/// 重新打开目录都不改写较早的失败原因，不依赖 runtime 最近一次的文本。
#[test]
fn a_failed_turn_keeps_its_detail_across_reload_and_a_later_success() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::failure_kind(ModelErrorKind::InvalidRequest, "invalid model request"),
        ScriptedAttempt::success("second response"),
    ]));
    let (conversation, path) = conversation_with(&fixture, script as _, None);
    let failed = crate::test_support::run_async(conversation.run_turn("initial goal", &mut |_| {})).unwrap();
    assert_eq!(failed.turn_status, TurnStatus::Failed);
    let detail = failed.error.expect("a failed turn reports its detail");

    // 持久终态记录携带同一份细节。
    let durable = SessionData::open(&path).unwrap().entries().iter().find_map(|entry| match entry {
        SessionEntry::Record {
            record: LedgerRecord::OperationFinished { error, .. },
            ..
        } => error.clone(),
        _ => None,
    });
    assert_eq!(durable.as_ref(), Some(&detail));

    let thread_id = conversation.thread().thread_id;
    let projected_error = |catalog: &ThreadCatalog| {
        let snapshot = catalog.read_snapshot(&thread_id).unwrap();
        let page = snapshot.page(10, None).unwrap();
        page.turns
            .iter()
            .map(|turn| (turn.turn_id.clone(), turn.status, turn.error.clone()))
            .collect::<Vec<_>>()
    };
    let before = projected_error(&fixture.catalog());
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1, Some(TurnStatus::Failed));
    assert_eq!(before[0].2.as_ref(), Some(&detail));

    // 随后成功的一轮不改写较早失败的持久事实；重新打开目录仍能重建它。
    crate::test_support::run_async(conversation.run_turn("later goal", &mut |_| {})).unwrap();
    let after = projected_error(&fixture.catalog());
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].2.as_ref(), Some(&detail));
    assert_eq!(after[1].1, Some(TurnStatus::Completed));
    assert_eq!(after[1].2, None, "a successful turn carries no failure detail");
}
