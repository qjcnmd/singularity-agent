//! 协调器与 turn 管线的行为测试：以最小 fake Provider 驱动真实管线，
//! 验证单活动 turn、事件顺序、失败收敛、取消与设置生效时序。

use std::sync::Arc;
use std::sync::mpsc;

use singularity_agent::session::SessionManager;
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

/// 一次成功完成、返回固定文本的 provider。
struct StaticProvider {
    text: String,
}

impl Provider for StaticProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &singularity_core::CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "resp-fake",
            self.text.clone(),
        ))
    }
}

/// 第一次调用即失败的 provider。
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

fn new_conversation(
    sessions: &std::path::Path,
    provider: Arc<dyn Provider + Send + Sync>,
    model: Option<&str>,
) -> Conversation {
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

#[test]
fn successful_turn_emits_lifecycle_events_and_final_text() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let conversation = new_conversation(
        &sessions,
        Arc::new(StaticProvider {
            text: "done".into(),
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

/// 在 provider 内部挂起直到外部放行，制造确定的活动 turn 窗口。
struct GatedProvider {
    release: std::sync::Mutex<mpsc::Receiver<()>>,
}

impl Provider for GatedProvider {
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
        Ok(ModelTurnResponse::completed(
            request.request_id.clone(),
            "r",
            "ok",
        ))
    }
}

#[test]
fn only_one_turn_may_be_active_at_a_time() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedProvider {
            release: std::sync::Mutex::new(release_rx),
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
    for _ in 0..400 {
        if shared.has_active_turn() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(shared.has_active_turn(), "first turn should be active");
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

#[test]
fn settings_queue_applies_after_the_active_turn_completes() {
    let home = temp_sessions();
    let sessions = home.path().join("sessions");
    let (release_tx, release_rx) = mpsc::channel();
    let shared = Arc::new(new_conversation(
        &sessions,
        Arc::new(GatedProvider {
            release: std::sync::Mutex::new(release_rx),
        }),
        Some("openai_compatible/base-model"),
    ));
    let turn_thread = {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let (_, mut sink) = collect_sink();
            shared.run_turn("hello", &mut sink)
        })
    };
    for _ in 0..400 {
        if shared.has_active_turn() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    fn thread_settings_count(sessions: &std::path::Path, thread_id: &str) -> usize {
        SessionManager::open_existing(&sessions.join(format!("{thread_id}.jsonl")))
            .expect("reopen")
            .metadata_entries()
            .iter()
            .filter(|entry| {
                entry.kind() == singularity_agent::session::SessionMetadataKind::ThreadSettings
            })
            .count()
    }
    let thread_id = shared.thread().unwrap().thread_id;

    // 无效 selector（快照无法解析的模型）在排队前即被拒绝。
    let rejected = shared.queue_settings(SettingsPatch {
        model: Some("no-such-model".to_string()),
        ..SettingsPatch::default()
    });
    assert!(
        rejected.is_err(),
        "invalid selector must be rejected eagerly"
    );

    // 活动 turn 期间：合法 patch 只排队，不落盘、不改内存投影。
    // （legacy 环境层只允许重选当前模型；此处验证的是排队时序本身。）
    let queued = shared
        .queue_settings(SettingsPatch {
            model: Some("base-model".to_string()),
            ..SettingsPatch::default()
        })
        .expect("queue settings");
    assert!(queued);
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model"),
        "queued settings must not apply mid-turn"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        0,
        "queued settings must not persist mid-turn"
    );
    release_tx.send(()).expect("release");
    turn_thread.join().expect("join").expect("turn ok");

    // turn 终态收敛后应用排队意图：更新投影并持久化 thread_settings。
    let queued = shared.take_queued_settings().expect("queued intent");
    shared.apply_queued_settings(queued).expect("apply queued");
    assert_eq!(
        shared.thread().unwrap().model.as_deref(),
        Some("openai_compatible/base-model"),
        "projection reflects the applied selector"
    );
    assert_eq!(
        thread_settings_count(&sessions, &thread_id),
        1,
        "applied settings persist as thread metadata"
    );
    let session = SessionManager::open_existing(&sessions.join(format!("{thread_id}.jsonl")))
        .expect("reopen");
    assert_eq!(
        persisted_model_selector(&session).as_deref(),
        Some("openai_compatible/base-model"),
        "persisted settings project back to the same selector"
    );
}
