//! 协调器的并发、持久化故障与恢复行为。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::path::Path;
use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::test_support::{
    GatedProvider, SessionsFixture, conversation_with, coordinator, seed_compaction_history,
    temp_sessions,
};
use singularity_agent::session::{SessionData, SessionEntry, SessionManager, SessionMetadata};
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, Provider, ProviderError,
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
        let _ = crate::test_support::run_async(conversation.run_turn("hello", &mut sink));
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
            .and_then(|mut reservation| crate::test_support::run_async(reservation.compact()));
    }));
    assert!(panic.is_err(), "the provider panic must propagate");
    assert!(
        compacting.phase() == singularity_protocol::SessionPhase::Idle,
        "compaction must release the single-writer window while unwinding"
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
        std::thread::spawn(move || {
            crate::test_support::run_async(conversation.run_turn("first", &mut sink))
        })
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
    let outcome =
        crate::test_support::run_async(conversation.run_turn("second", &mut sink)).expect("runs");
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
        .and_then(|mut reservation| crate::test_support::run_async(reservation.compact()))
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
        .and_then(|mut reservation| crate::test_support::run_async(reservation.compact()))
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
                .and_then(|mut reservation| crate::test_support::run_async(reservation.compact()))
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

        fn complete_stream<'a>(
            &'a self,
            _request: &'a ModelTurnRequest,
            cancellation: &'a tokio_util::sync::CancellationToken,
            _observer: &'a mut dyn singularity_model::ProviderObserver,
        ) -> singularity_model::ProviderFuture<'a> {
            Box::pin(async move {
                cancellation.cancel();
                Err(ProviderError::new(
                    ModelErrorKind::AuthError,
                    "compaction credentials rejected",
                )
                .into())
            })
        }
    }

    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let conversation = new_conversation(&fixture, Arc::new(StopThenAuthError), None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&sessions, &thread_id);

    let error = conversation
        .reserve_compaction()
        .and_then(|mut reservation| crate::test_support::run_async(reservation.compact()))
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
                .and_then(|mut reservation| crate::test_support::run_async(reservation.compact()))
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

fn ledger_of(sessions: &Path, thread_id: &str) -> Vec<singularity_agent::session::LedgerRecord> {
    SessionData::open(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .ledger_records()
}

/// 工具执行边界的中断测试：bash 进程产生部分流式输出后仍在运行，
/// 此时触发中断，验证子进程树被正常终止、工具以模型可见失败闭合、
/// operation 收敛为 interrupted，且未完成副作用绝不被自动重放，下一条输入可正常开启新轮次。
mod turn_outcomes;
