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
use crate::test_support::{GatedProvider, SessionsFixture, conversation_with, input_sequence};
use crate::{Conversation, ConversationControlError, FollowUpPromotion};
use singularity_agent::session::{LedgerRecord, SessionData, SessionEntry};
use singularity_model::{
    ModelErrorKind, ModelRole, Provider,
    test_support::{ScriptedAttempt, ScriptedProvider},
};
use singularity_protocol::TurnEvent;
use singularity_protocol::TurnStatus;
use singularity_protocol::{ControlChannel, ControlDisposition};

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
        let outcome = worker_conversation.run_turn(&goal, &mut |event| events.push(event));
        (outcome, events)
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the model");
    inject(&control_conversation);
    let _ = release_tx.send(());
    let (outcome, events) = worker.join().expect("worker");
    (
        outcome.expect("every control run converges to a trusted terminal outcome"),
        events,
    )
}

/// 一条场景钉住全部 FIFO 接受语义：steer 对（注入下一请求、按接受序）、
/// follow-up 对（可信终态后各自成回合、携带文本）、跨通道共享序号、撤回
/// 不产生 durable 归宿，以及 steer 在下一份 assistant 响应前进入请求。
#[test]
fn controls_are_accepted_in_shared_fifo_order_with_true_dispositions() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("c1", "read", serde_json::json!({"path": "missing-a"})),
        ScriptedAttempt::success("adjusted course"),
        ScriptedAttempt::success("f1 done"),
        ScriptedAttempt::success("f2 done"),
    ]));
    let (gate, started_rx) = GatedProvider::new(script.clone() as Arc<dyn Provider + Send + Sync>);
    let (conversation, path) = conversation_with(&fixture, Arc::clone(&gate) as _, None);
    let (outcome, _) =
        run_with_control_window(&gate, started_rx, &conversation, "initial goal", |c| {
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

    assert!(conversation.snapshot().pending_controls.is_empty());
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
    let fixture = SessionsFixture::new();
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let (conversation, path) =
        conversation_with(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
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
    let (conversation, _) = conversation_with(
        &fixture,
        Arc::clone(&script) as Arc<dyn Provider + Send + Sync>,
        None,
    );
    let queued = std::sync::Mutex::new(None);
    let outcome = {
        let conversation = Arc::clone(&conversation);
        conversation
            .run_turn("go", &mut |event| {
                if let TurnEvent::ProviderAttempt { observation, .. } = &event
                    && observation.status == ProviderAttemptStatus::Error
                {
                    // 真实失败已经确定、终态尚未裁决：此时接受停止。
                    *queued.lock().unwrap() = Some(
                        conversation
                            .submit_follow_up("must stay queued")
                            .expect("a queued input is accepted before the terminal"),
                    );
                    conversation.abort().expect("the stop is accepted");
                }
            })
            .expect("a real failure still converges to a trusted terminal")
    };

    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    assert!(
        outcome.user_stopped,
        "the accepted stop is part of the outcome"
    );
    assert_eq!(
        outcome.error.as_ref().map(|error| error.cause),
        Some(TurnFailureCause::ProviderAuth),
        "the real failure reason is preserved"
    );
    assert_eq!(
        script.requests().len(),
        1,
        "an accepted stop never starts the next queued turn"
    );
    let pending = conversation.snapshot().pending_controls;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].control_id,
        queued.lock().unwrap().as_ref().unwrap().control_id
    );
}

/// 接受停止同时关闭本轮注入窗口：其后的 steer 与 send-now 都被拒绝，
/// 原队列项保持原位（它们属于下一轮，不属于本轮的取消集合）。
#[test]
fn an_accepted_stop_closes_the_injection_window_without_losing_queued_input() {
    let fixture = SessionsFixture::new();
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let (conversation, _) =
        conversation_with(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            conversation.run_turn("initial", &mut sink)
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the provider");
    let queued = conversation
        .submit_follow_up("kept for the next turn")
        .expect("queue a follow-up");
    conversation.abort().expect("stop the running turn");

    assert!(matches!(
        conversation.steer("late steer"),
        Err(ConversationControlError::NotRunning)
    ));
    assert!(matches!(
        conversation.promote_pending(Some(&queued.control_id)),
        Err(ConversationControlError::NotRunning)
    ));
    assert!(matches!(
        conversation.promote_pending(None),
        Err(ConversationControlError::NotRunning)
    ));
    let pending = conversation.snapshot().pending_controls;
    assert_eq!(pending.len(), 1, "the queued input is not lost");
    assert_eq!(pending[0].control_id, queued.control_id);
    assert_eq!(pending[0].sequence, queued.sequence);
    assert_eq!(
        conversation.phase(),
        singularity_protocol::SessionPhase::Stopping
    );

    let _ = release_tx.send(());
    let outcome = worker
        .join()
        .expect("worker")
        .expect("interruption converges durably");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    assert!(outcome.user_stopped);
    assert_eq!(
        conversation.snapshot().pending_controls.len(),
        1,
        "the stopped turn leaves the queued follow-up for the next explicit input"
    );
}

/// 失败归还的输入与队列共用同一接受序：先接受的 follow-up 排在后接受的
/// steer 之前，channel 不决定等待位置。
#[test]
fn returned_inputs_are_requeued_in_acceptance_order() {
    let fixture = SessionsFixture::new();
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let (conversation, path) =
        conversation_with(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            conversation.run_turn("initial", &mut sink)
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the turn reaches the provider");
    let follow_up = conversation
        .submit_follow_up("first accepted")
        .expect("queue the follow-up first");
    let steer = conversation.steer("second accepted").expect("steer second");
    assert!(follow_up.sequence < steer.sequence);
    assert_eq!(
        steer.turn_id.as_deref(),
        Some(
            conversation
                .active_controls()
                .expect("the running turn owns the inbox")
                .turn_id
                .as_str()
        ),
        "an injected steer is bound to the running turn while it is still in the inbox"
    );

    // 让本轮在写回 assistant 时失败：注入箱里未消费的 steer 被归还。
    std::fs::remove_file(&path).unwrap();
    let _ = release_tx.send(());
    assert!(
        worker.join().expect("worker").is_err(),
        "the failed turn reports its error instead of a trusted terminal"
    );

    let pending = conversation.snapshot().pending_controls;
    assert_eq!(
        pending
            .iter()
            .map(|control| control.text.as_str())
            .collect::<Vec<_>>(),
        ["first accepted", "second accepted"],
        "a returned input takes its acceptance position, not the queue head"
    );
    // 归还同时解除 turn 关联：那一轮已经结束；身份、来源与接受序号保持原值，
    // 界面据此仍能逐项处置同一条输入。
    assert_eq!(pending[1].control_id, steer.control_id);
    assert_eq!(pending[1].sequence, steer.sequence);
    assert_eq!(
        pending[1].turn_id, None,
        "a returned input no longer belongs to the turn that just ended"
    );
    assert_eq!(pending[1].channel, ControlChannel::Steer);
    assert_eq!(pending[1].disposition, ControlDisposition::Pending);
}

#[test]
fn cancel_is_rejected_once_the_turn_terminal_is_published() {
    let fixture = SessionsFixture::new();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::success("done")]));
    let (conversation, _) =
        conversation_with(&fixture, provider as Arc<dyn Provider + Send + Sync>, None);
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
        !ThreadCatalog::new(fixture.dir.clone(), Arc::clone(&fixture.coordinator))
            .read_thread_summary(&conversation.thread().thread_id)
            .expect("summary projection")
            .manually_stopped
    );
}

#[test]
fn follow_up_edit_keeps_one_identity_and_one_fifo_position() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("initial done"),
        ScriptedAttempt::success("edited follow-up done"),
    ]));
    let (gate, started_rx) =
        GatedProvider::new(Arc::clone(&script) as Arc<dyn Provider + Send + Sync>);
    let (conversation, _) = conversation_with(&fixture, Arc::clone(&gate) as _, None);

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
            assert_eq!(conversation.snapshot().pending_controls, vec![edited]);
        },
    );

    assert!(conversation.snapshot().pending_controls.is_empty());
    assert_eq!(
        input_sequence(&script.requests()),
        ["initial", "edited text"]
    );
}

#[test]
fn follow_up_promotion_preserves_identity_and_order_for_single_and_batch_inputs() {
    for inputs in [vec!["promote this"], vec!["batch one", "batch two"]] {
        let fixture = SessionsFixture::new();
        let script = Arc::new(ScriptedProvider::new([
            ScriptedAttempt::tool_call("c1", "read", serde_json::json!({"path": "missing"})),
            ScriptedAttempt::success("done"),
        ]));
        let (gate, started_rx) =
            GatedProvider::new(Arc::clone(&script) as Arc<dyn Provider + Send + Sync>);
        let (conversation, _) = conversation_with(&fixture, Arc::clone(&gate) as _, None);
        let mut expected = Vec::new();
        let (outcome, events) = run_with_control_window(
            &gate,
            started_rx,
            &conversation,
            "initial",
            |conversation| {
                for text in &inputs {
                    expected.push(conversation.submit_follow_up(*text).expect("queue input"));
                }
                let target = (inputs.len() == 1).then_some(expected[0].control_id.as_str());
                assert!(matches!(
                    conversation
                        .promote_pending(target)
                        .expect("promote inputs"),
                    FollowUpPromotion::Injected
                ));
                assert!(conversation.snapshot().pending_controls.is_empty());
            },
        );
        for control in &mut expected {
            control.turn_id = Some(outcome.turn_id.clone());
            control.disposition = ControlDisposition::Injected;
        }
        let injected: Vec<_> = events
            .into_iter()
            .filter_map(|event| match event {
                TurnEvent::ControlChanged { control }
                    if control.disposition == ControlDisposition::Injected =>
                {
                    Some(control)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            injected, expected,
            "each accepted input is delivered once, in order"
        );
        assert!(conversation.snapshot().pending_controls.is_empty());
        let requests = script.requests();
        assert_eq!(requests.len(), 2);
        let delivered: Vec<_> = requests[1]
            .messages
            .iter()
            .filter(|message| {
                message.role == ModelRole::User && inputs.contains(&message.content.as_str())
            })
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(delivered, inputs, "all inputs reach the same active turn");
    }
}

/// 空闲会话的批量发送只交出队首：其余条目按原顺序留在队列中，由该预订的链条
/// 在自然交接点继续消费。
#[test]
fn sending_the_whole_queue_while_idle_reserves_only_the_head() {
    let fixture = SessionsFixture::new();
    let (gate, started) = GatedProvider::stop_gate();
    let runner = fixture.runner(Some(gate.clone()));
    let thread = fixture
        .catalog()
        .create_thread(fixture.home().to_str().unwrap(), None)
        .unwrap();
    let conversation = Conversation::new(runner, thread);
    run_with_control_window(
        &gate,
        started,
        &conversation,
        "saved input",
        |conversation| {
            conversation
                .submit_follow_up("head input")
                .expect("queue head");
            conversation
                .submit_follow_up("tail input")
                .expect("queue tail");
            conversation.abort().expect("stop the running turn");
        },
    );
    let queued = conversation.snapshot().pending_controls;
    assert_eq!(queued.len(), 2);

    let promotion = conversation
        .promote_pending(None)
        .expect("reserve the queue head");
    assert!(matches!(promotion, FollowUpPromotion::Reserved { .. }));
    assert_eq!(
        conversation
            .snapshot()
            .pending_controls
            .iter()
            .map(|control| control.control_id.clone())
            .collect::<Vec<_>>(),
        vec![queued[1].control_id.clone()],
        "the remaining queue stays queued for the reserved chain"
    );

    // 预订未执行即销毁时，队首回到原队列，顺序不变。
    drop(promotion);
    assert_eq!(conversation.snapshot().pending_controls, queued);
}

/// 空队列上的批量发送是安全结束，不是错误；指定不存在的单条仍报原来的错误。
#[test]
fn sending_an_empty_queue_is_a_no_op_and_an_unknown_control_still_fails() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([ScriptedAttempt::success("done")]));
    let (conversation, _) = conversation_with(&fixture, script as _, None);

    assert!(matches!(
        conversation.promote_pending(None),
        Ok(FollowUpPromotion::Empty)
    ));
    assert_eq!(conversation.snapshot().pending_controls, Vec::new());
    assert!(matches!(
        conversation.promote_pending(Some("missing-control")),
        Err(ConversationControlError::ControlNotFound)
    ));
}

#[test]
fn skill_load_failure_keeps_measured_usage_in_the_failed_terminal() {
    let fixture = SessionsFixture::new();
    let skill_path = fixture.home().join("skills/review.md");
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
    let (conversation, path) = conversation_with(&fixture, Arc::clone(&gate) as _, None);
    let (outcome, _) =
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
    assert_eq!(
        error.cause,
        crate::TurnFailureCause::ProjectInstructions,
        "技能正文属于指令材料：不因发生在注入阶段就归为无来源 Internal"
    );
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
    assert!(conversation.snapshot().pending_controls.is_empty());
}

/// 失败 turn 的细节随 operation 终态落盘，历史重读直接带同一错误概念：
/// 后续成功轮次、重新打开目录都不会让较早的失败原因消失，也不再依赖
/// runtime 最近一次错误文本。
#[test]
fn a_failed_turn_keeps_its_detail_across_reload_and_a_later_success() {
    let fixture = SessionsFixture::new();
    let skill_path = fixture.home().join("skills/review.md");
    std::fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
    std::fs::write(
        &skill_path,
        "---\nname: review\ndescription: Review changes\n---\nReview the change",
    )
    .unwrap();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("first response"),
        ScriptedAttempt::success("second response"),
    ]));
    let (gate, started_rx) = GatedProvider::new(script as Arc<dyn Provider + Send + Sync>);
    let (conversation, path) = conversation_with(&fixture, Arc::clone(&gate) as _, None);
    let (failed, _) =
        run_with_control_window(&gate, started_rx, &conversation, "initial goal", move |c| {
            std::fs::remove_file(&skill_path).unwrap();
            c.steer("/review this change").unwrap();
        });
    assert_eq!(failed.turn_status, TurnStatus::Failed);
    let detail = failed.error.expect("a failed turn reports its detail");

    // 持久终态记录携带同一份细节。
    let durable =
        SessionData::open(&path)
            .unwrap()
            .entries()
            .iter()
            .find_map(|entry| match entry {
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
    conversation.run_turn("later goal", &mut |_| {}).unwrap();
    let after = projected_error(&fixture.catalog());
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].2.as_ref(), Some(&detail));
    assert_eq!(after[1].1, Some(TurnStatus::Completed));
    assert_eq!(
        after[1].2, None,
        "a successful turn carries no failure detail"
    );
}

#[test]
fn pending_queue_survives_stop_but_is_not_restored_with_history() {
    let fixture = SessionsFixture::new();
    let (gate, started) = GatedProvider::stop_gate();
    let runner = fixture.runner(Some(gate.clone()));
    let thread = fixture
        .catalog()
        .create_thread(fixture.home().to_str().unwrap(), None)
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
    let queued = conversation.snapshot().pending_controls;
    assert_eq!(queued.len(), 1);
    let promotion = conversation
        .promote_pending(Some(&queued[0].control_id))
        .unwrap();
    assert!(matches!(promotion, FollowUpPromotion::Reserved { .. }));
    assert!(conversation.snapshot().pending_controls.is_empty());
    drop(promotion);
    assert_eq!(conversation.snapshot().pending_controls, queued);
    drop(conversation);
    let reopened = Conversation::new(runner, thread.clone());
    assert!(reopened.snapshot().pending_controls.is_empty());
    let history =
        std::fs::read_to_string(fixture.dir.join(format!("{}.jsonl", thread.thread_id))).unwrap();
    assert!(history.contains("saved input"));
    assert!(!history.contains("unconsumed input"));
}

/// 保留的 follow-up 之后的普通提交有同一套身份：启动写者失败后两条输入都
/// 留在队列里，公开的待处理身份集合与已接受集合一致，都能按 ID 处置，且
/// 保留的输入仍按接受顺序先于后来者执行、被撤回的从不运行。
/// 这正是「内部待处理身份集合 == 公开可管理身份集合」的验收断言。
#[test]
fn a_submission_queued_behind_a_retained_follow_up_stays_manageable() {
    let fixture = SessionsFixture::new();
    let script = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("retained follow-up done"),
        ScriptedAttempt::success("later input done"),
    ]));
    let (gate, started) = GatedProvider::new(script.clone() as Arc<dyn Provider + Send + Sync>);
    let runner = fixture.runner(Some(gate.clone()));
    let thread = fixture
        .catalog()
        .create_thread(fixture.home().to_str().unwrap(), None)
        .unwrap();
    let conversation = Conversation::new(runner.clone(), thread.clone());

    // 第一轮留下一条保留的 follow-up 后中断：队列里只有它。
    let (first, _) = run_with_control_window(&gate, started, &conversation, "initial goal", |c| {
        c.submit_follow_up("retained follow-up").unwrap();
        c.abort().unwrap();
    });
    assert_eq!(first.turn_status, TurnStatus::Interrupted);
    let retained = conversation.snapshot().pending_controls;
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].text, "retained follow-up");

    // 独占写者，让紧接着的启动在打开写者时失败：普通提交排在保留输入之后，
    // 两者都必须留在同一个待处理集合里。
    let writer = runner.open_turn_writer(&thread).unwrap();
    assert!(
        conversation
            .run_turn("later submission", &mut |_| {})
            .is_err(),
        "the writer is held, so the chain cannot start"
    );
    drop(writer);

    let pending = conversation.snapshot().pending_controls;
    assert_eq!(
        pending.len(),
        2,
        "the retained follow-up and the submission are both still accepted and unconsumed"
    );
    assert_eq!(pending[0].control_id, retained[0].control_id);
    assert_eq!(pending[0].text, "retained follow-up");
    assert_eq!(pending[1].text, "later submission");
    assert_eq!(pending[1].channel, ControlChannel::Submit);
    assert_eq!(pending[1].disposition, ControlDisposition::Pending);
    assert!(pending[1].sequence > pending[0].sequence);

    // 逐项处置：按 ID 撤回普通提交，保留的 follow-up 不受影响。
    conversation
        .withdraw_follow_up(&pending[1].control_id)
        .expect("the queued submission is withdrawable by id");
    let remaining = conversation.snapshot().pending_controls;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].control_id, pending[0].control_id);

    // 释放写者后整轮执行：保留的输入按接受顺序先于后来的输入，被撤回的从不运行。
    // 每一步模型请求里最新的用户输入就是该步正在执行的输入。
    conversation.run_turn("later input", &mut |_| {}).unwrap();
    assert_eq!(
        input_sequence(&script.requests()),
        ["retained follow-up", "later input"],
        "the retained input keeps its place and the withdrawn one never runs"
    );
}

/// 运行中的回合里排队的普通提交必须能被整队列提升交付，而不是让提升在
/// 队列中遇到没有身份的条目时报 ControlNotFound。
#[test]
fn batch_promotion_never_stumbles_on_a_queued_submission() {
    let fixture = SessionsFixture::new();
    let (gate, started) = GatedProvider::stop_gate();
    let runner = fixture.runner(Some(gate.clone()));
    let thread = fixture
        .catalog()
        .create_thread(fixture.home().to_str().unwrap(), None)
        .unwrap();
    let conversation = Conversation::new(runner, thread.clone());

    // 第一轮留下一条保留的 follow-up 后中断；接收端留给下面第二轮继续使用。
    let (first_release_tx, first_release_rx) = channel();
    gate.with_release(first_release_rx);
    let first_worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            conversation.run_turn("initial goal", &mut sink)
        })
    };
    started
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the first turn reaches the model");
    conversation.submit_follow_up("retained follow-up").unwrap();
    conversation.abort().unwrap();
    let _ = first_release_tx.send(());
    let first = first_worker
        .join()
        .expect("worker")
        .expect("interruption converges durably");
    assert_eq!(first.turn_status, TurnStatus::Interrupted);

    // 保留的 follow-up 成为下一轮；普通提交排在它后面，本轮执行期间仍在队列里。
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |_event: TurnEvent| {};
            conversation.run_turn("later submission", &mut sink)
        })
    };
    started
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the retained follow-up reaches the model");

    let queued = conversation.snapshot().pending_controls;
    assert_eq!(queued.len(), 1, "only the submission is still queued");
    assert_eq!(queued[0].channel, ControlChannel::Submit);
    assert_eq!(queued[0].text, "later submission");

    let promotion = conversation
        .promote_pending(None)
        .expect("the whole queue is promotable regardless of how each input entered");
    assert!(matches!(promotion, FollowUpPromotion::Injected));
    assert!(conversation.snapshot().pending_controls.is_empty());

    let _ = release_tx.send(());
    let outcome = worker
        .join()
        .expect("worker")
        .expect("the promoted submission converges");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    let history =
        std::fs::read_to_string(fixture.dir.join(format!("{}.jsonl", thread.thread_id))).unwrap();
    assert!(
        history.contains("later submission"),
        "the promoted submission is delivered as a user message"
    );
}
