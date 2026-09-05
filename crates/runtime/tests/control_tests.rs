#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! FIFO 控制通道与持久化状态推进测试（steer / follow_up / cancel）。
//!
//! steer、follow-up 与 cancel 三条控制通道共用统一的接受序号计数器：
//! steer 输入在下一份 assistant 响应前注入当前轮次（`injected`）；
//! follow-up 在当前轮次可信终态后作为独立轮次启动（`started_as_new_turn`）；
//! cancel 触发 interrupted 终态（`cancelled`）且控制记录先于轮次终态落盘；
//! 撤回且从未启动的输入保留 cancelled 控制事实，不产生 user 消息。窗口内的控制
//! 注入由 [`GatedProvider`] 钉住（首个请求停在模型边界），不使用
//! sleep。单写者窗口对控制的接受/拒绝语义由 conversation_tests 覆盖。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crate::ThreadCatalog;
use crate::events::TurnEvent;
use crate::objects::TurnStatus;
use crate::runner::TurnRunner;
use crate::test_support::{
    GatedProvider, conversation_with, input_sequence, provider_snapshot, temp_sessions,
};
use crate::{Conversation, ConversationControlError, FollowUpPromotion};
use singularity_agent::session::{
    ControlChannel, ControlDisposition, ControlRequest, LedgerRecord, SessionEntry, SessionManager,
};
use singularity_model::{
    ModelRole, Provider,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

/// control_accepted 事实投影：同一控制按 reducer 语义折叠（pending 接受 +
/// 终态 disposition → 单条最终归宿），返回 (channel, sequence, disposition, text)。
fn control_facts(
    path: &std::path::Path,
) -> Vec<(ControlChannel, u64, ControlDisposition, Option<String>)> {
    let session = SessionManager::open_existing_read_only(path).expect("reopen");
    singularity_agent::session::reduce_controls(session.entries())
        .into_iter()
        .map(|control| {
            (
                control.channel,
                control.sequence,
                control.disposition,
                control.text,
            )
        })
        .collect()
}

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
        let s1 = c.steer("steer left");
        let f1 = c.submit_follow_up("f1");
        let s2 = c.steer("steer right");
        c.submit_follow_up("f2").expect("queue f2");
        let f3 = c.submit_follow_up("f3").expect("queue f3");
        assert!(
            c.withdraw_follow_up(&f3.control_id).is_ok(),
            "f3 is withdrawable before start"
        );
        assert!(s1.is_ok() && f1.is_ok() && s2.is_ok());
    });
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    // 每条接受都 durable：pending 接受 + 终态 disposition 折叠为单条归宿。
    // s1/f1/s2/f2 启动执行；f3 接受后立即撤回，折叠为 cancelled（从未启动）。
    let controls = control_facts(&path);
    assert_eq!(
        controls.len(),
        5,
        "two steers + two started follow-ups + one withdrawn"
    );
    // FIFO 权威是接受序号（跨通道交错）：接受序号即提交顺序。
    let sequence_of = |channel: ControlChannel, text: &str| -> u64 {
        controls
            .iter()
            .find(|(c, _, _, durable_text)| {
                *c == channel
                    && durable_text
                        .as_deref()
                        .is_some_and(|durable| durable.contains(text))
            })
            .unwrap_or_else(|| panic!("no durable record for {channel:?} {text:?}"))
            .1
    };
    let s1 = sequence_of(ControlChannel::Steer, "steer left");
    let f1 = sequence_of(ControlChannel::FollowUp, "f1");
    let s2 = sequence_of(ControlChannel::Steer, "steer right");
    let f2 = sequence_of(ControlChannel::FollowUp, "f2");
    assert!(
        s1 < f1 && f1 < s2 && s2 < f2,
        "acceptance sequence interleaves channels in submission order: {s1},{f1},{s2},{f2}"
    );
    let follow_ups = controls
        .iter()
        .filter(|(channel, ..)| *channel == ControlChannel::FollowUp)
        .map(|(_, _, disposition, text)| (*disposition, text.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        follow_ups,
        vec![
            (ControlDisposition::StartedAsNewTurn, Some("f1".to_string())),
            (ControlDisposition::StartedAsNewTurn, Some("f2".to_string())),
            (ControlDisposition::Cancelled, Some("f3".to_string())),
        ],
        "f1/f2 start as their own turns; the withdrawn f3 converges to cancelled and never starts"
    );
    for (channel, _, disposition, _) in &controls {
        match channel {
            ControlChannel::Steer => assert_eq!(*disposition, ControlDisposition::Injected),
            ControlChannel::FollowUp => {
                assert!(matches!(
                    *disposition,
                    ControlDisposition::StartedAsNewTurn | ControlDisposition::Cancelled
                ));
            }
            ControlChannel::Cancel => panic!("no cancel in this scenario"),
        }
    }

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

/// cancel：接受即落 `control_accepted(cancelled)` 且先于 interrupted 终态
/// 记录；取消不影响后续合法输入。
#[test]
fn cancel_is_durable_before_the_interrupted_terminal_and_leaves_the_thread_usable() {
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
            conversation.interrupt().expect("interrupt active turn");
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

    let session = SessionManager::open_existing_read_only(&path).expect("reopen");
    let entries = session.entries();
    let cancel_at = entries
        .iter()
        .position(|entry| {
            matches!(
                entry,
                SessionEntry::Record {
                    record: LedgerRecord::ControlAccepted {
                        channel: ControlChannel::Cancel,
                        disposition: ControlDisposition::Cancelled,
                        text: None,
                        ..
                    },
                    ..
                }
            )
        })
        .expect("the accepted cancel is durable");
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
                ..
            },
            ..
        }
    ));
    let finished_at = finished[0].0;
    assert!(
        cancel_at < finished_at,
        "cancel acceptance precedes the terminal record"
    );

    // 取消不影响后续合法输入：下一条输入作为新 turn 正常完成。
    let mut sink = |_event: TurnEvent| {};
    let next = conversation.run_turn("next input", &mut sink);
    assert!(next.is_ok(), "the thread stays usable after a cancel");
}

#[test]
fn restored_pending_controls_are_visible_immediately_and_raise_the_sequence_watermark() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("initial done"),
        ScriptedAttempt::success("restored done"),
        ScriptedAttempt::success("new done"),
    ]));
    let (gate, started_rx) =
        GatedProvider::new(Arc::clone(&script) as Arc<dyn Provider + Send + Sync>);
    let runner = Arc::new(
        TurnRunner::new(sessions.clone(), provider_snapshot())
            .with_provider_override(Arc::clone(&gate) as Arc<dyn Provider + Send + Sync>),
    );
    let catalog = ThreadCatalog::new(&runner);
    let thread = catalog
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let restored = ControlRequest {
        control_id: "control-restored".to_string(),
        turn_id: "turn-before-restart".to_string(),
        channel: ControlChannel::FollowUp,
        sequence: 11,
        text: Some("restored follow-up".to_string()),
    };
    let session_path = sessions.join(format!("{}.jsonl", thread.thread_id));
    let mut writer = SessionManager::open_existing(&session_path).expect("open ledger");
    writer
        .append_record(restored.pending_record())
        .expect("seed pending control");
    drop(writer);

    let conversation =
        Conversation::new(Arc::clone(&runner), thread).expect("restore conversation");
    let pending = conversation.pending_controls();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].control_id, "control-restored");
    assert_eq!(pending[0].sequence, 11);

    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            conversation.run_turn("fresh input", &mut sink)
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("fresh turn reaches provider");
    let next = conversation
        .submit_follow_up("new follow-up")
        .expect("queue another follow-up");
    assert_eq!(
        next.sequence, 12,
        "the restored maximum raises the watermark"
    );
    let _ = release_tx.send(());
    worker
        .join()
        .expect("worker")
        .expect("turn chain completes");
    assert_eq!(
        input_sequence(&script.requests()),
        ["restored follow-up", "fresh input", "new follow-up"],
        "restored and new controls keep FIFO order"
    );
}

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
    let (conversation, path) = conversation_with(&sessions, Arc::clone(&gate) as _, None);

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

    assert_eq!(
        control_facts(&path),
        vec![(
            ControlChannel::FollowUp,
            0,
            ControlDisposition::StartedAsNewTurn,
            Some("edited text".to_string()),
        )]
    );
    assert_eq!(
        input_sequence(&script.requests()),
        ["initial", "edited text"]
    );
}

#[test]
fn restored_queue_keeps_one_editable_owner_through_execution() {
    for action in ["keep", "replace", "withdraw"] {
        let home = temp_sessions();
        let sessions = home.path().join("sessions");
        let script = Arc::new(ScriptedProvider::new(
            (0..3).map(|_| ScriptedAttempt::success("done")),
        ));
        let (gate, started) =
            GatedProvider::new(Arc::clone(&script) as Arc<dyn Provider + Send + Sync>);
        let runner = Arc::new(
            TurnRunner::new(sessions.clone(), provider_snapshot())
                .with_provider_override(Arc::clone(&gate) as _),
        );
        let thread = ThreadCatalog::new(&runner)
            .create_thread(home.path().to_str().unwrap(), None)
            .unwrap();
        let mut writer =
            SessionManager::open_existing(&sessions.join(format!("{}.jsonl", thread.thread_id)))
                .unwrap();
        for sequence in 0..2 {
            writer
                .append_record(
                    ControlRequest {
                        control_id: format!("restored-{sequence}"),
                        turn_id: "prior".into(),
                        channel: ControlChannel::FollowUp,
                        sequence,
                        text: Some(format!("queued-{sequence}")),
                    }
                    .pending_record(),
                )
                .unwrap();
        }
        drop(writer);
        let conversation = Conversation::new(runner, thread).unwrap();
        run_with_control_window(
            &gate,
            started,
            &conversation,
            "fresh",
            move |conversation| {
                assert_eq!(conversation.pending_controls().len(), 1);
                match action {
                    "replace" => {
                        conversation
                            .replace_follow_up("restored-1", "edited")
                            .unwrap();
                    }
                    "withdraw" => {
                        conversation.withdraw_follow_up("restored-1").unwrap();
                    }
                    _ => {}
                }
            },
        );
        let expected = match action {
            "replace" => vec!["queued-0", "edited", "fresh"],
            "withdraw" => vec!["queued-0", "fresh"],
            _ => vec!["queued-0", "queued-1", "fresh"],
        };
        assert_eq!(input_sequence(&script.requests()), expected, "{action}");
    }
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
    let (conversation, path) = conversation_with(&sessions, Arc::clone(&gate) as _, None);

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

    let facts = control_facts(&path);
    assert_eq!(
        facts,
        vec![(
            ControlChannel::FollowUp,
            0,
            ControlDisposition::Injected,
            Some("promote this".to_string()),
        )],
        "promotion keeps one durable control and one terminal disposition"
    );
    assert_eq!(script.requests().len(), 2);
    assert!(
        script.requests()[1]
            .messages
            .iter()
            .any(|message| message.content.contains("promote this"))
    );
}

#[test]
fn idle_promotion_reservation_restores_the_same_control_when_execution_cannot_start() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let runner = Arc::new(TurnRunner::new(sessions.clone(), provider_snapshot()));
    let catalog = ThreadCatalog::new(&runner);
    let thread = catalog
        .create_thread(std::env::current_dir().unwrap().to_str().unwrap(), None)
        .expect("create thread");
    let request = ControlRequest {
        control_id: "control-promote".to_string(),
        turn_id: "turn-before-idle".to_string(),
        channel: ControlChannel::FollowUp,
        sequence: 4,
        text: Some("keep exactly once".to_string()),
    };
    let path = sessions.join(format!("{}.jsonl", thread.thread_id));
    let mut writer = SessionManager::open_existing(&path).expect("open ledger");
    writer
        .append_record(request.pending_record())
        .expect("seed pending follow-up");
    drop(writer);

    let conversation = Conversation::new_with_model_override(
        runner,
        thread,
        Some("missing-provider/missing-model".to_string()),
    )
    .expect("restore conversation");
    let reservation = match conversation
        .promote_follow_up(&request.control_id)
        .expect("reserve selected follow-up")
    {
        FollowUpPromotion::Reserved {
            control,
            reservation,
        } => {
            assert_eq!(control.control_id, request.control_id);
            reservation
        }
        FollowUpPromotion::Injected(_) => panic!("idle promotion cannot inject"),
    };
    assert!(conversation.pending_controls().is_empty());

    let mut sink = |_event: TurnEvent| {};
    assert!(reservation.run_promoted(&mut sink).is_err());
    let pending = conversation.pending_controls();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].control_id, request.control_id);
    assert_eq!(pending[0].text.as_deref(), Some("keep exactly once"));
    assert!(matches!(
        conversation.promote_follow_up("missing"),
        Err(ConversationControlError::ControlNotFound)
    ));
    assert_eq!(
        control_facts(&path),
        vec![(
            ControlChannel::FollowUp,
            4,
            ControlDisposition::Pending,
            Some("keep exactly once".to_string()),
        )]
    );
}
