//! 协调器与 turn 管线的行为测试：以最小 fake Provider 驱动真实管线，
//! 验证单活动 turn、事件顺序、失败收敛、取消、设置自动生效与
//! followUp 后续队列合同。

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;

use crate::events::TurnEvent;
use crate::objects::{ThreadStatus, TurnStatus};
use crate::runner::{TurnOutcome, TurnRunner};
use crate::store::{create_thread, resume_thread};
use crate::{
    Conversation, ConversationError, ReasoningPatch, SettingsApplyTiming, SettingsPatch,
    TurnRunError, compose_merged_selector,
};
use singularity_agent::message::{AgentMessage, AgentMessageRole};
use singularity_agent::session::{
    SessionManager, SessionMetadata, SessionMetadataKind, project_session,
};
use singularity_model::{
    ModelError, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderProtocolContract, ProviderReasoningReplay,
};

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

struct ThinkingProvider;

impl Provider for ThinkingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut response = ModelTurnResponse::completed(&request.request_id, "thinking", "answer");
        response.provider_reasoning_history = vec![ProviderReasoningReplay::Chat {
            provider_name: "openai_compatible".to_string(),
            model_name: "base-model".to_string(),
            reasoning_effort: None,
            tool_call_ids: Vec::new(),
            reasoning_content: "considered the evidence".to_string(),
        }];
        Ok(response)
    }
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
/// `requested` 在请求到达 provider 时置位——会话已在准备阶段打开，此后
/// 对 JSONL 的改动不会影响本轮执行，只影响终态后的重新打开。
struct GatedRecordingProvider {
    release: std::sync::Mutex<mpsc::Receiver<()>>,
    log: Arc<RequestLog>,
    requested: Arc<std::sync::atomic::AtomicBool>,
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
        self.requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
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

fn wait_for_requested(requested: &std::sync::atomic::AtomicBool) {
    for _ in 0..400 {
        if requested.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("provider should have received the request");
}

/// 顺序门控 provider：每次请求按序阻塞在各自的闸门上（无闸门时立即返回），
/// 用于确定性构造多轮链窗口。
struct SequentialGatedProvider {
    gates: std::sync::Mutex<std::collections::VecDeque<std::sync::Mutex<mpsc::Receiver<()>>>>,
    log: Arc<RequestLog>,
    requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl Provider for SequentialGatedProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(gate) = self.gates.lock().unwrap().pop_front() {
            let _ = gate
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }
        self.log.record(request);
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "r",
            "ok",
        ))
    }
}

fn wait_for_request_count(requests: &std::sync::atomic::AtomicUsize, count: usize) {
    for _ in 0..400 {
        if requests.load(std::sync::atomic::Ordering::SeqCst) >= count {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("provider should have received {count} requests");
}

struct FailingProvider;

struct PanickingProvider;

impl Provider for PanickingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        panic!("compaction provider panic")
    }
}

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

/// 收集 turn/started 事件的完整 turn id 序列与 settingsApplied 投影。
#[derive(Clone, Default)]
struct EventCollector {
    methods: Arc<std::sync::Mutex<Vec<&'static str>>>,
    started_turn_ids: Arc<std::sync::Mutex<Vec<String>>>,
    applied_threads: Arc<std::sync::Mutex<Vec<crate::Thread>>>,
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
            TurnEvent::ThreadSettingsApplied { thread } => {
                self.applied_threads
                    .lock()
                    .expect("applied threads")
                    .push(thread.clone());
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
    SessionManager::open_existing_read_only(&sessions.join(format!("{thread_id}.jsonl")))
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
fn completed_turn_exposes_its_persisted_thinking_blocks() {
    let home = temp_sessions();
    let conversation = new_conversation(
        &home.path().join("sessions"),
        Arc::new(ThinkingProvider),
        None,
    );
    let mut events = Vec::new();
    let outcome = conversation
        .run_turn("reason", &mut |event| events.push(event))
        .expect("turn completes");

    assert_eq!(
        conversation
            .thinking_for_turn(&outcome.turn_id)
            .expect("thinking loads"),
        vec!["considered the evidence"]
    );
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
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
    // 两次部分修改按字段合并为一份待生效意图；返回值说明生效时点。
    let queued = shared
        .queue_settings(SettingsPatch {
            model: Some("base-model".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue first patch");
    assert_eq!(queued, SettingsApplyTiming::QueuedForNextTurn);
    let queued = shared
        .queue_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue second patch");
    assert_eq!(queued, SettingsApplyTiming::QueuedForNextTurn);
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
        project_session(&session).model
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
    assert_eq!(updated, SettingsApplyTiming::AppliedNow);
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

#[test]
fn persisted_selector_keeps_reasoning_effort_across_resume() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let thread_id = "11111111-2222-3333-4444-555555555555";

    // 直接落盘带 reasoning 的 thread_settings metadata，绕过选择器校验，
    // 聚焦恢复投影的三段保真。
    let mut session = SessionManager::create_with_id(Path::new("."), &sessions, thread_id)
        .expect("create session file");
    session
        .append_metadata(
            SessionMetadata::thread_settings(
                "openai_compatible".to_string(),
                "base-model".to_string(),
                Some("high".to_string()),
            )
            .expect("thread settings metadata"),
        )
        .expect("append settings");
    drop(session);

    let resumed = resume_thread(&sessions, thread_id).expect("resume thread");
    assert_eq!(
        resumed.model.as_deref(),
        Some("openai_compatible/base-model#high"),
        "resume must keep the reasoning effort segment"
    );
    let reopened = SessionManager::open_existing(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen session file");
    assert_eq!(
        project_session(&reopened).model.as_deref(),
        Some("openai_compatible/base-model#high"),
        "persisted selector projection keeps all three segments"
    );
}

#[test]
fn settings_persistence_failure_keeps_intent_and_fails_run() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::new(RequestLog::default()),
            requested: Arc::clone(&requested),
        }),
        Some("base-model"),
    ));
    let thread_id = shared.thread().unwrap().thread_id;
    let path = sessions.join(format!("{thread_id}.jsonl"));

    // 在 sink 回调里让 JSONL 变为只读：会话复用每次写盘都重新打开文件，
    // 但运行中的 turn 全部写入（终态 metadata 等）都在 turn/completed 事件
    // 之前完成；本窗口只截断紧随其后的设置持久化重开，其余写入不受影响。
    let readonly_at_terminal = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let turn_readonly = Arc::clone(&readonly_at_terminal);
    let sink_path = path.clone();
    let sink = move |event: TurnEvent| {
        if matches!(event, TurnEvent::TurnCompleted { .. }) {
            let metadata = std::fs::metadata(&sink_path).expect("jsonl exists");
            let mut permissions = metadata.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&sink_path, permissions).expect("make jsonl readonly");
            turn_readonly.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    };

    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut sink = sink;
            shared.run_turn("first turn", &mut sink)
        })
    };
    wait_for_requested(&requested);
    shared
        .queue_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue during turn");

    release_tx.send(()).expect("release");
    let result = turn_thread.join().expect("join");
    let error = result.expect_err("settings persistence failure must fail the run");
    assert!(
        matches!(error, ConversationError::Configuration(_)),
        "expected a settings persistence error, got {error:?}"
    );
    assert!(
        readonly_at_terminal.load(std::sync::atomic::Ordering::SeqCst),
        "the readonly window must have been armed by the terminal event"
    );

    // 意图保留：线程投影未更新，JSONL 没有 thread_settings 记录。
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("base-model"),
        "intent stays queued; projection keeps the old selector"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        0,
        "no thread_settings metadata was persisted"
    );

    // 恢复可写后同一意图可被继续消费（下一 turn 的终态路径或空闲重提）。
    // 临时目录内的测试文件恢复可写；Unix 权限放宽只影响该临时文件本身。
    #[allow(clippy::permissions_set_readonly_false)]
    {
        let mut permissions = std::fs::metadata(&path).expect("jsonl").permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).expect("restore writable");
    }
    let timing = shared
        .queue_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            ..SettingsPatch::default()
        })
        .expect("re-apply after restore");
    assert_eq!(timing, SettingsApplyTiming::AppliedNow);
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
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
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
fn steer_affects_only_the_current_turn() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedRecordingProvider {
            release: std::sync::Mutex::new(release_rx),
            log: Arc::clone(&log),
            requested: Arc::clone(&requested),
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
    wait_for_requested(&requested);
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
fn follow_up_submitted_at_terminal_is_consumed_and_never_lingers() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (gate_tx, gate_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(SequentialGatedProvider {
            gates: std::sync::Mutex::new(vec![std::sync::Mutex::new(gate_rx)].into()),
            log: Arc::clone(&log),
            requests: Arc::clone(&requests),
        }),
        None,
    ));
    let injection = Arc::new(std::sync::Mutex::new(None::<bool>));

    let turn_thread = {
        let shared = Arc::clone(&shared);
        let injection = Arc::clone(&injection);
        let sink_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut sink = move |event: TurnEvent| {
                // 终态收敛瞬间注入 followUp（受控调度：sink 运行于链条线程，
                // 先于下一次取队列；只注入一次避免自续链）。
                if matches!(event, TurnEvent::TurnCompleted { .. }) {
                    let mut slot = injection.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(sink_shared.submit_follow_up("terminal injection"));
                    }
                }
            };
            shared.run_turn("initial input", &mut sink)
        })
    };
    wait_for_request_count(&requests, 1);
    gate_tx.send(()).expect("release first turn");
    let outcome = turn_thread
        .join()
        .expect("chain thread")
        .expect("chain completes");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);

    // 终态收敛瞬间的注入被接受并恰好执行一次，不允许滞留。
    assert_eq!(
        *injection.lock().unwrap(),
        Some(true),
        "terminal injection must be accepted while the chain is closing"
    );
    assert_eq!(
        log.input_sequence(),
        vec![
            "initial input".to_string(),
            "terminal injection".to_string()
        ],
        "terminal injection runs as its own turn exactly once"
    );
    assert!(
        shared.pending_follow_ups().is_empty(),
        "accepted follow-up must never linger after the chain ends"
    );
    // 链条结束后窗口已关闭：后续提交被明确拒绝。
    assert!(
        !shared.submit_follow_up("after close"),
        "closed chain window must reject follow-ups"
    );
    assert!(!shared.has_active_turn());
}

#[test]
fn preparation_failure_requeues_current_input_at_queue_head() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (gate1_tx, gate1_rx) = mpsc::channel();
    let (gate2_tx, gate2_rx) = mpsc::channel();
    let log = Arc::new(RequestLog::default());
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(SequentialGatedProvider {
            gates: std::sync::Mutex::new(
                vec![
                    std::sync::Mutex::new(gate1_rx),
                    std::sync::Mutex::new(gate2_rx),
                ]
                .into(),
            ),
            log: Arc::clone(&log),
            requests: Arc::clone(&requests),
        }),
        None,
    ));
    let thread_id = shared.thread().unwrap().thread_id;
    let session_path = sessions.join(format!("{thread_id}.jsonl"));

    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let mut sink = EventCollector::default().sink();
            shared.run_turn("explicit input", &mut sink)
        })
    };
    wait_for_request_count(&requests, 1);
    assert!(
        shared.submit_follow_up("second input"),
        "follow-up accepted while the first turn runs"
    );
    assert!(
        shared.submit_follow_up("third input"),
        "follow-up accepted while the first turn runs"
    );
    gate1_tx.send(()).expect("release first turn");
    wait_for_request_count(&requests, 2);
    assert!(
        shared.submit_follow_up("fourth input"),
        "follow-up accepted while the second turn runs"
    );
    // 删除会话文件：下一轮准备阶段（open_and_repair_session）必然失败。
    std::fs::remove_file(&session_path).expect("remove session file");
    gate2_tx.send(()).expect("release second turn");

    let result = turn_thread.join().expect("chain thread");
    let error = result.expect_err("preparation failure aborts the chain");
    assert!(
        matches!(
            error,
            ConversationError::Turn(TurnRunError::Preparation { .. })
        ),
        "expected preparation failure, got {error:?}"
    );
    // 本轮输入（third input）回到队首，其余已接受输入保持相对顺序。
    assert_eq!(
        shared.pending_follow_ups(),
        vec!["third input", "fourth input"],
        "failed current input returns to the head; the rest keep order"
    );
    // 失败输入从未执行；窗口已释放。
    assert_eq!(
        log.input_sequence(),
        vec!["explicit input", "second input"],
        "failed current input never executed"
    );
    assert!(
        !shared.has_active_turn(),
        "aborted chain releases the window"
    );
}

#[test]
fn panic_in_turn_releases_the_reservation_window() {
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
    let log = Arc::new(RequestLog::default());
    let shared = new_conversation(
        &sessions,
        Arc::new(RecordingProvider {
            text: "ok".into(),
            log: Arc::clone(&log),
        }),
        Some("base-model"),
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
        shared.submit_follow_up("queued while reserved"),
        "followUp is protected by the reserved chain window"
    );
    let timing = shared
        .queue_settings(SettingsPatch {
            provider: Some("openai_compatible".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue settings during reservation");
    assert_eq!(timing, SettingsApplyTiming::QueuedForNextTurn);
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("base-model"),
        "reserved settings must not change the current projection"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        0,
        "reserved settings must not persist before a trusted terminal"
    );

    // 未消费的预订 drop 后窗口释放；已接受的 followUp 与设置意图保留，
    // 随后由下一条执行链按合同消费。
    drop(reservation);
    assert!(!shared.has_active_turn());
    let outcome = shared.run_turn("now it runs", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert!(shared.pending_follow_ups().is_empty());
    assert_eq!(
        log.input_sequence(),
        vec!["queued while reserved", "now it runs"]
    );
    assert_eq!(thread_settings_count(&sessions, &thread_id), 1);
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model")
    );
}

#[test]
fn reasoning_patch_keeps_sets_and_clears_selector_effort() {
    let current = Some("openai_compatible/base-model#medium");
    for (reasoning, expected) in [
        (ReasoningPatch::Keep, "openai_compatible/base-model#medium"),
        (
            ReasoningPatch::Set("high".to_string()),
            "openai_compatible/base-model#high",
        ),
        (ReasoningPatch::Clear, "openai_compatible/base-model"),
    ] {
        assert_eq!(
            compose_merged_selector(
                current,
                &SettingsPatch {
                    reasoning,
                    ..SettingsPatch::default()
                }
            ),
            expected
        );
    }
}

#[test]
fn compact_releases_its_busy_window_when_the_provider_panics() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(&sessions, Arc::new(PanickingProvider), None);
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
        let _ = conversation.compact();
    }));

    assert!(panic.is_err(), "the provider panic must propagate");
    assert!(
        !conversation.has_active_turn(),
        "compaction must release the single-writer window while unwinding"
    );
}
