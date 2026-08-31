//! 协调器的并发安全护栏：panic 与锁中毒路径的窗口释放、单写者锁冲突、
//! 预订窗口的回收、写者锁占用下设置提交仍被接受。turn 链行为（lifecycle
//! 事件、steer/followUp）的行为回归由评估器与真实使用兜底，不在此重复。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::path::Path;
use std::sync::Arc;

use crate::events::TurnEvent;
use crate::objects::TurnStatus;
use crate::runner::TurnRunner;
use crate::store::{create_thread, resume_thread};
use crate::test_support::{
    StopGateProvider, coordinator, input_sequence, provider_snapshot, temp_sessions,
    test_model_configuration,
};
use crate::{Conversation, SettingsApplyTiming, SettingsPatch};
use singularity_agent::message::{AgentMessage, AgentMessageRole};
use singularity_agent::session::{SessionManager, SessionMetadata, SessionMetadataKind};
use singularity_model::{
    ModelConfigurationSnapshot, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider,
    ProviderError,
    test_support::{ScriptedAttempt, ScriptedProvider},
};

/// 收集 turn/started 事件的完整 turn id 序列。
#[derive(Clone, Default)]
struct EventCollector {
    methods: Arc<std::sync::Mutex<Vec<&'static str>>>,
    started_turn_ids: Arc<std::sync::Mutex<Vec<String>>>,
}

impl EventCollector {
    fn sink(self) -> impl FnMut(TurnEvent) {
        move |event: TurnEvent| match &event {
            TurnEvent::TurnStarted { turn } => {
                self.started_turn_ids
                    .lock()
                    .expect("ids")
                    .push(turn.turn_id.clone());
                self.methods.lock().expect("methods").push(event.method());
            }
            _ => self.methods.lock().expect("methods").push(event.method()),
        }
    }
}

fn new_conversation(
    sessions: &std::path::Path,
    provider: Arc<dyn Provider + Send + Sync>,
    model: Option<&str>,
) -> Arc<Conversation> {
    let runner = Arc::new(
        TurnRunner::new(sessions.to_path_buf(), provider_snapshot())
            .with_provider_override(provider),
    );
    let thread = create_thread(
        sessions,
        std::env::current_dir().unwrap().to_str().unwrap(),
        model.map(str::to_string),
        runner.coordinator(),
    )
    .expect("create thread");
    Conversation::new(runner, thread)
}

fn thread_settings_count(sessions: &std::path::Path, thread_id: &str) -> usize {
    SessionManager::open_existing_read_only(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .metadata_entries()
        .iter()
        .filter(|entry| entry.kind() == SessionMetadataKind::ThreadSettings)
        .count()
}

/// 最后一条 `thread_settings` 记录反推的 selector（与 resume 投影的
/// last-wins 组合规则一致）。
fn last_recorded_selector(sessions: &std::path::Path, thread_id: &str) -> Option<String> {
    SessionManager::open_existing_read_only(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .metadata_entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionMetadata::ThreadSettings {
                provider,
                model,
                reasoning,
            } => Some(singularity_model::compose_model_selector(
                provider,
                model,
                reasoning.as_deref().filter(|value| !value.is_empty()),
            )),
            _ => None,
        })
}

#[test]
fn panic_in_turn_releases_the_reservation_window() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(&sessions, Arc::new(ScriptedProvider::ok("ok")), None);
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
        !conversation.has_active_turn(),
        "panic must not leak the active window"
    );
    let reservation = conversation
        .reserve_start()
        .expect("reservation succeeds after a panic");
    drop(reservation);
}

#[test]
fn reservation_holds_window_and_releases_on_drop() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("ok"),
        ScriptedAttempt::success("ok"),
    ]));
    let shared = new_conversation(
        &sessions,
        Arc::clone(&provider) as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = shared.thread().unwrap().thread_id;

    // 预订原子开启活动窗口：busy、设置、followUp 与控制路由全部从同一
    // Reserved 生命周期状态派生。
    let reservation = shared.reserve_start().expect("first reservation wins");
    assert!(shared.has_active_turn(), "reservation is a busy window");
    assert!(
        shared.reserve_start().is_err(),
        "second reservation must be rejected"
    );
    let mut sink = EventCollector::default().sink();
    assert!(
        shared.run_turn("must not run", &mut sink).is_err(),
        "run_turn must be rejected while a reservation holds the window"
    );
    assert!(!shared.steer("not running yet"));
    shared.interrupt();
    assert!(
        !shared.submit_follow_up("queued while reserved"),
        "followUp is rejected during Reserved (no writer yet)"
    );
    let timing = shared
        .update_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            ..SettingsPatch::default()
        })
        .expect("apply settings during reservation");
    assert_eq!(timing.timing, SettingsApplyTiming::AppliedNow);
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model"),
        "commit point only updates the in-memory projection"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        0,
        "commit point writes nothing: recording happens at the next turn start"
    );

    // 未消费的预订 drop 后窗口释放；Reserved 期间被拒绝的 followUp 不再
    // 出现在后续链中（其接受需要活动 turn 的共享写者）。
    drop(reservation);
    assert!(!shared.has_active_turn());
    let outcome = shared.run_turn("now it runs", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(shared.pending_follow_up_count(), 0);
    assert_eq!(
        input_sequence(&provider.requests()),
        vec!["now it runs".to_string()],
        "the follow-up rejected during Reserved never reaches a model step"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "the turn recorded the effective selector at its start"
    );
}

/// 运行中改设置走与空闲时同一条提交路径：写者锁被活动 turn 占用时提交点
/// 仍只更新内存投影（不写文件、不报错），落盘由下一 turn 开始时记录（turn 边界记录）。
#[test]
fn settings_update_mid_turn_is_accepted_and_recorded_at_next_turn_start() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (gate, started_rx) = StopGateProvider::new();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let conversation = new_conversation(
        &sessions,
        gate as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = conversation.thread().unwrap().thread_id;

    let mut sink = EventCollector::default().sink();
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || conversation.run_turn("first", &mut sink))
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("turn reaches the provider");

    let timing = conversation
        .update_settings(SettingsPatch {
            model: Some("base-model-2".to_string()),
            ..SettingsPatch::default()
        })
        .expect("mid-turn settings update is accepted");
    assert_eq!(timing.timing, SettingsApplyTiming::AppliedNow);
    assert_eq!(
        conversation.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model-2"),
        "in-memory projection is updated while the turn holds the writer lock"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "turn 1 start recorded the original selector; the commit point writes nothing"
    );

    release_tx.send(()).expect("gate release");
    let outcome = worker.join().expect("turn thread").expect("turn ok");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    let mut sink = EventCollector::default().sink();
    let outcome = conversation.run_turn("second", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        2,
        "turn 2 start recorded the changed selector"
    );
    assert_eq!(
        last_recorded_selector(&sessions, &thread_id).as_deref(),
        Some("openai_compatible/base-model-2"),
        "resume projection (last-wins) shows the mid-turn change"
    );
}

#[test]
fn compact_releases_its_busy_window_when_the_provider_panics() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(
        &sessions,
        Arc::new(ScriptedProvider::new([ScriptedAttempt::Panic])),
        None,
    );
    let thread_id = conversation.thread().unwrap().thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let mut session = SessionManager::open_existing(&path).expect("open session");
    for (role, text) in [
        (AgentMessageRole::User, "first user ".repeat(5_000)),
        (
            AgentMessageRole::Assistant,
            "first assistant ".repeat(5_000),
        ),
        (AgentMessageRole::User, "recent user ".repeat(5_000)),
        (
            AgentMessageRole::Assistant,
            "recent assistant ".repeat(5_000),
        ),
    ] {
        session
            .append_message(AgentMessage::text(role, text))
            .expect("append history");
    }
    drop(session);

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let cancellation = singularity_core::CancellationToken::new();
        let _ = conversation.compact(&cancellation);
    }));

    assert!(panic.is_err(), "the provider panic must propagate");
    assert!(
        !conversation.has_active_turn(),
        "compaction must release the single-writer window while unwinding"
    );
}

#[test]
fn failed_compaction_closes_its_durable_operation() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(
        &sessions,
        Arc::new(ScriptedProvider::new([ScriptedAttempt::failure_kind(
            ModelErrorKind::NetworkError,
            "summary request failed",
        )])),
        None,
    );
    let thread_id = conversation.thread().unwrap().thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));
    let mut session = SessionManager::open_existing(&path).expect("open session");
    for (role, text) in [
        (AgentMessageRole::User, "first user ".repeat(5_000)),
        (
            AgentMessageRole::Assistant,
            "first assistant ".repeat(5_000),
        ),
        (AgentMessageRole::User, "recent user ".repeat(5_000)),
        (
            AgentMessageRole::Assistant,
            "recent assistant ".repeat(5_000),
        ),
    ] {
        session
            .append_message(AgentMessage::text(role, text))
            .expect("append history");
    }
    drop(session);

    let cancellation = singularity_core::CancellationToken::new();
    conversation
        .compact(&cancellation)
        .expect_err("provider failure must surface");

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
fn resume_thread_conflicts_with_active_writer_and_succeeds_after_release() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let thread_id = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    let session = SessionManager::create_with_id(Path::new("."), &sessions, thread_id)
        .expect("create session file");

    // 同一会话已有存活写者（模拟另一进程持有锁）：resume 必须快速失败。
    let conflict = match resume_thread(&sessions, thread_id, &coordinator(&sessions)) {
        Ok(_) => panic!("resume must conflict with an active writer"),
        Err(crate::store::ResumeError::Store(message)) => message,
        Err(other) => panic!("expected store conflict, got {other:?}"),
    };
    assert!(
        conflict.contains("active writer"),
        "conflict reason must mention the active writer: {conflict}"
    );

    // 写者释放后 resume 恢复正常。
    drop(session);
    let resumed =
        resume_thread(&sessions, thread_id, &coordinator(&sessions)).expect("resume after release");
    assert_eq!(resumed.thread_id, thread_id);
}

/// 状态锁中毒后全部公共 API 按 fail-closed 收敛。
#[test]
fn state_lock_poison_fails_closed() {
    let sessions = temp_sessions();
    let conversation = new_conversation(
        sessions.path(),
        Arc::new(ScriptedProvider::new([ScriptedAttempt::Panic])),
        None,
    );
    let guard = Arc::clone(&conversation);

    // 毒化 state Mutex：在线程中持锁后 panic。
    let handle = std::thread::spawn(move || {
        guard.poison_state_lock();
    });
    handle.join().unwrap_err();

    // 读路径：中毒按 busy/None 收敛。
    assert!(conversation.has_active_turn(), "poisoned → busy");
    assert_eq!(conversation.active_turn_id(), None, "poisoned → None");

    // 写路径：中毒按 false/Err 拒绝。
    assert!(!conversation.steer("test"), "poisoned → false");
    assert!(!conversation.submit_follow_up("test"), "poisoned → false");

    // lock_state() fail-loud：直接返回中毒错误。
    match conversation.thread() {
        Err(crate::ConversationError::State(msg)) => {
            assert!(msg.contains("poisoned"), "fail-loud: {msg}");
        }
        other => panic!("expected State poisoned, got {other:?}"),
    }
}

/// 首次请求成功并携带 usage（调用未注册工具迫使循环续接），第二次请求失败：
/// 失败终态事件必须报告本轮已记录的 usage（回归：失败终态曾以空 usage 出口）。
struct UsageThenFailingProvider {
    calls: std::sync::atomic::AtomicUsize,
}

impl Provider for UsageThenFailingProvider {
    fn model_configuration(&self) -> ModelConfigurationSnapshot {
        test_model_configuration()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
        _on_event: &mut dyn FnMut(singularity_model::ProviderStreamEvent),
        _on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            let mut message = singularity_model::ModelMessage::text(
                singularity_model::ModelRole::Assistant,
                "calling a tool",
            );
            message.tool_calls.push(singularity_model::ModelToolCall {
                tool_call_id: "call-1".to_string(),
                tool_name: "definitely-not-a-registered-tool".to_string(),
                arguments: serde_json::json!({}),
                raw_arguments: "{}".to_string(),
                parse_status: singularity_model::ModelToolParseStatus::Valid,
                validation_errors: Vec::new(),
            });
            return Ok(ModelTurnResponse {
                request_id: request.request_id.clone(),
                response_id: "resp-1".to_string(),
                assistant_message: Some(message),
                usage: singularity_model::ModelUsage {
                    input_tokens: 10,
                    output_tokens: 32,
                    total_tokens: 42,
                    usage_present: true,
                    ..Default::default()
                },
                finish_reason: Some("tool_calls".to_string()),
                provider_name: None,
                model_name: None,
                provider_reasoning_history: Vec::new(),
            });
        }
        Err(ProviderError::from_model_error(
            singularity_model::ModelError::new(
                singularity_model::ModelErrorKind::NetworkError,
                "connection reset",
            ),
        ))
    }
}

#[test]
fn failed_turn_reports_usage_recorded_before_the_failure() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(
        &sessions,
        Arc::new(UsageThenFailingProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        None,
    );
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
    SessionManager::open_existing_read_only(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .ledger_records()
}

/// T020 [US1]：模型等待边界的中断收敛为 interrupted，终态恰好一条，
/// 且协调器立即接受下一条输入（Pi agent-loop.ts:215-219：aborted 终态
/// 结束本次 run，后续 prompt 走全新 run）。
#[test]
fn interruption_at_model_boundary_converges_interrupted_and_next_input_runs() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (gate, started_rx) = StopGateProvider::new();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    gate.with_release(release_rx);
    let conversation = new_conversation(
        &sessions,
        gate as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = conversation.thread().unwrap().thread_id;

    let mut terminal_events = Vec::new();
    let worker = {
        let conversation = Arc::clone(&conversation);
        std::thread::spawn(move || {
            let mut sink = |event: TurnEvent| terminal_events.push(event);
            let outcome = conversation.run_turn("first", &mut sink);
            (conversation, terminal_events, outcome)
        })
    };
    started_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("turn reaches the provider");
    conversation.interrupt();
    // 释放被阻塞的请求：provider 观察到取消令牌并以 Cancelled 收敛。
    release_tx.send(()).expect("release the gate");
    let (_conversation, terminal_events, outcome) = worker.join().expect("worker");

    let outcome = outcome.expect("interruption converges as an Ok interrupted outcome");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    let terminals: Vec<&TurnEvent> = terminal_events
        .iter()
        .filter(|event| {
            matches!(
                event,
                TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
            )
        })
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal event for the interrupted turn"
    );
    assert!(matches!(
        terminals[0],
        TurnEvent::TurnCompleted { turn } if turn.status == TurnStatus::Interrupted
    ));

    let records = ledger_of(&sessions, &thread_id);
    let finished: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                singularity_agent::session::LedgerRecord::OperationFinished { .. }
            )
        })
        .collect();
    assert_eq!(finished.len(), 1, "exactly one durable terminal outcome");
    assert!(matches!(
        finished[0],
        singularity_agent::session::LedgerRecord::OperationFinished {
            outcome: TurnStatus::Interrupted,
            ..
        }
    ));

    // 中断后协调器空闲，下一条输入走同一条链正常完成。
    let conversation = _conversation;
    let mut sink = EventCollector::default().sink();
    let next = conversation
        .run_turn("second", &mut sink)
        .expect("next input runs after interruption");
    assert_eq!(next.turn_status, TurnStatus::Completed);
}

/// T020 [US1]：工具执行边界的中断。bash 进程流式输出 `ready` 后仍在运行，
/// 此刻中断：进程树被终止、工具以模型可见失败闭合、operation 收敛为
/// interrupted，且 `replay: never` 的调用绝不被自动重放（下一条输入正常
/// 开新 turn）。Pi 同形：abort 在安全边界收敛，未知副作用不重放
/// （reducer.ts:79-109 的 toolBatch/aborting 状态）。
#[test]
fn interruption_at_tool_boundary_converges_interrupted_and_next_input_runs() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedAttempt::tool_call(
            "call-bash",
            "bash",
            serde_json::json!({"command": "echo ready; sleep 30"}),
        ),
        ScriptedAttempt::success("next turn done"),
    ]));
    let conversation = new_conversation(
        &sessions,
        provider as Arc<dyn Provider + Send + Sync>,
        Some("openai_compatible/base-model"),
    );
    let thread_id = conversation.thread().unwrap().thread_id;

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
    conversation.interrupt();
    let (conversation, outcome) = worker.join().expect("worker");

    let outcome = outcome.expect("tool-boundary interruption converges as interrupted");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);

    let session =
        SessionManager::open_existing_read_only(&sessions.join(format!("{thread_id}.jsonl")))
            .expect("reopen");
    let records = session.ledger_records();
    let started_tools: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record,
                singularity_agent::session::LedgerRecord::ToolStarted { .. }
            )
        })
        .collect();
    assert_eq!(
        started_tools.len(),
        1,
        "the never-replay tool was started exactly once"
    );
    assert!(matches!(
        started_tools[0],
        singularity_agent::session::LedgerRecord::ToolStarted {
            tool_call_id,
            replay: singularity_agent::session::ToolReplayClass::Never,
            ..
        } if tool_call_id == "call-bash"
    ));
    let aborted_results = session
        .entries()
        .iter()
        .filter(|entry| {
            matches!(entry,
                singularity_agent::session::SessionEntry::Message { message, .. }
                    if message.role() == singularity_agent::message::AgentMessageRole::ToolResult
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
    let mut sink = EventCollector::default().sink();
    let next = conversation
        .run_turn("continue", &mut sink)
        .expect("next input runs after a tool-boundary interruption");
    assert_eq!(next.turn_status, TurnStatus::Completed);
    assert_eq!(next.final_text, "next turn done");
}
