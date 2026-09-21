//! 协调器的并发与恢复行为：panic 路径的窗口释放、单写者锁冲突、预订窗口
//! 回收及写者锁占用下的设置提交。控制队列的顺序与注入由同目录 control 覆盖。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::path::Path;
use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::test_support::{
    GatedProvider, SessionsFixture, conversation_with, coordinator, input_sequence,
    seed_compaction_history, temp_sessions,
};
use singularity_agent::session::{SessionData, SessionEntry, SessionManager, SessionMetadata};
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider,
    ProviderAttemptEvent, ProviderCallError, ProviderError, ProviderStreamEvent,
    test_support::{ScriptedAttempt, ScriptedProvider},
};
use singularity_protocol::TurnEvent;
use singularity_protocol::TurnStatus;

fn new_conversation(
    fixture: &SessionsFixture,
    provider: Arc<dyn Provider + Send + Sync>,
    model: Option<&str>,
) -> Arc<Conversation> {
    conversation_with(fixture, provider, model).0
}

fn thread_settings_count(sessions: &std::path::Path, thread_id: &str) -> usize {
    SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .entries()
        .iter()
        .filter(|entry| {
            matches!(
                entry,
                SessionEntry::Metadata {
                    metadata: SessionMetadata::ThreadSettings { .. },
                    ..
                }
            )
        })
        .count()
}

/// 最后一条 thread_settings 记录反推的 selector（与 resume 投影的
/// last-wins 组合规则一致）。
fn last_recorded_selector(sessions: &std::path::Path, thread_id: &str) -> Option<String> {
    SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionEntry::Metadata {
                metadata:
                    SessionMetadata::ThreadSettings {
                        provider,
                        model,
                        reasoning,
                    },
                ..
            } => Some(singularity_model::compose_model_selector(
                provider,
                model,
                reasoning.as_deref().filter(|value| !value.is_empty()),
            )),
            _ => None,
        })
}

/// 同一条释放路径覆盖回合与压缩两种相位：panic 在展开时归还单写者窗口。
#[test]
fn a_panic_releases_the_reservation_window_in_turn_and_compaction() {
    let fixture = SessionsFixture::new();
    let conversation = new_conversation(&fixture, Arc::new(ScriptedProvider::ok("ok")), None);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut sink = |event: TurnEvent| {
            if matches!(event, TurnEvent::TurnStarted { .. }) {
                panic!("sink panic");
            }
        };
        let _ = conversation.run_turn("hello", &mut sink);
    }));
    assert!(panic.is_err(), "sink panic must propagate");
    assert!(
        conversation.phase() == singularity_protocol::SessionPhase::Idle,
        "panic must not leak the active window"
    );
    let reservation = conversation
        .reserve_start()
        .expect("reservation succeeds after a panic");
    drop(reservation);

    // 压缩相位：provider 自身 panic 时同样释放窗口。
    let sessions = fixture.dir.clone();
    let compacting = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::new([ScriptedAttempt::Panic])),
        None,
    );
    seed_compaction_history(&sessions, &compacting.thread().thread_id);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = compacting
            .reserve_compaction()
            .and_then(|mut reservation| reservation.compact());
    }));
    assert!(panic.is_err(), "the provider panic must propagate");
    assert!(
        compacting.phase() == singularity_protocol::SessionPhase::Idle,
        "compaction must release the single-writer window while unwinding"
    );
}

#[test]
fn reservation_holds_window_and_releases_on_drop() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("ok"),
        ScriptedAttempt::success("ok"),
    ]));
    let shared = new_conversation(
        &fixture,
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = shared.thread().thread_id;

    // 预订原子开启活动窗口：busy、设置、followUp 与控制路由全部从同一
    // Reserved 生命周期状态派生。
    let reservation = shared.reserve_start().expect("first reservation wins");
    assert!(
        shared.phase() == singularity_protocol::SessionPhase::Reserved,
        "reservation is a busy window"
    );
    assert!(
        shared.reserve_start().is_err(),
        "second reservation must be rejected"
    );
    let mut sink = |_event: TurnEvent| {};
    assert!(
        shared.run_turn("must not run", &mut sink).is_err(),
        "run_turn must be rejected while a reservation holds the window"
    );
    assert!(shared.steer("not running yet").is_err());
    assert!(shared.abort().is_err());
    assert!(
        shared.submit_follow_up("queued while reserved").is_err(),
        "followUp is rejected during Reserved (no writer yet)"
    );
    shared
        .update_settings("openai_compatible/base-model")
        .expect("apply settings during reservation");
    assert_eq!(
        shared.thread().model.as_deref(),
        Some("openai_compatible/base-model"),
        "commit point only updates the in-memory projection"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "accepted settings are durable before the next turn"
    );

    // 未消费的预订 drop 后窗口释放；Reserved 期间被拒绝的 followUp 不再
    // 出现在后续链中（其接受需要活动 turn 的共享写者）。
    drop(reservation);
    assert!(shared.phase() == singularity_protocol::SessionPhase::Idle);
    let outcome = shared.run_turn("now it runs", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert!(shared.snapshot().pending_controls.is_empty());
    assert_eq!(
        input_sequence(&provider.requests()),
        vec!["now it runs".to_string()],
        "the follow-up rejected during Reserved never reaches a model step"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "execution preserves the saved selector without appending it again"
    );
}

/// 运行中改设置走与空闲时同一条提交路径：写者锁被活动 turn 占用时提交点
/// 仍只更新内存投影（不写文件、不报错），落盘由下一 turn 开始时记录（turn 边界记录）。
#[test]
fn settings_update_is_durable_immediately_and_keeps_the_active_model_frozen() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let conversation = new_conversation(
        &fixture,
        gate as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = conversation.thread().thread_id;

    let mut sink = |_event: TurnEvent| {};
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || conversation.run_turn("first", &mut sink))
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("turn reaches the provider");

    conversation
        .update_settings("openai_compatible/base-model-2")
        .expect("mid-turn settings update is accepted");
    conversation
        .update_settings("openai_compatible/base-model-2")
        .expect("unchanged settings");
    assert_eq!(
        conversation.thread().model.as_deref(),
        Some("openai_compatible/base-model-2"),
        "in-memory projection is updated while the turn holds the writer lock"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        2,
        "the new selector is durable while the first turn is still running"
    );

    assert_eq!(
        last_recorded_selector(&sessions, &thread_id).as_deref(),
        Some("openai_compatible/base-model-2")
    );
    release_tx.send(()).expect("gate release");
    let outcome = worker.join().expect("turn thread").expect("turn ok");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    let mut sink = |_event: TurnEvent| {};
    let outcome = conversation.run_turn("second", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        2,
        "the next turn does not duplicate the already persisted selector"
    );
    assert_eq!(
        last_recorded_selector(&sessions, &thread_id).as_deref(),
        Some("openai_compatible/base-model-2"),
        "resume projection (last-wins) shows the mid-turn change"
    );
}

#[test]
fn failed_compaction_closes_its_durable_operation() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    // 摘要输出不进入对话流：已发射的部分摘要不影响可重试性，这一次请求按自身
    // 的 attempt 预算重试，最终仍以真实失败结束。
    let summary_failure = || {
        ScriptedAttempt::visible_then_fail(
            "partial summary",
            ProviderError::new(ModelErrorKind::NetworkError, "summary request failed")
                .with_retry_after(Some(std::time::Duration::from_millis(1))),
        )
    };
    let conversation = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::new([
            summary_failure(),
            summary_failure(),
            summary_failure(),
        ])),
        None,
    );
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    let error = conversation
        .reserve_compaction()
        .and_then(|mut reservation| reservation.compact())
        .expect("failure terminal is persisted");
    assert_eq!(error.status, TurnStatus::Failed);
    assert_eq!(
        error.error.unwrap().cause,
        crate::TurnFailureCause::ProviderNetwork
    );

    let finished: Vec<TurnStatus> = ledger_of(&sessions, &thread_id)
        .into_iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished {
                turn_id: None,
                outcome,
                ..
            } => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec![TurnStatus::Failed]);
}

#[test]
fn invalid_compaction_response_preserves_its_validation_source() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::new([ScriptedAttempt::success("")])),
        None,
    );
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    let error = conversation
        .reserve_compaction()
        .and_then(|mut reservation| reservation.compact())
        .expect("failure terminal is persisted");
    assert_eq!(error.status, TurnStatus::Failed);
    assert!(
        error
            .error
            .unwrap()
            .message
            .contains("summary contains no text")
    );

    // 失败原因随同一份 operation 终态落盘：重新打开 JSONL 仍能定位这次压缩
    // 为什么失败，而不是只看到一次 provider 请求与无原因 Failed。
    let durable = SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen the session file")
        .entries()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Record {
                record:
                    singularity_agent::session::LedgerRecord::OperationFinished {
                        turn_id: None,
                        outcome,
                        error,
                        ..
                    },
                ..
            } => Some((*outcome, error.clone())),
            _ => None,
        })
        .expect("one compaction terminal");
    assert_eq!(durable.0, TurnStatus::Failed);
    let detail = durable.1.expect("a failed compaction keeps its reason");
    assert!(
        detail.message.contains("summary contains no text"),
        "{detail:?}"
    );
}

#[test]
fn compaction_start_append_failure_preserves_the_storage_stage() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::new([ScriptedAttempt::success("summary")])),
        None,
    );
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let mut reservation = conversation
        .reserve_compaction()
        .expect("reserve compaction");
    std::fs::remove_file(&path).expect("remove session file");
    std::fs::create_dir(&path).expect("replace session file with a directory");

    let error = reservation
        .compact()
        .expect_err("the operation start cannot be persisted");
    assert!(matches!(
        error,
        crate::ConversationError::Compaction(crate::CompactionRunError::Start(_))
    ));
}

#[test]
fn compaction_terminal_append_failure_is_not_reported_as_execution() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let inner = Arc::new(ScriptedProvider::new([ScriptedAttempt::success("summary")]));
    let (gate, started_rx) = GatedProvider::new(inner);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let conversation = new_conversation(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            conversation
                .reserve_compaction()
                .and_then(|mut reservation| reservation.compact())
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("compaction reaches provider");
    std::fs::remove_file(&path).expect("remove session file");
    std::fs::create_dir(&path).expect("replace session file with a directory");
    release_tx.send(()).expect("release provider");

    let error = worker
        .join()
        .expect("compaction thread")
        .expect_err("the terminal cannot be persisted");
    assert!(matches!(
        error,
        crate::ConversationError::Compaction(crate::CompactionRunError::Terminalization(_))
    ));
}

#[test]
fn cancelled_compaction_is_reported_as_interrupted() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let (gate, started_rx) = GatedProvider::stop_gate();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let conversation = new_conversation(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            conversation
                .reserve_compaction()
                .and_then(|mut reservation| reservation.compact())
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("compaction reaches provider");
    conversation
        .abort()
        .expect("stop is accepted at the provider gate");
    release_tx.send(()).expect("release provider");
    let error = worker
        .join()
        .expect("compaction thread")
        .expect("interrupted terminal is persisted");
    assert_eq!(error.status, TurnStatus::Interrupted);
    assert!(error.error.is_none());

    let finished: Vec<(TurnStatus, bool)> = ledger_of(&sessions, &thread_id)
        .into_iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished {
                turn_id: None,
                outcome,
                user_stopped,
                ..
            } => Some((outcome, user_stopped)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(TurnStatus::Interrupted, true)],
        "an accepted stop is part of the compaction terminal, not re-derived later"
    );
}

/// 停止与真实 provider 错误同时发生：停止进入终态事实，鉴权错误仍按自身类别
/// 上报，不被取消令牌改写（请求层只分类一次，压缩直接传播）。
#[test]
fn an_accepted_stop_does_not_rewrite_a_real_compaction_failure() {
    /// 在返回真实鉴权错误之前先取消本轮令牌：等价于「用户在请求返回前按下
    /// 停止」，两种事实同时存在。
    struct StopThenAuthError;

    impl Provider for StopThenAuthError {
        fn model_configuration(&self) -> ModelConfigurationSnapshot {
            crate::test_support::test_model_configuration()
        }

        fn complete_stream(
            &self,
            _request: &ModelTurnRequest,
            cancellation: &singularity_core::CancellationToken,
            _on_event: &mut dyn FnMut(ProviderStreamEvent),
            _record_attempt: &mut dyn FnMut(ProviderAttemptEvent) -> std::io::Result<()>,
        ) -> Result<ModelTurnResponse, ProviderCallError> {
            cancellation.cancel();
            Err(
                ProviderError::new(ModelErrorKind::AuthError, "compaction credentials rejected")
                    .into(),
            )
        }
    }

    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(&fixture, Arc::new(StopThenAuthError), None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    let error = conversation
        .reserve_compaction()
        .and_then(|mut reservation| reservation.compact())
        .expect("failure terminal is persisted");
    assert_eq!(error.status, TurnStatus::Failed);
    assert_eq!(
        error.error.unwrap().cause,
        crate::TurnFailureCause::ProviderAuth
    );

    let durable = SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen the session file")
        .entries()
        .iter()
        .find_map(|entry| match entry {
            SessionEntry::Record {
                record:
                    singularity_agent::session::LedgerRecord::OperationFinished {
                        turn_id: None,
                        outcome,
                        user_stopped,
                        error,
                        ..
                    },
                ..
            } => Some((*outcome, *user_stopped, error.clone())),
            _ => None,
        })
        .expect("one compaction terminal");
    assert_eq!(durable.0, TurnStatus::Failed);
    assert!(durable.1, "the accepted stop stays a separate fact");
    let detail = durable
        .2
        .expect("the compaction terminal keeps the real failure reason");
    assert_eq!(
        detail.cause,
        singularity_protocol::TurnFailureCause::ProviderAuth
    );
    assert!(
        detail.message.contains("credentials rejected"),
        "{detail:?}"
    );
}

/// 独立压缩的停止接受窗口在提交边界关闭：终态提交完成后，stop 报告操作
/// 已结束，最终记录也不再接受一个无法反映的停止。
#[test]
fn compaction_stop_window_closes_at_its_commit_boundary() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "summary text",
    )]));
    let conversation =
        new_conversation(&fixture, provider as Arc<dyn Provider + Send + Sync>, None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    let mut reservation = conversation
        .reserve_compaction()
        .expect("reserve compaction");
    let outcome = reservation.compact().expect("compaction completes");
    assert_eq!(outcome.status, TurnStatus::Completed);
    assert!(outcome.reduced);
    assert!(outcome.terminal().is_none());
    assert!(
        matches!(
            conversation.abort(),
            Err(crate::ConversationControlError::NotRunning)
        ),
        "a stop after the commit boundary is reported as already finished"
    );
    drop(reservation);

    let finished: Vec<(TurnStatus, bool)> = ledger_of(&sessions, &thread_id)
        .into_iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished {
                turn_id: None,
                outcome,
                user_stopped,
                ..
            } => Some((outcome, user_stopped)),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec![(TurnStatus::Completed, false)]);
}

/// Agent 已返回成功、冻结边界之前接受停止：日志、调用返回值消费同一次冻结
/// 事实，不出现「日志 Interrupted、调用结果成功」的分裂。
#[test]
fn a_stop_accepted_after_a_successful_compaction_is_reported_as_interrupted() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let provider = Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "summary text",
    )]));
    let conversation =
        new_conversation(&fixture, provider as Arc<dyn Provider + Send + Sync>, None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    // 确定性停在「Agent 已成功、提交边界尚未冻结」这一刻，此时接受停止。
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let boundary_release = Arc::clone(&release);
    {
        let conversation = Arc::clone(&conversation);
        conversation
            .runner_handle()
            .pause_next_compaction_commit(Arc::new(move || {
                let _ = reached_tx.send(());
                boundary_release.wait();
                conversation
                    .abort()
                    .expect("the stop is accepted before the boundary freezes");
            }));
    }
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            conversation
                .reserve_compaction()
                .and_then(|mut reservation| reservation.compact())
        })
    };
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("compaction reaches its commit boundary");
    release.wait();

    let error = worker
        .join()
        .expect("compaction thread")
        .expect("interrupted terminal is persisted");
    assert_eq!(error.status, TurnStatus::Interrupted);
    assert!(error.error.is_none());

    let finished: Vec<(TurnStatus, bool)> = ledger_of(&sessions, &thread_id)
        .into_iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished {
                turn_id: None,
                outcome,
                user_stopped,
                ..
            } => Some((outcome, user_stopped)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(TurnStatus::Interrupted, true)],
        "the durable terminal is interrupted even though the agent returned success"
    );
}

/// 启动失败与已接受停止的组合：未送达输入不再归还，处置事件带同一控制身份。
/// 直接驱动 TurnRunner::run，使「启动准备失败」与「停止已接受」在同一轮内
/// 确定发生（经 Conversation 时这两件事之间的窗口只有微秒级，无法确定性复现）。
#[test]
fn a_start_failure_keeps_the_frozen_stop_fact() {
    use crate::conversation::TurnControls;
    use singularity_agent::agent::{ControlRequest, TurnInbox, control_id};
    use singularity_agent::session::{ExpectedSession, SessionAccess, SessionManager};
    use singularity_protocol::{ControlChannel, ControlDisposition, TurnEvent};

    let fixture = SessionsFixture::new();
    let mut thread = fixture
        .catalog()
        .create_thread(&crate::test_support::cwd(), None)
        .expect("create");
    let session = SessionManager::open_existing_with_access(
        &fixture.dir.join(format!("{}.jsonl", thread.thread_id)),
        &fixture.coordinator,
        ExpectedSession {
            id: &thread.thread_id,
            cwd: None,
        },
        SessionAccess::Append,
    )
    .expect("open writer");
    let controls = TurnControls::new(
        "turn-1",
        TurnInbox::default_handle(),
        Arc::new(std::sync::Mutex::new(session)),
    );
    // 停止已接受：取消写入与 freeze 的读取在同一临界区内，冻结事实为 true。
    controls.cancellation().cancel();
    // 会话文件本身可用，但工作目录不可用：start_turn 的准备阶段必然失败。
    thread.cwd = fixture
        .home()
        .join("missing-workspace")
        .to_string_lossy()
        .to_string();
    let input = ControlRequest {
        control_id: control_id(ControlChannel::Submit, 0),
        turn_id: None,
        channel: ControlChannel::Submit,
        sequence: 0,
        text: "go".to_string(),
    };
    let mut events = Vec::new();
    let result = fixture
        .runner(None)
        .run(input, &thread, &controls, &mut |event| events.push(event));

    assert!(
        result.result.is_err(),
        "the start must fail: {:?}",
        result.result
    );
    assert!(
        result.cancel_accepted,
        "the frozen stop fact survives a start failure"
    );
    assert_eq!(result.undelivered.len(), 1);
    assert_eq!(result.undelivered[0].control_id, "submit:0");
    assert!(
        matches!(
            events.as_slice(),
            [TurnEvent::ControlChanged { control }]
                if control.control_id == "submit:0"
                    && control.disposition == ControlDisposition::Cancelled
        ),
        "the undelivered input is dispositioned as cancelled: {events:?}"
    );
}

#[test]
fn resume_thread_conflicts_with_active_writer_and_succeeds_after_release() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let thread_id = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    let shared = coordinator();
    let session = SessionManager::create_with_id_with_coordinator(
        Path::new("."),
        &sessions,
        thread_id,
        &shared,
    )
    .expect("create session file");

    // 同一会话已有存活写者（模拟另一进程持有锁）：resume 必须快速失败。
    let cwd = session.cwd_string();
    let catalog = ThreadCatalog::new(sessions, shared);
    assert!(matches!(
        catalog.resume_thread(thread_id, &cwd),
        Err(crate::thread_catalog::CatalogError::WriterActive)
    ));

    // 写者释放后 resume 恢复正常。
    drop(session);
    let resumed = catalog
        .resume_thread(thread_id, &cwd)
        .expect("resume after release");
    assert_eq!(resumed.thread_id, thread_id);
}

#[test]
fn read_source_range_travels_from_the_tool_to_the_history_page() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let path = fixture.home().join("lines.txt");
    std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        // offset=0 与省略等价：实际从第 1 行开始；limit 只取两行。
        ScriptedAttempt::tool_call(
            "call",
            "read",
            serde_json::json!({"path": path, "offset": 0, "limit": 2}),
        ),
        ScriptedAttempt::success("done"),
    ]));
    let conversation = new_conversation(&fixture, provider, None);
    let mut live = Vec::new();
    conversation
        .run_turn("read", &mut |event| {
            if let TurnEvent::ToolExecutionEnd { read_source, .. } = event {
                live.push(read_source);
            }
        })
        .unwrap();
    let expected = singularity_protocol::ReadSource {
        start_line: 1,
        line_count: 2,
    };
    assert_eq!(live, vec![Some(expected)]);
    let catalog = ThreadCatalog::new(sessions, Arc::clone(&fixture.coordinator));
    let page = catalog
        .read_snapshot(&conversation.thread().thread_id)
        .unwrap()
        .page(40, None)
        .unwrap();
    let persisted = page
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .find_map(|item| match item {
            singularity_protocol::HistoryItem::ToolResult { read_source, .. } => Some(*read_source),
            _ => None,
        });
    assert_eq!(
        persisted,
        Some(Some(expected)),
        "实时事件与持久历史给出同一份真实读取范围"
    );
}

#[test]
fn reused_provider_tool_ids_have_distinct_live_and_historical_items() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let first = fixture.home().join("first.txt");
    let second = fixture.home().join("second.txt");
    std::fs::write(&first, "first output").unwrap();
    std::fs::write(&second, "second output").unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call("reused", "read", serde_json::json!({"path": first})),
        ScriptedAttempt::tool_call("reused", "read", serde_json::json!({"path": second})),
        ScriptedAttempt::success("done"),
    ]));
    let conversation = new_conversation(&fixture, provider.clone(), None);
    let mut completed = Vec::new();
    conversation
        .run_turn("read both", &mut |event| {
            if let TurnEvent::ToolExecutionEnd { item, output, .. } = event {
                completed.push((item.item_id, output));
            }
        })
        .unwrap();
    assert_eq!(completed.len(), 2);
    assert_ne!(completed[0].0, completed[1].0);
    let catalog = ThreadCatalog::new(sessions, Arc::clone(&fixture.coordinator));
    let snapshot = catalog
        .read_snapshot(&conversation.thread().thread_id)
        .unwrap();
    let page = snapshot.page(40, None).unwrap();
    let results = page
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| {
            if let singularity_protocol::HistoryItem::ToolResult { id, output, .. } = item {
                Some((id.as_str(), output.as_str()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![
            (completed[0].0.as_str(), "first output"),
            (completed[1].0.as_str(), "second output")
        ]
    );
    let requests = provider.requests();
    let raw_ids = requests[2]
        .messages
        .iter()
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(
        raw_ids,
        vec!["reused", "reused"],
        "provider replay keeps its original wire IDs"
    );
}

/// 同一次执行的两个公开出口共享用户消息身份：实时事件与公开历史都由
/// 生产者按条目首个文本块的身份发布，客户端不再自行拼接 id。
#[test]
fn user_message_events_and_public_history_share_one_content_identity() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("done"),
        ScriptedAttempt::success("done"),
    ]));
    let conversation = new_conversation(&fixture, provider, None);
    let mut event_ids = Vec::new();
    for text in ["first input", "first input"] {
        conversation
            .run_turn(text, &mut |event| {
                if let TurnEvent::UserMessage { item, .. } = event {
                    event_ids.push(item.item_id);
                }
            })
            .unwrap();
    }
    assert_eq!(event_ids.len(), 2);
    let catalog = ThreadCatalog::new(sessions, Arc::clone(&fixture.coordinator));
    let snapshot = catalog
        .read_snapshot(&conversation.thread().thread_id)
        .unwrap();
    let page = snapshot.page(40, None).unwrap();
    let user_ids = page
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            singularity_protocol::HistoryItem::Message { id, role, .. } if role == "user" => {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        user_ids, event_ids,
        "live events and public history share one user content identity"
    );
    assert_ne!(
        user_ids[0], user_ids[1],
        "repeated text keeps distinct identity"
    );
}

/// model_configuration 可变的 scripted provider：模拟配置刷新只改变后续
/// turn 解析出的有效窗口，用于核对活动 turn 的冻结事实不受其影响。
struct MutableLimitsProvider {
    inner: ScriptedProvider,
    context_tokens: std::sync::atomic::AtomicU32,
}

impl MutableLimitsProvider {
    fn set_context_tokens(&self, tokens: u32) {
        self.context_tokens
            .store(tokens, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Provider for MutableLimitsProvider {
    fn model_configuration(&self) -> singularity_model::ModelConfigurationSnapshot {
        singularity_model::ModelConfigurationSnapshot {
            max_context_tokens: self
                .context_tokens
                .load(std::sync::atomic::Ordering::SeqCst),
            max_output_tokens: 4_096,
        }
    }

    fn complete_stream(
        &self,
        request: &singularity_model::ModelTurnRequest,
        cancellation: &singularity_core::CancellationToken,
        on_event: &mut dyn FnMut(singularity_model::ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(
            singularity_model::ProviderAttemptEvent,
        ) -> std::io::Result<()>,
    ) -> Result<singularity_model::ModelTurnResponse, singularity_model::ProviderCallError> {
        Provider::complete_stream(&self.inner, request, cancellation, on_event, record_attempt)
    }
}

/// 运行中的 turn 报告其冻结的有效上下文窗口：配置刷新不改变当前执行的
/// 解释，后续执行采用新值；空闲后保留最近一次执行的事实。
#[test]
fn running_turn_keeps_its_frozen_window_across_configuration_refresh() {
    use std::sync::atomic::AtomicU32;

    let fixture = SessionsFixture::new();
    let provider = Arc::new(MutableLimitsProvider {
        inner: ScriptedProvider::new([
            ScriptedAttempt::success("first"),
            ScriptedAttempt::success("second"),
        ]),
        context_tokens: AtomicU32::new(100_000),
    });
    let (gated, started) = GatedProvider::new(provider.clone());
    let (release, release_receiver) = std::sync::mpsc::channel::<()>();
    gated.with_release(release_receiver);
    let conversation = new_conversation(&fixture, gated, None);
    let sink = |_event: TurnEvent| {};
    let running = {
        let conversation = Arc::clone(&conversation);
        let mut sink = sink;
        std::thread::spawn(move || conversation.run_turn("first", &mut sink).unwrap())
    };
    started
        .recv()
        .expect("the first request reaches the provider");
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(100_000),
        "the running turn reports the window frozen at its start"
    );
    provider.set_context_tokens(200_000);
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(100_000),
        "a configuration refresh never reinterprets the running execution"
    );
    release.send(()).expect("release the gated request");
    let outcome = running.join().unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(100_000),
        "the latest executed turn's window stays observable while idle"
    );
    conversation
        .run_turn("second", &mut |_event: TurnEvent| {})
        .unwrap();
    assert_eq!(
        conversation.snapshot().model_context_window,
        Some(200_000),
        "the next execution resolves and freezes the refreshed configuration"
    );
}

/// 首次请求成功并携带 usage（调用未注册工具迫使循环续接），第二次请求失败：
/// 失败终态事件必须报告本轮已记录的 usage（回归：失败终态曾以空 usage 出口）。
#[test]
fn failed_turn_reports_usage_recorded_before_the_failure() {
    let fixture = SessionsFixture::new();
    let provider = ScriptedProvider::new([
        ScriptedAttempt::ToolCalls {
            text: "calling a tool".to_string(),
            calls: vec![singularity_model::ModelToolCall {
                tool_call_id: "call-1".to_string(),
                tool_name: "definitely-not-a-registered-tool".to_string(),
                arguments: serde_json::json!({}),
            }],
            usage: Some(singularity_model::ModelUsage {
                input_tokens: 10,
                output_tokens: 32,
                total_tokens: 42,
                usage_present: true,
                ..Default::default()
            }),
        },
        // 网络错误可自动重试：按重试预算（initial + 2 retries）逐次失败收敛。
        ScriptedAttempt::failure_kind(ModelErrorKind::NetworkError, "connection reset"),
        ScriptedAttempt::failure_kind(ModelErrorKind::NetworkError, "connection reset"),
        ScriptedAttempt::failure_kind(ModelErrorKind::NetworkError, "connection reset"),
    ]);
    let conversation = new_conversation(&fixture, Arc::new(provider), None);
    let mut sink = |_event: TurnEvent| {};
    let outcome = conversation
        .run_turn("go", &mut sink)
        .expect("a converged failed terminal is a trusted Ok outcome");
    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    let usage = outcome
        .usage
        .usage_present
        .then_some(&outcome.usage)
        .expect("the failed outcome carries the usage recorded before the failure");
    assert_eq!(usage.total_tokens, 42);
    let error = outcome
        .error
        .expect("failed terminal carries protocol error detail");
    assert_eq!(
        error.cause,
        crate::TurnFailureCause::ProviderNetwork,
        "the error detail names the real provider cause"
    );
}

/// 读取指定 thread 会话文件的全部 ledger 记录（只读，不修复）。
fn ledger_of(sessions: &Path, thread_id: &str) -> Vec<singularity_agent::session::LedgerRecord> {
    SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .ledger_records()
}

/// 工具执行边界的中断测试：bash 进程产生部分流式输出后仍在运行，
/// 此时触发中断，验证子进程树被正常终止、工具以模型可见失败闭合、
/// operation 收敛为 interrupted，且未完成副作用绝不被自动重放，下一条输入可正常开启新轮次。
#[test]
fn interruption_at_tool_boundary_converges_interrupted_and_next_input_runs() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call(
            "call-bash",
            "bash",
            serde_json::json!({"command": "echo ready; sleep 30"}),
        ),
        ScriptedAttempt::success("next turn done"),
    ]));
    let conversation = new_conversation(
        &fixture,
        provider as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = conversation.thread().thread_id;

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let ready_tx = std::sync::Mutex::new(Some(ready_tx));
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = move |event: TurnEvent| {
                if let TurnEvent::ToolExecutionUpdate {
                    ref partial_result, ..
                } = event
                    && partial_result.contains("ready")
                    && let Some(sender) = ready_tx.lock().expect("ready lock").take()
                {
                    let _ = sender.send(());
                }
            };
            let outcome = conversation.run_turn("run a long command", &mut sink);
            (conversation, outcome)
        })
    };
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("tool is executing and has streamed output");
    conversation.abort().expect("abort active turn");
    let (conversation, outcome) = worker.join().expect("worker");

    let outcome = outcome.expect("tool-boundary interruption converges as interrupted");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);

    let session = SessionData::open(&sessions.join(format!("{thread_id}.jsonl"))).expect("reopen");
    let records = session.ledger_records();
    let aborted_results = session
        .entries()
        .iter()
        .filter(|entry| {
            matches!(entry,
                singularity_agent::session::SessionEntry::Message { message, .. }
                    if matches!(message, singularity_agent::message::AgentMessage::ToolResult { .. })
                        && message.content_text().contains("Operation aborted"))
        })
        .count();
    assert_eq!(
        aborted_results, 1,
        "the interrupted tool closes with exactly one model-visible failure"
    );
    let terminals: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                singularity_agent::session::LedgerRecord::OperationFinished { .. }
            )
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one durable terminal outcome");
    assert!(matches!(
        terminals[0],
        singularity_agent::session::LedgerRecord::OperationFinished {
            outcome: TurnStatus::Interrupted,
            ..
        }
    ));

    // 中断不破坏协调器：下一条输入作为新 turn 正常完成。
    let mut sink = |_event: TurnEvent| {};
    let next = conversation
        .run_turn("continue", &mut sink)
        .expect("next input runs after a tool-boundary interruption");
    assert_eq!(next.turn_status, TurnStatus::Completed);
    let completed = SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen completed turn");
    assert!(completed.entries().iter().any(|entry| matches!(entry,
        singularity_agent::session::SessionEntry::Message { message, .. }
        if message.content_text() == "next turn done"
    )));
}

#[test]
fn settings_survive_reopen_without_a_turn_and_failed_saves_preserve_selection() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::ok("ok")),
        Some("openai_compatible/base-model"),
    );
    let id = conversation.thread().thread_id;
    conversation
        .update_settings("openai_compatible/base-model-2")
        .unwrap();
    let catalog = ThreadCatalog::new(sessions, Arc::clone(&fixture.coordinator));
    let cwd = conversation.thread().cwd;
    assert_eq!(
        catalog.resume_thread(&id, &cwd).unwrap().model.as_deref(),
        Some("openai_compatible/base-model-2")
    );
    let writer = conversation
        .runner_handle()
        .open_turn_writer(&conversation.thread())
        .unwrap();
    let failed = conversation.update_settings("openai_compatible/base-model");
    assert!(failed.is_err());
    assert_eq!(
        conversation.thread().model.as_deref(),
        Some("openai_compatible/base-model-2")
    );
    drop(writer);
    assert_eq!(
        catalog.resume_thread(&id, &cwd).unwrap().model,
        conversation.thread().model
    );
}

#[test]
fn compaction_uses_the_same_busy_window_and_settings_writer() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(
        &fixture,
        Arc::new(ScriptedProvider::ok("ok")),
        Some("openai_compatible/base-model"),
    );
    let reservation = conversation.reserve_compaction().unwrap();
    assert_eq!(
        conversation.phase(),
        singularity_protocol::SessionPhase::Compacting
    );
    assert!(conversation.reserve_start().is_err());
    conversation
        .update_settings("openai_compatible/base-model-2")
        .unwrap();
    assert_eq!(
        last_recorded_selector(&sessions, &conversation.thread().thread_id).as_deref(),
        Some("openai_compatible/base-model-2")
    );
    conversation.abort().unwrap();
    assert_eq!(
        conversation.phase(),
        singularity_protocol::SessionPhase::Stopping
    );
    drop(reservation);
    assert_eq!(
        conversation.phase(),
        singularity_protocol::SessionPhase::Idle
    );
}

#[test]
fn interrupted_output_reloads_for_display_without_entering_the_next_request() {
    let fixture = SessionsFixture::new();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::visible_then_fail(
            "interrupted output",
            ProviderError::new(ModelErrorKind::JsonSchemaViolation, "invalid stream"),
        ),
        ScriptedAttempt::success("next answer"),
    ]));
    let conversation = new_conversation(&fixture, provider.clone(), None);
    let outcome = conversation.run_turn("first", &mut |_| {}).unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Failed);
    let thread_id = conversation.thread().thread_id;
    let snapshot = fixture.catalog().read_snapshot(&thread_id).unwrap();
    let page = snapshot.page(10, None).unwrap();
    assert!(
        page.turns
            .iter()
            .flat_map(|turn| &turn.items)
            .any(|item| matches!(item,
        singularity_protocol::HistoryItem::Message { role, text, .. }
        if role == "assistant" && text == "interrupted output"))
    );
    conversation.run_turn("continue", &mut |_| {}).unwrap();
    assert!(
        !provider.requests()[1]
            .messages
            .iter()
            .any(|message| message.content.contains("interrupted output"))
    );
}
