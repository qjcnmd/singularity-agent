#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
//! T046 [US4]：FIFO 控制接受的 durable 归宿（steer / follow_up / cancel）。
//!
//! 三条通道共用协调器的一个接受序号计数器（contracts/control-provider-tools.md、
//! data-model.md Control Request）：steer 在下一份 assistant 响应前注入
//! （`injected`）、follow-up 在当前回合可信终态后作为独立回合启动
//! （`started_as_new_turn`）、cancel 收敛 interrupted（`cancelled`）且记录
//! 先于终态落盘；撤回且从未启动的输入不产生 durable 记录。窗口内的控制
//! 注入由 [`GatedProvider`] 钉住（首个请求停在模型边界），不使用
//! sleep。单写者窗口对控制的接受/拒绝语义由 conversation_tests 覆盖。

use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use crate::Conversation;
use crate::events::TurnEvent;
use crate::objects::TurnStatus;
use crate::test_support::{GatedProvider, conversation_with, input_sequence, temp_sessions};
use singularity_agent::session::{
    ControlChannel, ControlDisposition, LedgerRecord, SessionEntry, SessionManager,
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
        .expect("control ledger reduces cleanly")
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

fn first_record_index(
    path: &std::path::Path,
    predicate: impl Fn(&LedgerRecord) -> bool,
) -> Option<usize> {
    let session = SessionManager::open_existing_read_only(path).expect("reopen");
    session
        .entries()
        .iter()
        .enumerate()
        .find_map(|(index, entry)| {
            matches!(entry, SessionEntry::Record { record, .. } if predicate(record))
                .then_some(index)
        })
}

/// 在「turn 已注册、模型未返回」的窗口内执行控制注入，随后释放收敛。
/// 注入必须在 join 前完成：借用协调器的闭包在 worker 存续期内调用。
fn run_with_control_window(
    gate: &Arc<GatedProvider>,
    started_rx: Receiver<()>,
    conversation: &Arc<Conversation>,
    goal: &str,
    inject: impl FnOnce(&Conversation) + Send + 'static,
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
        c.submit_follow_up("f2");
        c.submit_follow_up("f3");
        assert!(
            c.withdraw_follow_up().is_some(),
            "f3 is withdrawable before start"
        );
        assert!(s1 && f1 && s2);
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
            conversation.interrupt();
            let _ = release_tx.send(());
        })
    };
    let mut sink = |_event: TurnEvent| {};
    let outcome = conversation
        .run_turn("cancellable", &mut sink)
        .expect("interruption converges durably");
    interrupter.join().expect("interrupter");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);

    let cancel_at = first_record_index(&path, |record| {
        matches!(
            record,
            LedgerRecord::ControlAccepted {
                channel: ControlChannel::Cancel,
                disposition: ControlDisposition::Cancelled,
                text: None,
                ..
            }
        )
    })
    .expect("the accepted cancel is durable");
    let finished_at = first_record_index(&path, |record| {
        matches!(
            record,
            LedgerRecord::OperationFinished {
                outcome: TurnStatus::Interrupted,
                ..
            }
        )
    })
    .expect("interrupted terminal");
    assert!(
        cancel_at < finished_at,
        "cancel acceptance precedes the terminal record"
    );

    // 取消不影响后续合法输入：下一条输入作为新 turn 正常完成。
    let next = conversation.run_turn("next input", &mut sink);
    assert!(next.is_ok(), "the thread stays usable after a cancel");
}
