//! 协调器与 turn 管线的行为测试：以最小 fake Provider 驱动真实管线，
//! 验证单活动 turn、事件顺序、失败收敛、取消、设置自动生效与
//! followUp 后续队列合同。

use std::sync::Arc;
use std::sync::mpsc;

use singularity_agent::session::SessionManager;
use singularity_agent::session::SessionMetadataKind;
use singularity_model::{
    ModelError, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderProtocolContract,
};
use singularity_runtime::events::TurnEvent;
use singularity_runtime::objects::{ThreadStatus, TurnStatus};
use singularity_runtime::runner::{TurnOutcome, TurnRunner};
use singularity_runtime::store::{create_thread, persisted_model_selector, resume_thread};
use singularity_runtime::{Conversation, ConversationError, SettingsPatch};

fn temp_sessions() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("temp home");
    std::fs::create_dir_all(dir.path().join("sessions")).expect("sessions dir");
    dir
}

fn snapshot() -> singularity_model::ProviderConfigSnapshot {
    // 进程层注入固定三元组：快照完全不读磁盘与真实环境；
    // selector 校验按 legacy 规则解析 openai_compatible/base-model。
    // fake provider 不经 HTTP；Handle 背后的 runtime 无需存活。
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    std::mem::forget(runtime);
    singularity_model::ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL" => Some("base-model".to_string()),
            "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:9/v1".to_string()),
            "SINGULARITY_API_KEY" => Some("test-key-placeholder".to_string()),
            _ => None,
        },
        handle,
    )
}

/// 记录每次模型请求，用于断言各 turn 实际看到的输入与 selector。
#[derive(Default)]
struct RequestLog {
    entries: std::sync::Mutex<Vec<ModelTurnRequest>>,
}

impl RequestLog {
    fn record(&self, request: &ModelTurnRequest) {
        self.entries
            .lock()
            .expect("request log")
            .push(request.clone());
    }

    fn count(&self) -> usize {
        self.entries.lock().expect("request log").len()
    }

    /// 每次请求中最后一条 user 消息：即该请求所属 turn 的新增输入。
    /// （更早的输入会作为历史上下文重放，不能用于唯一性判断。）
    fn input_sequence(&self) -> Vec<String> {
        self.entries
            .lock()
            .expect("request log")
            .iter()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == singularity_model::ModelRole::User)
                    .map(|message| message.content.clone())
                    .unwrap_or_default()
            })
            .collect()
    }

    /// 第 n 次请求的 model selector（AgentConfig 投影）。
    fn model_of(&self, index: usize) -> Option<String> {
        self.entries
            .lock()
            .expect("request log")
            .get(index)
            .and_then(|request| request.model_preferences.model_name.clone())
    }
}

/// 一次成功完成、返回固定文本并记录请求的 provider。
struct RecordingProvider {
    text: String,
    log: Arc<RequestLog>,
}

impl Provider for RecordingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.log.record(request);
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "resp-fake",
            self.text.clone(),
        ))
    }
}

/// 在 provider 内部挂起直到外部放行，制造确定的活动 turn 窗口；同时记录请求。
struct GatedRecordingProvider {
    release: std::sync::Mutex<mpsc::Receiver<()>>,
    log: Arc<RequestLog>,
}

impl Provider for GatedRecordingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let _ = self
            .release
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(10));
        self.log.record(request);
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "r",
            "ok",
        ))
    }
}

struct FailingProvider;

impl Provider for FailingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        Err(ProviderError::from_model_error(ModelError::new(
            ModelErrorKind::AuthError,
            "Provider returned HTTP 401.",
        )))
    }
}

fn collect_sink() -> (
    Arc<std::sync::Mutex<Vec<&'static str>>>,
    impl FnMut(TurnEvent),
) {
    let log: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
    let sink_log = Arc::clone(&log);
    let sink = move |event: TurnEvent| {
        log.lock().expect("event log").push(event.method());
    };
    (sink_log, sink)
}

fn collected_turn_ids(sink_log: &[&'static str]) -> usize {
    // 由事件日志无法取 id；调用方需要 id 时使用 event collector。
    sink_log.iter().filter(|m| **m == "turn/started").count()
}

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
        TurnRunner::new(sessions.to_path_buf(), snapshot()).with_provider_override(provider),
    );
    let thread = create_thread(
        sessions,
        std::env::current_dir().unwrap().to_str().unwrap(),
        model.map(str::to_string),
    )
    .expect("create thread");
    Conversation::new(runner, thread)
}

fn wait_for_active(conversation: &Conversation) {
    for _ in 0..400 {
        if conversation.has_active_turn() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("turn should have become active");
}

/// 等待注入窗口就绪并接受转向输入：活动标记先于 Agent 收件箱注册出现，
/// 接受失败只说明窗口尚未开启，不产生任何副作用。
fn wait_steer_accepted(conversation: &Conversation, text: &str) {
    for _ in 0..400 {
        if conversation.steer(text) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("steer should have been accepted once the inbox is registered");
}

fn thread_settings_count(sessions: &std::path::Path, thread_id: &str) -> usize {
    SessionManager::open_existing(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen")
        .metadata_entries()
        .iter()
        .filter(|entry| entry.kind() == SessionMetadataKind::ThreadSettings)
        .count()
}

#[test]
fn successful_turn_emits_lifecycle_events_and_final_text() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(
        &sessions,
        Arc::new(RecordingProvider {
            text: "done".into(),
            log: Arc::new(RequestLog::default()),
        }),
        None,
    );
    let (log, mut sink) = collect_sink();
    let outcome: TurnOutcome = conversation
        .run_turn("hello", &mut sink)
        .expect("turn completes");

    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(outcome.final_text, "done");
    let methods = log.lock().unwrap().clone();
    assert_eq!(methods.first().copied(), Some("turn/started"));
    assert_eq!(methods.last().copied(), Some("turn/completed"));
    // 终态 metadata 已落盘：resume 投影出 completed。
    let resumed = resume_thread(&sessions, &outcome.thread_id).expect("resume");
    assert_eq!(resumed.last_turn_status, Some(ThreadStatus::Completed));
}

#[test]
fn provider_failure_converges_to_failed_turn_with_terminal_event() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(&sessions, Arc::new(FailingProvider), None);
    let (log, mut sink) = collect_sink();
    let error = conversation
        .run_turn("hello", &mut sink)
        .expect_err("fails");
    assert!(matches!(error, ConversationError::Turn(_)));
    let methods = log.lock().unwrap().clone();
    assert_eq!(methods.last().copied(), Some("turn/error"));
    let thread_id = conversation.thread().unwrap().thread_id;
    let resumed = resume_thread(&sessions, &thread_id).expect("resume");
    assert_eq!(resumed.last_turn_status, Some(ThreadStatus::Failed));
}

#[test]
fn only_one_turn_may_be_active_at_a_time() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::new(RequestLog::default()),
        }),
        None,
    ));

    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let (_, mut sink) = collect_sink();
            shared.run_turn("first", &mut sink)
        })
    };
    wait_for_active(&shared);
    let (_, mut sink) = collect_sink();
    let second = shared.run_turn("second", &mut sink);
    assert!(
        matches!(second, Err(ConversationError::TurnAlreadyActive)),
        "concurrent turn must be rejected"
    );
    release_tx.send(()).expect("release first turn");
    let outcome = turn_thread.join().expect("join").expect("first turn ok");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert!(!shared.has_active_turn());
}

#[test]
fn interrupt_cancels_the_running_turn() {
    use singularity_core::CancellationToken;
    struct HangingProvider {
        cancellation_probe: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    }
    impl Provider for HangingProvider {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            ProviderProtocolContract::default()
        }
        fn complete(
            &self,
            _request: &ModelTurnRequest,
            cancellation: &CancellationToken,
        ) -> Result<ModelTurnResponse, ProviderError> {
            self.cancellation_probe
                .lock()
                .unwrap()
                .replace(cancellation.clone());
            while !cancellation.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(ProviderError::from_model_error(ModelError::new(
                ModelErrorKind::Cancelled,
                "cancelled",
            )))
        }
    }

    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let probe: Arc<std::sync::Mutex<Option<CancellationToken>>> = Arc::default();
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(HangingProvider {
            cancellation_probe: Arc::clone(&probe),
        }),
        None,
    ));
    let thread_id = shared.thread().unwrap().thread_id;
    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let (_, mut sink) = collect_sink();
            shared.run_turn("hello", &mut sink)
        })
    };
    for _ in 0..1000 {
        if probe.lock().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        probe.lock().unwrap().is_some(),
        "provider should be running"
    );
    shared.interrupt();
    // AgentLoop 把 typed Cancelled 规范化为 aborted 终态：run_turn 正常返回
    // 携带 Interrupted 状态的结果，而不是 Err。
    let outcome = turn_thread
        .join()
        .expect("join")
        .expect("interrupted outcome");
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    let resumed = resume_thread(&sessions, &thread_id)
        .expect("resume")
        .last_turn_status;
    assert_eq!(resumed, Some(ThreadStatus::Interrupted));
}

// ---------------------------------------------------------------------------
// 设置生效时序：排队意图由 Conversation 自动应用，调用方零手动步骤
// ---------------------------------------------------------------------------

#[test]
fn settings_accepted_during_turn_apply_automatically_before_next_turn() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::clone(&log),
        }),
        Some("base-model"),
    ));
    let thread_id = shared.thread().unwrap().thread_id;

    // 生产路径：与 TUI 相同的调用方式——活动 turn 中提交设置，然后直接启动
    // 下一 turn。测试不得手动提取或应用任何内部队列。
    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let (_, mut sink) = collect_sink();
            shared.run_turn("first turn", &mut sink)
        })
    };
    wait_for_active(&shared);

    // 无效 selector（快照无法解析的模型）在提交点即被拒绝。
    let rejected = shared.queue_settings(SettingsPatch {
        model: Some("no-such-model".to_string()),
        ..SettingsPatch::default()
    });
    assert!(
        rejected.is_err(),
        "invalid selector must be rejected eagerly"
    );

    // 活动 turn 期间：合法 patch 只记录意图，不落盘、不改内存投影。
    // 两次部分修改按字段合并为一份待生效意图。
    let queued = shared
        .queue_settings(SettingsPatch {
            model: Some("base-model".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue first patch");
    assert!(queued);
    let queued = shared
        .queue_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue second patch");
    assert!(queued);
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("base-model"),
        "current turn keeps its startup selector"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        0,
        "queued settings must persist after the trusted terminal, mid-turn"
    );

    release_tx.send(()).expect("release");
    turn_thread.join().expect("join").expect("first turn ok");

    // 可信终态后自动持久化：恰一条 thread_settings metadata，投影同步更新。
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "merged patches persist as exactly one thread_settings metadata"
    );
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model"),
        "projection reflects the applied selector"
    );
    let persisted = {
        let session = SessionManager::open_existing(&sessions.join(format!("{thread_id}.jsonl")))
            .expect("reopen");
        persisted_model_selector(&session)
    };
    assert_eq!(
        persisted.as_deref(),
        Some("openai_compatible/base-model"),
        "persisted settings project back to the applied selector"
    );

    // 直接启动下一 turn：新 selector 即刻生效于模型请求。
    let (_, mut sink) = collect_sink();
    let next = shared
        .run_turn("second turn", &mut sink)
        .expect("next turn");
    assert_eq!(next.turn_status, TurnStatus::Completed);
    assert_eq!(log.count(), 2, "two turns, two model requests");
    assert_eq!(
        log.model_of(0).as_deref(),
        Some("base-model"),
        "current turn used its startup selector"
    );
    assert_eq!(
        log.model_of(1).as_deref(),
        Some("openai_compatible/base-model"),
        "next turn uses the applied selector"
    );
}

#[test]
fn idle_settings_persist_immediately_without_any_turn() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(
        &sessions,
        Arc::new(RecordingProvider {
            text: "ok".into(),
            log: Arc::new(RequestLog::default()),
        }),
        None,
    );
    let thread_id = conversation.thread().unwrap().thread_id;
    let updated = conversation
        .queue_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            model: Some("base-model".to_string()),
            ..SettingsPatch::default()
        })
        .expect("apply while idle");
    assert!(updated);
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "idle updates persist as exactly one metadata record"
    );
    assert_eq!(
        conversation.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model")
    );
}

// ---------------------------------------------------------------------------
// followUp 后续队列：Conversation 是唯一所有者，每条恰好执行一次
// ---------------------------------------------------------------------------

#[test]
fn follow_up_runs_exactly_once_as_a_distinct_new_turn() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::clone(&log),
        }),
        Some("openai_compatible/base-model"),
    ));
    let collector = EventCollector::default();

    let turn_thread = {
        let shared = Arc::clone(&shared);
        let collector = collector.clone();
        std::thread::spawn(move || {
            let mut sink = collector.sink();
            shared.run_turn("initial goal", &mut sink)
        })
    };
    wait_for_active(&shared);

    // 活动 turn 中提交一条 followUp：进入 Conversation 队列。
    assert!(
        shared.submit_follow_up("the one follow-up"),
        "active turn accepts follow-ups"
    );
    assert_eq!(shared.pending_follow_ups(), vec!["the one follow-up"]);

    release_tx.send(()).expect("release");
    let outcome = turn_thread.join().expect("join").expect("chain completes");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    // 当前 turn 与 followUp 使用两个不同 turn id。
    let ids = collector.started_turn_ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 2, "followUp starts exactly one new turn");
    assert_ne!(ids[0], ids[1], "turns have distinct identities");

    // Provider 恰好看到一次该输入：每个请求的最后一条 user 消息即该轮新增
    // 输入，序列恰为「初始输入 → followUp」。
    assert_eq!(
        log.input_sequence(),
        vec!["initial goal".to_string(), "the one follow-up".to_string()],
        "followUp executes exactly once as its own turn"
    );
    // 队列清空，且后续输入不再残留。
    assert!(shared.pending_follow_ups().is_empty());
    assert!(!shared.has_active_turn());

    // 会话落盘两轮 turn 边界。
    let thread_id = shared.thread().unwrap().thread_id;
    let session =
        SessionManager::open_existing(&sessions.join(format!("{thread_id}.jsonl"))).expect("open");
    assert_eq!(
        session
            .metadata_entries()
            .iter()
            .filter(|entry| entry.kind() == SessionMetadataKind::TurnStarted)
            .count(),
        2,
        "both turns persist their own started marker"
    );
}

#[test]
fn multiple_follow_ups_execute_in_fifo_order_once_each() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::clone(&log),
        }),
        Some("openai_compatible/base-model"),
    ));

    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let (_, mut sink) = collect_sink();
            shared.run_turn("t0", &mut sink)
        })
    };
    wait_for_active(&shared);
    for text in ["fu-1", "fu-2", "fu-3"] {
        assert!(shared.submit_follow_up(text));
    }
    release_tx.send(()).expect("release");
    turn_thread.join().expect("join").expect("chain completes");

    assert_eq!(log.count(), 4, "one request per accepted input");
    assert_eq!(
        log.input_sequence(),
        vec![
            "t0".to_string(),
            "fu-1".to_string(),
            "fu-2".to_string(),
            "fu-3".to_string()
        ],
        "followUps execute once each in submission order"
    );
    assert!(shared.pending_follow_ups().is_empty());
}

#[test]
fn steer_affects_only_the_current_turn() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::clone(&log),
        }),
        Some("openai_compatible/base-model"),
    ));
    let collector = EventCollector::default();

    let turn_thread = {
        let shared = Arc::clone(&shared);
        let collector = collector.clone();
        std::thread::spawn(move || {
            let mut sink = collector.sink();
            shared.run_turn("original task", &mut sink)
        })
    };
    wait_for_active(&shared);
    wait_steer_accepted(&shared, "course correction");
    release_tx.send(()).expect("release");
    turn_thread.join().expect("join").expect("turn completes");

    // 单一 turn：steer 不产生新的 turn。
    assert_eq!(
        collected_turn_ids(&collector.methods.lock().unwrap()),
        1,
        "steer stays inside the current turn"
    );
    // steer 输入在当前 turn 的后续请求中生效一次；不产生新的 turn 输入。
    assert_eq!(
        log.input_sequence(),
        vec!["original task".to_string(), "course correction".to_string()],
        "steer joins the current turn's context without spawning a turn"
    );
}

#[test]
fn interrupt_ends_current_turn_but_queued_follow_up_still_runs() {
    use singularity_core::CancellationToken;
    // 第一轮挂起等待取消信号（可取消），后续轮次直接成功。
    struct FirstHangThenDone {
        cancellation_probe: Arc<std::sync::Mutex<Option<CancellationToken>>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Provider for FirstHangThenDone {
        fn protocol_contract(&self) -> ProviderProtocolContract {
            ProviderProtocolContract::default()
        }
        fn complete(
            &self,
            request: &ModelTurnRequest,
            cancellation: &CancellationToken,
        ) -> Result<ModelTurnResponse, ProviderError> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if call == 0 {
                self.cancellation_probe
                    .lock()
                    .unwrap()
                    .replace(cancellation.clone());
                while !cancellation.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                return Err(ProviderError::from_model_error(ModelError::new(
                    ModelErrorKind::Cancelled,
                    "cancelled",
                )));
            }
            Ok(ModelTurnResponse::completed(
                request.request_id.clone(),
                "r",
                "after interrupt",
            ))
        }
    }

    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let probe: Arc<std::sync::Mutex<Option<CancellationToken>>> = Arc::default();
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(FirstHangThenDone {
            cancellation_probe: Arc::clone(&probe),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Some("openai_compatible/base-model"),
    ));
    let collector = EventCollector::default();

    let turn_thread = {
        let shared = Arc::clone(&shared);
        let collector = collector.clone();
        std::thread::spawn(move || {
            let mut sink = collector.sink();
            shared.run_turn("work that will be interrupted", &mut sink)
        })
    };
    for _ in 0..1000 {
        if probe.lock().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(shared.submit_follow_up("run after the interrupt"));

    shared.interrupt();
    // 链条继续：被中断的 turn 终态后，已接受的 followUp 作为新 turn 执行。
    let outcome = turn_thread.join().expect("join").expect("chain completes");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert_eq!(outcome.final_text, "after interrupt");

    let ids = collector.started_turn_ids.lock().unwrap().clone();
    assert_eq!(ids.len(), 2, "interrupted turn plus followUp turn");
    assert_ne!(ids[0], ids[1]);
    assert!(shared.pending_follow_ups().is_empty());
    let thread_id = shared.thread().unwrap().thread_id;
    let resumed = resume_thread(&sessions, &thread_id).expect("resume");
    assert_eq!(resumed.last_turn_status, Some(ThreadStatus::Completed));
}

#[test]
fn reservation_holds_window_and_releases_on_drop() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let shared = new_conversation(
        &sessions,
        Arc::new(RecordingProvider {
            text: "ok".into(),
            log: Arc::new(RequestLog::default()),
        }),
        Some("base-model"),
    );

    // 预订原子开启活动窗口：第二个预订与普通 run 都被拒绝。
    let reservation = shared.reserve_start().expect("first reservation wins");
    assert!(
        shared.reserve_start().is_err(),
        "second reservation must be rejected"
    );
    let mut sink = EventCollector::default().sink();
    assert!(
        shared.run_turn("must not run", &mut sink).is_err(),
        "run_turn must be rejected while a reservation holds the window"
    );

    // 未消费的预订 drop 后窗口释放：随后可正常执行一轮。
    drop(reservation);
    assert!(!shared.has_active_turn());
    let outcome = shared.run_turn("now it runs", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert!(shared.pending_follow_ups().is_empty());
}
