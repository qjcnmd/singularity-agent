//! 协调器的并发安全护栏：panic 与锁中毒路径的窗口释放、单写者锁冲突、
//! 预订窗口的回收、写者锁占用下设置提交仍被接受。turn 链行为（lifecycle
//! 事件、steer/followUp）的行为回归由评估器与真实使用兜底，不在此重复。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::path::Path;
use std::sync::Arc;

use crate::events::TurnEvent;
use crate::objects::TurnStatus;
use crate::runner::TurnRunner;
use crate::store::{ThreadLockCoordinator, create_thread, resume_thread};
use crate::{Conversation, SettingsApplyTiming, SettingsPatch};
use singularity_agent::message::{AgentMessage, AgentMessageRole};
use singularity_agent::session::{
    SessionManager, SessionMetadata, SessionMetadataKind, WriterLockCoordinator,
};
use singularity_model::{
    ModelTurnRequest, ModelTurnResponse, Provider, ProviderError, ProviderProtocolContract,
};

fn temp_sessions() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("temp home");
    std::fs::create_dir_all(dir.path().join("sessions")).expect("sessions dir");
    dir
}

/// 每个测试独立临时目录，各自构造进程级协调器即可。
fn coordinator(sessions: &Path) -> ThreadLockCoordinator {
    Arc::new(WriterLockCoordinator::new(sessions))
}

fn snapshot() -> singularity_model::ProviderConfigSnapshot {
    // 目录快照来自隔离的用户配置目录：config.json 声明 openai_compatible 的
    // base-model 与 base-model-2，auth.json 提供测试 key。fake provider 经
    // provider_override 注入，不经 HTTP；
    // Handle 背后的 runtime 无需存活。
    static FIXTURE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let home = FIXTURE.get_or_init(|| {
        let directory = tempfile::tempdir().expect("snapshot fixture home");
        let path = directory.path().to_path_buf();
        let config = serde_json::json!({
            "version": 1,
            "default_provider": "openai_compatible",
            "default_model": "openai_compatible/base-model",
            "providers": {
                "openai_compatible": {
                    "base_url": "http://127.0.0.1:9/v1",
                    "models": {
                        "base-model": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128_000,
                            "max_output_tokens": 4_096
                        },
                        "base-model-2": {
                            "api_protocol": "chat",
                            "max_context_tokens": 128_000,
                            "max_output_tokens": 4_096
                        }
                    }
                }
            }
        });
        std::fs::write(path.join("config.json"), config.to_string()).expect("write fixture config");
        let auth = serde_json::json!({
            "schema_version": 1,
            "providers": { "openai_compatible": { "api_key": "test-key-placeholder" } }
        });
        let auth_path = path.join("auth.json");
        std::fs::write(&auth_path, auth.to_string()).expect("write fixture auth");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict fixture auth");
        }
        // fixture 目录随进程存活：capture 按目录读取两文件。
        std::mem::forget(directory);
        path
    });
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let handle = runtime.handle().clone();
    std::mem::forget(runtime);
    singularity_model::ProviderConfigSnapshot::capture_from_directory(home, handle)
}

/// 记录每次模型请求，用于断言各 turn 实际看到的输入。
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
        _on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.log.record(request);
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "resp-fake",
            self.text.clone(),
        ))
    }
}

/// 首次请求时发出 started 信号并阻塞，直到测试放行：把断言精确锚定在
/// 「turn 已在执行、写者锁已被占用」的时刻。后续请求直接放行（测试的
/// 第二轮在主线程断言完成后运行）。
struct GateProvider {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl Provider for GateProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
        _on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        self.started.send(()).expect("test is still waiting");
        if let Some(release) = self.release.lock().expect("gate").take() {
            release.recv().expect("test releases the gate");
        }
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "resp-gate",
            "done".to_string(),
        ))
    }
}

struct PanickingProvider;

impl Provider for PanickingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
        _on_attempt: &mut dyn FnMut(singularity_model::ProviderAttemptEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        panic!("compaction provider panic")
    }
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
                provider
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or(singularity_model::DEFAULT_PROVIDER_NAME),
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
        shared.submit_follow_up("queued while reserved"),
        "followUp is protected by the reserved chain window"
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

    // 未消费的预订 drop 后窗口释放；已接受的 followUp 保留，
    // 随后由下一条执行链按合同消费。
    drop(reservation);
    assert!(!shared.has_active_turn());
    let outcome = shared.run_turn("now it runs", &mut sink).expect("runs");
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    assert!(shared.pending_follow_ups().is_empty());
    assert_eq!(
        log.input_sequence(),
        vec![
            "queued while reserved".to_string(),
            "now it runs".to_string()
        ]
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "the turn recorded the effective selector at its start"
    );
}

/// 运行中改设置走与空闲时同一条提交路径：写者锁被活动 turn 占用时提交点
/// 仍只更新内存投影（不写文件、不报错），落盘由下一 turn 开始时记录——
/// 对齐 codex 的 turn 边界记录编排。
#[test]
fn settings_update_mid_turn_is_accepted_and_recorded_at_next_turn_start() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let conversation = new_conversation(
        &sessions,
        Arc::new(GateProvider {
            started: started_tx,
            release: std::sync::Mutex::new(Some(release_rx)),
        }),
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
    let conversation = new_conversation(sessions.path(), Arc::new(PanickingProvider), None);
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
