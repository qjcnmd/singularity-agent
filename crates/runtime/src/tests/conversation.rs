//! 协调器的并发、持久化故障与恢复行为。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::path::Path;
use std::sync::Arc;

use crate::Conversation;
use crate::ThreadCatalog;
use crate::test_support::{
    GatedProvider, SessionsFixture, conversation_with, seed_compaction_history,
};
use singularity_agent::session::{SessionData, SessionEntry, SessionMetadata};
use singularity_model::{
    ModelErrorKind, Provider, ProviderError,
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

fn run_compaction(
    reservation: &mut crate::OperationReservation,
) -> Result<(), crate::ConversationError> {
    match crate::test_support::run_async(reservation.execute(&mut |_| {})) {
        crate::OperationResult::Compaction(result) => result,
        crate::OperationResult::Turn(_) => panic!("expected the reserved compaction"),
    }
}

fn thread_settings_count(sessions: &std::path::Path, thread_id: &str) -> usize {
    SessionData::open(&sessions.join(singularity_agent::session::session_file_name(thread_id)))
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
    SessionData::open(&sessions.join(singularity_agent::session::session_file_name(thread_id)))
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

/// 运行中修改名称与设置立即落盘；当前请求继续使用已经冻结的模型。
#[test]
fn settings_update_is_durable_immediately_and_keeps_the_active_model_frozen() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let (gate, started_rx) = GatedProvider::new(Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("done"),
        ScriptedAttempt::success("done"),
    ])));
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
        .rename("  running task  ")
        .expect("mid-turn rename");
    assert_eq!(
        fixture
            .catalog()
            .read_snapshot(&thread_id)
            .unwrap()
            .summary
            .title
            .as_deref(),
        Some("running task"),
        "the trimmed name is durable before the running turn completes"
    );
    conversation
        .update_settings("openai_compatible/base-model-2")
        .expect("mid-turn settings update is accepted");
    conversation
        .update_settings("openai_compatible/base-model-2")
        .expect("unchanged settings");
    assert_eq!(
        conversation.thread().model.as_deref(),
        Some("openai_compatible/base-model-2"),
        "in-memory projection is updated while the first turn is still running"
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
    seed_compaction_history(&fixture, &thread_id);

    {
        let mut reservation = conversation.reserve_compaction().unwrap();
        conversation
            .rename("compacting task")
            .expect("compaction writer accepts rename");
        run_compaction(&mut reservation).expect("failure terminal is persisted");
    }
    assert_eq!(
        fixture
            .catalog()
            .read_snapshot(&thread_id)
            .unwrap()
            .summary
            .title
            .as_deref(),
        Some("compacting task")
    );

    let finished: Vec<TurnStatus> = ledger_of(&sessions, &thread_id)
        .into_iter()
        .filter_map(|record| match record {
            singularity_agent::session::LedgerRecord::OperationFinished {
                turn_id: None,
                outcome,
                error,
                ..
            } => {
                assert_eq!(
                    error.unwrap().cause,
                    crate::TurnFailureCause::ProviderNetwork
                );
                Some(outcome)
            }
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
    seed_compaction_history(&fixture, &thread_id);

    conversation
        .reserve_compaction()
        .and_then(|mut reservation| run_compaction(&mut reservation))
        .expect("failure terminal is persisted");

    // 失败原因随同一份 operation 终态落盘：重新打开 JSONL 仍能定位这次压缩
    // 为什么失败，而不是只看到一次 provider 请求与无原因 Failed。
    let durable = SessionData::open(
        &sessions.join(singularity_agent::session::session_file_name(&thread_id)),
    )
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
fn compaction_summary_append_failure_stops_execution() {
    let fixture = SessionsFixture::new();
    let sessions = fixture.dir.clone();
    let inner = Arc::new(ScriptedProvider::new([ScriptedAttempt::success("summary")]));
    let (gate, started_rx) = GatedProvider::new(inner);
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let conversation = new_conversation(&fixture, gate as Arc<dyn Provider + Send + Sync>, None);
    let thread_id = conversation.thread().thread_id;
    seed_compaction_history(&fixture, &thread_id);
    let path = sessions.join(singularity_agent::session::session_file_name(&thread_id));
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            conversation
                .reserve_compaction()
                .and_then(|mut reservation| run_compaction(&mut reservation))
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
        .expect_err("the summary cannot be persisted");
    assert!(matches!(
        error,
        crate::ConversationError::Turn(crate::TurnRunError::Execution(_))
    ));
}

fn ledger_of(sessions: &Path, thread_id: &str) -> Vec<singularity_agent::session::LedgerRecord> {
    SessionData::open(&sessions.join(singularity_agent::session::session_file_name(thread_id)))
        .expect("reopen")
        .ledger_records()
}

/// 工具执行边界的中断测试：bash 进程产生部分流式输出后仍在运行，
/// 此时触发中断，验证子进程树被正常终止、工具以模型可见失败闭合、
/// operation 收敛为 interrupted，且未完成副作用绝不被自动重放，下一条输入可正常开启新轮次。
mod turn_outcomes;
