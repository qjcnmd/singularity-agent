#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! 运行期 FIFO 输入、停止和已消费消息的持久化测试。
//!
//! steer 与 follow-up 共用接受序号计数器：
//! steer 输入在下一份 assistant 响应前注入当前轮次（injected）；
//! follow-up 在当前轮次可信终态后作为独立轮次启动（started_as_new_turn）；
//! cancel 触发 interrupted 终态；撤回且从未启动的输入不产生 user 消息。窗口内的控制
//! 注入由 GatedProvider 钉住（首个请求停在模型边界），不使用
//! sleep。单写者窗口对控制的接受/拒绝语义由同目录 conversation 覆盖。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crate::ThreadCatalog;
use crate::runner::TurnRunner;
use crate::test_support::{
    GatedProvider, conversation_with, input_sequence, provider_snapshot, temp_sessions,
};
use crate::{Conversation, ConversationControlError, FollowUpPromotion};
use singularity_agent::session::{LedgerRecord, SessionData, SessionEntry};
use singularity_model::{
    ModelRole, Provider,
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
    inject: impl FnOnce(&Arc<Conversation>) + Send + 'static,
) -> crate::TurnOutcome {
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let worker_conversation = Arc::clone(conversation);
    let control_conversation = Arc::clone(conversation);
    let goal = goal.to_string();
    let worker = std::thread::spawn(move || {
        let mut sink = |_event: TurnEvent| {};
        worker_conversation.run_turn(&goal, &mut sink)
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the model");
    inject(&control_conversation);
    let _ = release_tx.send(());
    worker
        .join()
        .expect("worker")
        .expect("every control run converges to a trusted terminal outcome")
}

/// 一条场景钉住全部 FIFO 接受语义：steer 对（注入下一请求、按接受序）、
/// follow-up 对（可信终态后各自成回合、携带文本）、跨通道共享序号、撤回
/// 不产生 durable 归宿，以及 steer 在下一份 assistant 响应前进入请求。
#[test]
fn controls_are_accepted_in_shared_fifo_order_with_true_dispositions() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("c1", "read", serde_json::json!({"path": "missing-a"})),
        ScriptedAttempt::success("adjusted course"),
        ScriptedAttempt::success("f1 done"),
        ScriptedAttempt::success("f2 done"),
    ]));
    let (gate, started_rx) = GatedProvider::new(script.clone() as Arc<dyn Provider + Send + Sync>);
    let (conversation, path) = conversation_with(&sessions, Arc::clone(&gate) as _, None);
    let outcome = run_with_control_window(&gate, started_rx, &conversation, "initial goal", |c| {
        let s1 = c.steer("steer left").unwrap();
        let f1 = c.submit_follow_up("f1").unwrap();
        let s2 = c.steer("steer right").unwrap();
        c.submit_follow_up("f2").expect("queue f2");
        let f3 = c.submit_follow_up("f3").expect("queue f3");
        assert!(
            c.withdraw_follow_up(&f3.control_id).is_ok(),
            "f3 is withdrawable before start"
        );
        assert!(s1.sequence < f1.sequence && f1.sequence < s2.sequence);
    });
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    assert!(conversation.pending_controls().is_empty());
    assert!(!SessionData::open(&path).unwrap().entries().iter().any(|entry| matches!(entry, SessionEntry::Message { message, .. } if message.content_text() == "f3")));
    let requests = script.requests();
    assert_eq!(requests.len(), 4, "two model steps + one per follow-up");
    assert_eq!(
        input_sequence(&requests[2..]),
        ["f1", "f2"],
        "each follow-up runs as its own turn, in acceptance order"
    );
    let second_request_users: Vec<String> = requests[1]
        .messages
        .iter()
        .filter(|message| {
            message.role == ModelRole::User && !message.content.starts_with("<system-reminder>")
        })
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

/// cancel：回合终态保存用户停止标志。
/// 记录；取消不影响后续合法输入。
#[test]
fn cancellation_records_manual_stop_and_leaves_the_thread_usable() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let (conversation, path) =
        conversation_with(&sessions, gate as Arc<dyn Provider + Send + Sync>, None);
    let interrupter = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the turn reaches the provider");
            conversation.abort().expect("abort active turn");
            let _ = release_tx.send(());
        })
    };
    let mut events = Vec::new();
    let outcome = {
        let mut sink = |event: TurnEvent| events.push(event);
        conversation
            .run_turn("cancellable", &mut sink)
            .expect("interruption converges durably")
    };
    interrupter.join().expect("interrupter");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    let terminal_events: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
            )
        })
        .collect();
    assert_eq!(terminal_events.len(), 1, "one terminal event is emitted");
    assert!(matches!(
        terminal_events[0],
        TurnEvent::TurnCompleted { turn } if turn.status == TurnStatus::Interrupted
    ));

    let session = SessionData::open(&path).expect("reopen");
    let entries = session.entries();
    let finished: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            matches!(
                entry,
                SessionEntry::Record {
                    record: LedgerRecord::OperationFinished { .. },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(finished.len(), 1, "one terminal outcome is durable");
    assert!(matches!(
        finished[0].1,
        SessionEntry::Record {
            record: LedgerRecord::OperationFinished {
                outcome: TurnStatus::Interrupted,
                user_stopped: true,
                ..
            },
            ..
        }
    ));
    // 取消不影响后续合法输入：下一条输入作为新 turn 正常完成。
    let mut sink = |_event: TurnEvent| {};
    let next = conversation.run_turn("next input", &mut sink);
    assert!(next.is_ok(), "the thread stays usable after a cancel");
}

#[test]
fn cancel_is_rejected_once_the_turn_terminal_is_published() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::success("done")]));
    let (conversation, path) =
        conversation_with(&sessions, provider as Arc<dyn Provider + Send + Sync>, None);
    let aborter = Arc::clone(&conversation);
    let mut late_abort = None;
    let outcome = conversation
        .run_turn("complete normally", &mut |event| {
            if matches!(event, TurnEvent::TurnCompleted { .. }) {
                late_abort = Some(aborter.abort());
            }
        })
        .expect("the turn reaches its trusted terminal");

    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert!(matches!(
        late_abort,
        Some(Err(ConversationControlError::NotRunning))
    ));
    assert!(
        !singularity_agent::session::project_session(&SessionData::open(&path).unwrap(), false)
            .manually_stopped
    );
}

/// 撤回在一次生命周期临界区内先持久化后改队列：写盘失败时队列的身份、
/// 次序与文本完全不变；成功撤回按同一条 identity 收敛为 cancelled。

#[test]
fn follow_up_edit_keeps_one_identity_and_one_fifo_position() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("initial done"),
        ScriptedAttempt::success("edited follow-up done"),
    ]));
    let (gate, started_rx) =
        GatedProvider::new(Arc::clone(&script) as Arc<dyn Provider + Send + Sync>);
    let (conversation, _) = conversation_with(&sessions, Arc::clone(&gate) as _, None);

    run_with_control_window(
        &gate,
        started_rx,
        &conversation,
        "initial",
        |conversation| {
            let queued = conversation
                .submit_follow_up("original text")
                .expect("queue follow-up");
            let edited = conversation
                .replace_follow_up(&queued.control_id, "edited text")
                .expect("edit pending follow-up");
            assert_eq!(edited.control_id, queued.control_id);
            assert_eq!(edited.sequence, queued.sequence);
            assert_eq!(conversation.pending_controls(), vec![edited]);
        },
    );

    assert!(conversation.pending_controls().is_empty());
    assert_eq!(
        input_sequence(&script.requests()),
        ["initial", "edited text"]
    );
}

#[test]
fn running_follow_up_promotion_reuses_one_identity_and_injects_once() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("c1", "read", serde_json::json!({"path": "missing"})),
        ScriptedAttempt::success("done"),
    ]));
    let (gate, started_rx) =
        GatedProvider::new(Arc::clone(&script) as Arc<dyn Provider + Send + Sync>);
    let (conversation, _) = conversation_with(&sessions, Arc::clone(&gate) as _, None);

    run_with_control_window(
        &gate,
        started_rx,
        &conversation,
        "initial",
        |conversation| {
            let queued = conversation
                .submit_follow_up("promote this")
                .expect("queue follow-up");
            let promoted = conversation
                .promote_follow_up(&queued.control_id)
                .expect("promote into active inbox");
            assert!(matches!(
                promoted,
                FollowUpPromotion::Injected(ref control)
                    if control.control_id == queued.control_id
                        && control.sequence == queued.sequence
            ));
            assert!(conversation.pending_controls().is_empty());
        },
    );

    assert!(conversation.pending_controls().is_empty());
    assert_eq!(script.requests().len(), 2);
    assert!(
        script.requests()[1]
            .messages
            .iter()
            .any(|message| message.content.contains("promote this"))
    );
}

#[test]
fn skill_load_failure_keeps_measured_usage_in_the_failed_terminal() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let skill_path = home.path().join("skills/review.md");
    std::fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
    std::fs::write(
        &skill_path,
        "---\nname: review\ndescription: Review changes\n---\nReview the change",
    )
    .unwrap();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success_with_usage(
            "first response",
            singularity_model::ModelUsage {
                input_tokens: 100,
                output_tokens: 20,
                total_tokens: 120,
                usage_present: true,
                cached_input_tokens_present: true,
                ..Default::default()
            },
        ),
    ]));
    let (gate, started_rx) = GatedProvider::new(script.clone() as Arc<dyn Provider + Send + Sync>);
    let (conversation, path) = conversation_with(&sessions, Arc::clone(&gate) as _, None);
    let outcome =
        run_with_control_window(&gate, started_rx, &conversation, "initial goal", move |c| {
            std::fs::remove_file(&skill_path).unwrap();
            c.steer("/review this change").unwrap();
        });
    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    assert_eq!(outcome.usage.input_tokens, 100);
    assert_eq!(outcome.usage.output_tokens, 20);
    assert_eq!(outcome.usage.total_tokens, 120);
    assert!(outcome.usage.usage_present && outcome.usage.usage_complete);
    let error = outcome.error.unwrap();
    assert!(error.message.contains("review.md"));
    assert_eq!(error.cause, crate::TurnFailureCause::Internal);
    assert_eq!(script.requests().len(), 1);

    let session = SessionData::open(&path).unwrap();
    let terminal_usage = session.entries().iter().find_map(|entry| match entry {
        SessionEntry::Record {
            record: LedgerRecord::OperationFinished { usage, .. },
            ..
        } => usage.as_ref(),
        _ => None,
    });
    assert_eq!(terminal_usage, Some(&outcome.usage));
    assert!(conversation.pending_controls().is_empty());
}

#[test]
fn pending_queue_survives_stop_but_is_not_restored_with_history() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (gate, started) = GatedProvider::stop_gate();
    let runner = Arc::new(
        TurnRunner::new(sessions.clone(), provider_snapshot()).with_provider_override(gate.clone()),
    );
    let thread = ThreadCatalog::new(&runner)
        .create_thread(home.path().to_str().unwrap(), None)
        .unwrap();
    let conversation = Conversation::new(runner.clone(), thread.clone());
    run_with_control_window(
        &gate,
        started,
        &conversation,
        "saved input",
        |conversation| {
            conversation.submit_follow_up("unconsumed input").unwrap();
            conversation.abort().unwrap();
        },
    );
    let queued = conversation.pending_controls();
    assert_eq!(queued.len(), 1);
    let promotion = conversation
        .promote_follow_up(&queued[0].control_id)
        .unwrap();
    assert!(matches!(promotion, FollowUpPromotion::Reserved { .. }));
    assert!(conversation.pending_controls().is_empty());
    drop(promotion);
    assert_eq!(conversation.pending_controls(), queued);
    drop(conversation);
    let reopened = Conversation::new(runner, thread.clone());
    assert!(reopened.pending_controls().is_empty());
    let history =
        std::fs::read_to_string(sessions.join(format!("{}.jsonl", thread.thread_id))).unwrap();
    assert!(history.contains("saved input"));
    assert!(!history.contains("unconsumed input"));
}
