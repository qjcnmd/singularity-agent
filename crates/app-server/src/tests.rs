use std::sync::{Arc, Mutex};

use singularity_agent::{agent::AgentOutcome, session::SessionManager};
use singularity_model::{
    ModelError, ModelErrorKind, ModelRole, ModelToolCall, ModelToolParseStatus,
    ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage, Provider, ProviderError,
    ProviderProtocolContract,
};

use singularity_protocol::{ConversationRole, TurnInputDelivery};

use super::*;

fn app_server(store: SessionStore) -> AppServer {
    AppServer::new(
        store,
        ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            None,
            None,
        ),
    )
}

#[test]
fn initialized_request_is_rejected_as_an_invalid_envelope() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");

    let response = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","id":2,"params":{}}"#)
        .expect("initialized request");

    assert_eq!(response.len(), 1);
    assert_eq!(response[0]["jsonrpc"], "2.0");
    assert_eq!(response[0]["id"], 2);
    assert_eq!(response[0]["error"]["code"], -32600);
    assert_eq!(response[0]["error"]["message"], "Invalid Request");

    let still_uninitialized = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"server/capabilities","id":3,"params":{}}"#)
        .expect("server remains unacknowledged");
    assert_eq!(
        still_uninitialized[0]["error"]["message"],
        "Not initialized"
    );
}

#[test]
fn event_subscription_binds_gap_cursor_to_one_output_reservation() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");

    let message = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"event/subscribe","id":2,"params":{"eventTypes":["thread/started"]}}"#,
        )
        .expect("subscription request");
    let outputs = server
        .handle_with_output(message)
        .expect("subscription outputs");

    assert_eq!(outputs.len(), 2);
    assert_eq!(
        outputs[1].reservation.order,
        outputs[0].reservation.order + 1
    );
    assert_eq!(outputs[0].reservation.event_cursor, Some(1));
    assert_eq!(outputs[1].reservation.event_cursor, None);
    assert_eq!(outputs[0].message["params"]["event"]["cursor"], 1);
    assert_eq!(outputs[1].message["result"]["cursor"], 1);
}

#[test]
fn duplicate_activation_preserves_the_original_and_global_stop_cancels_future_turns() {
    let server = app_server(SessionStore::open(":memory:").expect("store"));
    let (original, _guard) = server.activate_turn("turn_1").expect("activate turn");

    let duplicate = server.activate_turn("turn_1");

    assert!(matches!(duplicate, Err(AppServerError::Workspace(_))));
    assert!(!original.is_cancelled());
    server
        .request_execution_stop()
        .expect("request execution stop");
    assert!(original.is_cancelled());
    let (late, _late_guard) = server.activate_turn("turn_2").expect("late activation");
    assert!(late.is_cancelled());
}

#[test]
fn cancellation_monitor_classifies_non_contention_store_failure_as_infrastructure() {
    let token = CancellationToken::new();
    let monitor = cancellation_monitor(
        Some(SessionStore::open(":memory:").expect("store")),
        "missing-turn",
        token.clone(),
    )
    .expect("monitor setup")
    .expect("monitor");
    monitor.control.started.store(true, Ordering::SeqCst);
    monitor.control.wake.send(()).expect("start monitor");
    monitor
        .done
        .recv_timeout(Duration::from_secs(1))
        .expect("monitor completion");
    assert_eq!(
        CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );
    assert!(token.is_cancelled());
}

#[test]
fn cancellation_monitor_classifies_persisted_user_cancellation_separately() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let monitor_store = store.trusted_reopen().expect("monitor store");
    let token = CancellationToken::new();
    let monitor = cancellation_monitor(Some(monitor_store), &turn.turn_id, token.clone())
        .expect("monitor setup")
        .expect("monitor");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Running,
            AgentStatus::CancelRequested.as_str(),
        )
        .expect("request cancellation");
    monitor.control.started.store(true, Ordering::SeqCst);
    monitor.control.wake.send(()).expect("start monitor");
    monitor
        .done
        .recv_timeout(Duration::from_secs(1))
        .expect("monitor completion");
    assert_eq!(
        CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::UserCancellation)
    );
    assert!(token.is_cancelled());
}

#[test]
fn cancellation_monitor_classifies_external_cancellation_as_user_cancellation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let monitor_store = store.trusted_reopen().expect("monitor store");
    let token = CancellationToken::new();
    let monitor = cancellation_monitor(Some(monitor_store), &turn.turn_id, token.clone())
        .expect("monitor setup")
        .expect("monitor");
    token.cancel();
    monitor.control.started.store(true, Ordering::SeqCst);
    monitor.control.wake.send(()).expect("start monitor");
    monitor
        .done
        .recv_timeout(Duration::from_secs(1))
        .expect("monitor completion");
    assert_eq!(
        CancellationMonitorOutcome::from_code(monitor.control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::UserCancellation)
    );
}

fn in_flight_monitor_for_teardown_test(
    cancellation: &CancellationToken,
    shutdown_wait: Duration,
) -> (
    CancellationMonitor,
    Arc<CancellationMonitorControl>,
    Sender<()>,
    Receiver<bool>,
    Receiver<()>,
) {
    let (wake, _wake_receiver) = mpsc::channel();
    let (done_sender, done) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    let (published_sender, published_receiver) = mpsc::channel();
    let (finished_sender, finished_receiver) = mpsc::channel();
    let control = Arc::new(CancellationMonitorControl {
        started: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        outcome: AtomicU8::new(0),
        wake,
    });
    let thread_control = Arc::clone(&control);
    let thread_cancellation = cancellation.clone();
    let thread = std::thread::spawn(move || {
        release_receiver.recv().expect("release in-flight monitor");
        let published = thread_control.record_outcome(CancellationMonitorOutcome::UserCancellation);
        if published {
            thread_cancellation.cancel();
        }
        published_sender.send(published).expect("published result");
        let _ = done_sender.send(());
        finished_sender.send(()).expect("monitor finished");
    });
    (
        CancellationMonitor {
            control: Arc::clone(&control),
            done,
            thread: Some(thread),
            shutdown_wait,
        },
        control,
        release_sender,
        published_receiver,
        finished_receiver,
    )
}

#[test]
fn in_flight_monitor_timeout_freezes_before_late_cancellation() {
    let cancellation = CancellationToken::new();
    let (monitor, control, release, published, finished) =
        in_flight_monitor_for_teardown_test(&cancellation, Duration::ZERO);

    assert_eq!(
        monitor.stabilize(&cancellation),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );
    assert!(cancellation.is_cancelled());
    release.send(()).expect("release monitor");
    assert!(!published.recv().expect("late publication result"));
    finished.recv().expect("monitor finished");
    assert_eq!(
        CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );
}

#[test]
fn drop_timeout_freezes_before_detached_monitor_can_cancel() {
    let cancellation = CancellationToken::new();
    let (monitor, control, release, published, finished) =
        in_flight_monitor_for_teardown_test(&cancellation, Duration::ZERO);
    {
        let _guard = ActiveTurnGuard {
            turn_id: "drop-timeout-turn".to_string(),
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            steer_handles: Arc::new(Mutex::new(HashMap::new())),
            cancellation: cancellation.clone(),
            monitor: Some(monitor),
            stabilized_monitor_outcome: None,
        };
    }

    assert!(cancellation.is_cancelled());
    assert_eq!(
        CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );
    release.send(()).expect("release detached monitor");
    assert!(!published.recv().expect("late publication result"));
    finished.recv().expect("monitor finished");
    assert_eq!(
        CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );
}

#[test]
fn frozen_monitor_outcome_wins_over_late_cancellation_and_preserves_safe_states() {
    let (wake, _wake_receiver) = mpsc::channel();
    let (done_sender, done) = mpsc::channel();
    let (recorded_sender, recorded) = mpsc::channel();
    let control = Arc::new(CancellationMonitorControl {
        started: AtomicBool::new(true),
        stop: AtomicBool::new(false),
        outcome: AtomicU8::new(0),
        wake,
    });
    let thread_control = Arc::clone(&control);
    std::thread::spawn(move || {
        thread_control.record_outcome(CancellationMonitorOutcome::InfrastructureFailure);
        recorded_sender.send(()).expect("recorded outcome");
        done_sender.send(()).expect("monitor done");
    });
    recorded.recv().expect("monitor outcome recorded");
    let token = CancellationToken::new();

    let mut guard = ActiveTurnGuard {
        turn_id: "synthetic-turn".to_string(),
        active_turns: Arc::new(Mutex::new(HashMap::new())),
        steer_handles: Arc::new(Mutex::new(HashMap::new())),
        cancellation: token.clone(),
        monitor: Some(CancellationMonitor {
            control: Arc::clone(&control),
            done,
            thread: None,
            shutdown_wait: Duration::from_millis(TURN_MONITOR_SHUTDOWN_WAIT_MS),
        }),
        stabilized_monitor_outcome: None,
    };
    token.cancel();
    assert_eq!(
        guard.stabilize_monitor(&token),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );
    control.record_outcome(CancellationMonitorOutcome::UserCancellation);
    assert_eq!(
        CancellationMonitorOutcome::from_code(control.outcome.load(Ordering::SeqCst)),
        Some(CancellationMonitorOutcome::InfrastructureFailure)
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("running turn");
    let server = app_server(store);
    let failure_status = RunStatus::failed("late monitor failure");
    assert!(matches!(
        server.commit_turn_run_status(
            turn.clone(),
            &failure_status,
            None,
            &token,
            Some(CancellationMonitorOutcome::InfrastructureFailure),
        ),
        Err(AppServerError::TurnExecution {
            stage: TurnFailureStage::CancellationMonitor,
            cause: TurnFailureCause::CancellationMonitor,
        })
    ));
    let mut emitted = Vec::new();
    let mut emit = |message| emitted.push(message);
    let failure = TurnFailure {
        stage: TurnFailureStage::CancellationMonitor,
        cause: TurnFailureCause::CancellationMonitor,
    };
    assert!(matches!(
        server.finish_turn_failure(
            &mut emit,
            &turn,
            None,
            &token,
            Some(CancellationMonitorOutcome::InfrastructureFailure),
            failure,
        ),
        Err(AppServerError::TurnExecution {
            stage: TurnFailureStage::CancellationMonitor,
            cause: TurnFailureCause::CancellationMonitor,
        })
    ));
    let failed = server.store.get_turn(&turn.turn_id).expect("failed turn");
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(failed.agent_loop_status, AgentStatus::Failed.as_str());

    let cancelled_turn = server
        .store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("cancelled turn");
    let cancelled_token = CancellationToken::new();
    cancelled_token.cancel();
    server
        .commit_turn_run_status(
            cancelled_turn.clone(),
            &RunStatus::failed("late user result"),
            None,
            &cancelled_token,
            Some(CancellationMonitorOutcome::UserCancellation),
        )
        .expect("user cancellation commit");
    let interrupted = server
        .store
        .get_turn(&cancelled_turn.turn_id)
        .expect("interrupted turn");
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    assert_eq!(
        interrupted.agent_loop_status,
        AgentStatus::Cancelled.as_str()
    );

    let blocked_thread = server
        .store
        .create_thread(None, None)
        .expect("blocked thread");
    let blocked_turn = server
        .store
        .create_turn(&blocked_thread.thread_id, AgentStatus::Running.as_str())
        .expect("blocked turn");
    server
        .store
        .update_turn_state(
            &blocked_turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked state");
    let blocked_result = server
        .terminalize_turn_failure(
            &blocked_turn,
            &token,
            Some(CancellationMonitorOutcome::InfrastructureFailure),
            failure,
        )
        .expect("preserve blocked turn");
    assert!(matches!(
        blocked_result,
        TurnTerminalizationResult::Preserved
    ));
    assert_eq!(
        server
            .store
            .get_turn(&blocked_turn.turn_id)
            .expect("blocked")
            .status,
        TurnStatus::Blocked
    );

    let completed_thread = server
        .store
        .create_thread(None, None)
        .expect("completed thread");
    let completed_turn = server
        .store
        .create_turn(&completed_thread.thread_id, AgentStatus::Running.as_str())
        .expect("completed turn");
    server
        .store
        .update_turn_state(
            &completed_turn.turn_id,
            TurnStatus::Completed,
            AgentStatus::Completed.as_str(),
        )
        .expect("completed state");
    let completed_result = server
        .terminalize_turn_failure(
            &completed_turn,
            &token,
            Some(CancellationMonitorOutcome::InfrastructureFailure),
            failure,
        )
        .expect("preserve completed turn");
    assert!(matches!(
        completed_result,
        TurnTerminalizationResult::Preserved
    ));
    assert_eq!(
        server
            .store
            .get_turn(&completed_turn.turn_id)
            .expect("completed")
            .status,
        TurnStatus::Completed
    );
}

#[test]
fn turn_started_event_failure_terminalizes_the_running_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    let mut server = app_server(store);
    let filter = Arc::clone(&server.event_filter);
    let poisoned = std::thread::spawn(move || {
        let _guard = filter.lock().expect("event filter");
        panic!("poison event filter");
    })
    .join();
    assert!(poisoned.is_err());

    let message = JsonRpcMessage::request(
        Method::TurnStart,
        1,
        json!({
            "threadId": &thread.thread_id,
            "input": [{"type": "text", "text": "event failure"}]
        }),
    )
    .expect("turn start request");
    let error = server
        .handle_turn_start_streaming(message, |_| {})
        .expect_err("event failure must be surfaced");
    assert!(matches!(
        error,
        AppServerError::TurnTerminalization {
            stage: TurnFailureStage::EventNotification,
            cause: TurnFailureCause::Workspace,
            failure: TurnTerminalizationFailure::EventNotification,
        }
    ));
    let persisted = server
        .store
        .list_threads()
        .expect("threads")
        .first()
        .expect("thread")
        .thread_id
        .clone();
    let turns = server
        .store
        .read_thread_history(&persisted, None, 8)
        .expect("history");
    assert!(turns.messages.is_empty());
    let failed = server
        .store
        .list_trace(&persisted)
        .expect("trace")
        .into_iter()
        .find(|trace| trace.payload["status"] == AgentStatus::Failed.as_str())
        .expect("failed terminal trace");
    assert_eq!(failed.payload["status"], AgentStatus::Failed.as_str());
}

#[test]
fn late_turn_failure_does_not_overwrite_a_blocked_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let server = app_server(store);

    assert!(
        server
            .commit_turn_run_status(
                turn.clone(),
                &RunStatus::failed("stale run failure"),
                None,
                &CancellationToken::new(),
                None,
            )
            .is_err()
    );
    let persisted = server
        .store
        .get_turn(&turn.turn_id)
        .expect("persisted turn");
    assert_eq!(persisted.status, TurnStatus::Blocked);
    assert_eq!(persisted.agent_loop_status, AgentStatus::Blocked.as_str());
}

#[test]
fn running_turn_failure_stages_terminalize_as_failed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let stages = [
        TurnFailureStage::AgentLoop,
        TurnFailureStage::ApprovalCheckpoint,
        TurnFailureStage::TerminalOutcome,
    ];
    let mut turns = Vec::new();
    for stage in stages {
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        turns.push((turn, stage));
    }
    let server = app_server(store);

    for (turn, stage) in turns {
        if stage == TurnFailureStage::TerminalOutcome {
            let invalid_commit =
                RunStatus::failed("invalid completion").with_status(AgentStatus::Completed);
            assert!(
                server
                    .commit_turn_run_status(
                        turn.clone(),
                        &invalid_commit,
                        None,
                        &CancellationToken::new(),
                        None,
                    )
                    .is_err()
            );
        }
        let mut emitted = Vec::new();
        let mut emit = |message| emitted.push(message);
        let result = server.finish_turn_failure(
            &mut emit,
            &turn,
            None,
            &CancellationToken::new(),
            None,
            stage,
        );
        assert!(matches!(
            result,
            Err(AppServerError::TurnExecution { stage: actual, .. }) if actual == stage
        ));
        let persisted = server.store.get_turn(&turn.turn_id).expect("failed turn");
        assert_eq!(persisted.status, TurnStatus::Failed);
        assert_eq!(persisted.agent_loop_status, AgentStatus::Failed.as_str());
        let trace = server
            .store
            .list_trace(&persisted.thread_id)
            .expect("trace")
            .into_iter()
            .find(|trace| {
                trace.payload["audit_events"]
                    .to_string()
                    .contains(stage.as_str())
            })
            .expect("typed failure trace");
        assert!(
            trace.payload["audit_events"]
                .to_string()
                .contains("turn_execution")
        );
    }
}

#[test]
fn terminalization_preserves_interrupted_and_blocked_turns() {
    let cases = [
        (TurnStatus::Interrupted, AgentStatus::Cancelled),
        (TurnStatus::Blocked, AgentStatus::Blocked),
    ];
    for (expected_status, expected_agent_status) in cases {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                expected_status.clone(),
                expected_agent_status.as_str(),
            )
            .expect("safe state");
        let server = app_server(store);
        let mut emitted = Vec::new();
        let mut emit = |message| emitted.push(message);

        let result = server.finish_turn_failure(
            &mut emit,
            &turn,
            None,
            &CancellationToken::new(),
            None,
            TurnFailureStage::AgentLoop,
        );

        assert!(matches!(
            result,
            Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::AgentLoop,
                ..
            })
        ));
        let persisted = server.store.get_turn(&turn.turn_id).expect("safe turn");
        assert_eq!(persisted.status, expected_status);
        assert_eq!(persisted.agent_loop_status, expected_agent_status.as_str());
    }
}

#[test]
fn terminalization_failure_keeps_stage_and_redacts_cleanup_cause() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let mut missing_turn = turn;
    missing_turn.turn_id = "missing-turn-with-secret-path".to_string();
    let server = app_server(store);
    let mut emitted = Vec::new();
    let mut emit = |message| emitted.push(message);

    let error = server
        .finish_turn_failure(
            &mut emit,
            &missing_turn,
            None,
            &CancellationToken::new(),
            None,
            TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
            },
        )
        .expect_err("terminalization must report its cleanup failure");
    assert!(matches!(
        error,
        AppServerError::TurnTerminalization {
            stage: TurnFailureStage::TerminalOutcome,
            cause: TurnFailureCause::Store,
            failure: TurnTerminalizationFailure::Store,
        }
    ));
    assert!(!error.to_string().contains("missing-turn-with-secret-path"));
}

#[test]
fn monitor_open_failure_does_not_publish_active_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let server = app_server(SessionStore::open(&db_path).expect("store"));
    std::fs::hard_link(&db_path, dir.path().join("sessions-alias.sqlite3"))
        .expect("hard link store");

    assert!(matches!(
        server.activate_turn("turn_monitor_failure"),
        Err(AppServerError::Store(StoreError::InvalidState(message)))
            if message.contains("hard links")
    ));
    assert!(
        server
            .active_turns
            .lock()
            .expect("active turn registry")
            .is_empty()
    );
}

#[test]
fn turn_start_monitor_failure_does_not_persist_a_running_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("store");
    let thread = store
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    let mut server = app_server(store);
    std::fs::hard_link(&db_path, dir.path().join("sessions-alias.sqlite3"))
        .expect("hard link store");
    let message: JsonRpcMessage = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "turn/start",
        "params": {
            "threadId": &thread.thread_id,
            "input": [{"type": "text", "text": "must not persist"}]
        }
    }))
    .expect("turn/start message");

    assert!(matches!(
        server.handle_turn_start_streaming(message, |_| {}),
        Err(AppServerError::Store(StoreError::InvalidState(message)))
            if message.contains("hard links")
    ));
    assert!(
        server
            .active_turns
            .lock()
            .expect("active registry")
            .is_empty()
    );
    server
        .store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("no running turn was persisted before monitor setup");
}

#[derive(Clone)]
struct StaticProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
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
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        let response_index = seen_requests.len();
        seen_requests.push(request.clone());
        let mut response = self
            .responses
            .get(response_index)
            .unwrap_or_else(|| self.responses.last().expect("static provider response"))
            .clone();
        response.request_id = request.request_id.clone();
        Ok(response)
    }
}

fn failed_model_response(error: ModelError) -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed("request_1", "response_1", "unused");
    response.status = ModelTurnStatus::Failed;
    response.assistant_message = None;
    response.error = Some(error);
    response
}

#[test]
fn app_server_preserves_safe_provider_failure_via_turn_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let provider_sentinel = "provider-body-sentinel";
    let mut provider_error =
        ModelError::new(ModelErrorKind::AuthError, provider_sentinel.to_string());
    provider_error.validation_errors = vec![provider_sentinel.to_string()];
    let provider = StaticProvider {
        responses: vec![failed_model_response(provider_error)],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    let error = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect_err("provider failure must surface");
    assert!(matches!(
        error,
        AppServerError::TurnExecution {
            stage: TurnFailureStage::AgentLoop,
            cause: TurnFailureCause::Internal,
        }
    ));
    assert!(!error.to_string().contains(provider_sentinel));
    // turn 以 Failed 终态提交，trace 只含安全摘要。
    let traces = server
        .store
        .list_trace(&thread.thread_id)
        .expect("trace");
    let terminal = traces
        .iter()
        .find(|trace| trace.payload["status"] == AgentStatus::Failed.as_str())
        .expect("failed terminal trace");
    assert_eq!(terminal.payload["error"], SAFE_AGENT_LOOP_FAILURE);
    let trace_json = serde_json::to_string(&terminal).expect("trace json");
    assert!(!trace_json.contains(provider_sentinel));
    assert!(!trace_json.contains("validation_errors"));
}

#[test]
fn agent_loop_system_prompt_loads_bounded_agents_md_from_thread_cwd() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ancestor = temp
        .path()
        .join("SINGULARITY_API_KEY=must-not-leak")
        .join("ancestor");
    let workspace = ancestor.join("workspace");
    std::fs::create_dir_all(ancestor.join(".git")).expect("ancestor git marker");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(ancestor.join("AGENTS.md"), "ancestor instructions").expect("ancestor agents");
    std::fs::write(workspace.join("AGENTS.md"), "workspace instructions")
        .expect("workspace agents");
    std::fs::write(workspace.join("AGENTS.override.md"), "workspace override")
        .expect("workspace override");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    let developer = &requests[0].messages[0].content;
    assert!(
        developer.contains("workspace override"),
        "system prompt should contain workspace override only: {developer}"
    );
    assert!(!developer.contains("ancestor instructions"));
    assert_eq!(requests[0].messages[1].role, ModelRole::User);
    assert_eq!(requests[0].messages[1].content, "user goal");
    let hidden_workspace_marker = workspace.to_string_lossy();
    assert!(!requests[0].tools.iter().any(|tool| {
        serde_json::to_string(tool)
            .expect("serialize tool")
            .contains(hidden_workspace_marker.as_ref())
    }));
}

#[test]
fn agent_loop_replays_session_file_history_across_turns() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "project instructions")
        .expect("agents instructions");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![
            ModelTurnResponse::completed(
                "model_request_turn_1_0",
                "response_1",
                "previous assistant",
            ),
            ModelTurnResponse::completed(
                "model_request_turn_2_0",
                "response_2",
                "done",
            ),
        ],
        seen_requests: Arc::clone(&seen_requests),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    for (id, input) in [(1, "previous user"), (2, "current user")] {
        let responses = server
            .turn_start(
                JsonRpcMessage::request(
                    Method::TurnStart,
                    id,
                    json!({
                        "threadId": thread.thread_id,
                        "input": [{"type": "text", "text": input}],
                    }),
                )
                .expect("request"),
            )
            .expect("turn start");
        let result: TurnStartResult = serde_json::from_value(
            responses
                .iter()
                .find(|message| message["id"] == id)
                .expect("response")["result"]
                .clone(),
        )
        .expect("turn result");
        assert_eq!(result.turn.status, TurnStatus::Completed);
    }

    // 第二轮请求上下文：上一轮 user/assistant 消息经 session 文件重放。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    let roles: Vec<ModelRole> = requests[1]
        .messages
        .iter()
        .map(|message| message.role.clone())
        .collect();
    assert_eq!(
        roles,
        vec![
            ModelRole::Developer,
            ModelRole::User,
            ModelRole::Assistant,
            ModelRole::User,
        ]
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "project instructions",
            "previous user",
            "previous assistant",
            "current user",
        ]
    );
    // 会话文件生成且消息完整（跨轮历史的唯一事实源）。
    let session_file = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join(format!("{}.jsonl", thread.thread_id));
    assert!(session_file.exists());
    let session = SessionManager::open(&session_file).expect("session");
    let entries = session.build_context_entries().expect("entries");
    let texts: Vec<String> = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            singularity_agent::session::SessionEntryType::Message(message) => {
                Some(message.content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["previous user", "previous assistant", "current user", "done"]);
}

#[test]
fn partial_realtime_item_fails_without_persisting_or_completing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let server = app_server(store);
    server
        .event_filter
        .lock()
        .expect("event filter")
        .event_types = Some(vec![
        "item/started".to_string(),
        "item/agentMessage/delta".to_string(),
        "item/failed".to_string(),
        "item/completed".to_string(),
        "turn/completed".to_string(),
    ]);

    for (label, cancelled) in [
        ("raw provider stream failure sentinel", false),
        ("raw terminal mismatch sentinel", false),
        ("raw cancellation sentinel", true),
    ] {
        let thread = server
            .store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _, _) = server
            .store
            .create_turn_with_input_and_trace(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "run"}]),
                "app_server",
                "turn started",
            )
            .expect("turn");
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        let mut events = server
            .project_assistant_delta(&mut assistant_events, "partial")
            .expect("project partial");
        let mut status = RunStatus::failed(label);
        if cancelled {
            status.status = AgentStatus::Cancelled;
        }

        let committed = server
            .commit_turn_run_status(
                turn,
                &status,
                Some(&assistant_events.item_id),
                &CancellationToken::new(),
                None,
            )
            .expect("commit safe terminal status");
        events.extend(
            server
                .committed_turn_events(&committed, Some(&assistant_events))
                .expect("terminal events"),
        );

        let methods = events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                "item/started",
                "item/agentMessage/delta",
                "item/failed",
                "turn/completed",
            ]
        );
        assert_eq!(events[2]["params"]["error"], SAFE_ASSISTANT_ITEM_FAILURE);
        assert!(
            !serde_json::to_string(&events)
                .expect("events json")
                .contains(label)
        );
        assert!(committed.assistant_item.is_none());
        assert!(
            server
                .store
                .read_thread_history(&thread.thread_id, None, 10)
                .expect("history")
                .messages
                .iter()
                .all(|message| message.role != ConversationRole::Assistant)
        );
    }
}

#[test]
fn terminal_store_failure_fails_visible_realtime_item_without_completion() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(&db_path).expect("store");
    let assistant_item_id = SessionStore::allocate_assistant_item_id();
    let collision_thread = store.create_thread(None, None).expect("collision thread");
    let collision_turn = store
        .create_turn(&collision_thread.thread_id, AgentStatus::Running.as_str())
        .expect("collision turn");
    store
        .commit_turn_outcome(
            &collision_turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: AgentStatus::Completed.as_str(),
                assistant_item_id: Some(&assistant_item_id),
                assistant_delta: Some("existing assistant"),
                trace: &TraceEvent::for_turn(
                    "trace_existing_assistant",
                    &collision_thread.thread_id,
                    &collision_turn.turn_id,
                    "test",
                    "existing assistant",
                ),
            },
        )
        .expect("existing assistant");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "complete"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    let server = app_server(store);
    server
        .event_filter
        .lock()
        .expect("event filter")
        .event_types = Some(vec![
        "item/started".to_string(),
        "item/agentMessage/delta".to_string(),
        "item/failed".to_string(),
        "item/completed".to_string(),
        "turn/completed".to_string(),
    ]);
    let status = outcome_to_run_status(AgentOutcome {
        final_text: "complete".to_string(),
        turns: 1,
        usage: ModelUsage::default(),
        compacted: false,
        aborted: false,
    });
    let mut assistant_events = AssistantItemEventState::new(assistant_item_id);
    let mut events = server
        .project_assistant_delta(&mut assistant_events, "partial")
        .expect("partial delta");

    assert!(
        server
            .commit_turn_run_status(
                turn.clone(),
                &status,
                Some(&assistant_events.item_id),
                &CancellationToken::new(),
                None,
            )
            .is_err()
    );
    let result = server.finish_turn_failure(
        &mut |event| events.push(event),
        &turn,
        Some(&assistant_events),
        &CancellationToken::new(),
        None,
        TurnFailure {
            stage: TurnFailureStage::TerminalOutcome,
            cause: TurnFailureCause::Store,
        },
    );

    assert!(matches!(result, Err(AppServerError::TurnExecution { .. })));
    assert_eq!(
        events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>(),
        vec![
            "item/started",
            "item/agentMessage/delta",
            "item/failed",
            "turn/completed",
        ]
    );
    assert_eq!(
        server.store.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Failed
    );
    assert!(
        server
            .store
            .read_thread_history(&thread.thread_id, None, 10)
            .expect("history")
            .messages
            .iter()
            .all(|message| message.role != ConversationRole::Assistant)
    );
}

#[test]
fn realtime_item_tracks_started_and_delta_filtering_independently() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let server = app_server(store);

    for (subscriptions, expected, started_generated, delta_generated) in [
        (
            vec!["item/started".to_string()],
            vec!["item/started"],
            true,
            false,
        ),
        (
            vec!["item/agentMessage/delta".to_string()],
            vec!["item/agentMessage/delta", "item/agentMessage/delta"],
            false,
            true,
        ),
        (Vec::new(), Vec::new(), false, false),
    ] {
        server
            .event_filter
            .lock()
            .expect("event filter")
            .event_types = Some(subscriptions);
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        let mut events = server
            .project_assistant_delta(&mut assistant_events, "first")
            .expect("first delta");
        events.extend(
            server
                .project_assistant_delta(&mut assistant_events, "second")
                .expect("second delta"),
        );

        assert_eq!(
            events
                .iter()
                .map(|event| event["method"].as_str().expect("event method"))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(assistant_events.started_generated, started_generated);
        assert_eq!(assistant_events.delta_generated, delta_generated);
    }
}

#[test]
fn agent_capability_is_always_available_without_a_sandbox_gate() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    let responses = server
        .agent_capability(
            JsonRpcMessage::request(Method::AgentCapability, 1, json!({})).expect("request"),
        )
        .expect("agent capability");
    let result: AgentCapabilityResult =
        serde_json::from_value(responses[0]["result"].clone()).expect("capability result");
    // 无门禁语义：恒 available，协议形状保持（CLI doctor 依赖 status==completed）。
    assert!(result.agent_loop.available);
    assert!(result.agent_loop.blockers.is_empty());
    assert_eq!(result.agent_loop.status, "completed");
}

#[test]
fn unsequenced_turn_outputs_carry_one_explicit_transport_trace_binding() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    let binding = TransportTraceBinding::for_turn("thread_trace", "turn_trace");
    server.pending_transport_trace_binding = Some(binding.clone());

    let outputs = server
        .sequence_outputs(vec![json!({"jsonrpc": "2.0", "id": 1, "result": {}})])
        .expect("sequence bound output");
    assert_eq!(outputs[0].trace_binding.as_ref(), Some(&binding));
    server.output_order.complete(outputs[0].reservation.order);

    let unbound = server
        .sequence_outputs(vec![json!({"jsonrpc": "2.0", "id": 2, "result": {}})])
        .expect("sequence unbound output");
    assert!(unbound[0].trace_binding.is_none());
    server.output_order.complete(unbound[0].reservation.order);
}

/// 构造带工具调用序列的 fake provider（write 工具 → 文本收尾）。
fn tool_using_static_provider(
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
) -> StaticProvider {
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    tool_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_1".to_string(),
        tool_name: "write".to_string(),
        arguments: json!({"path": "hello.txt", "content": "hello"}),
        raw_arguments: json!({"path": "hello.txt", "content": "hello"}).to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    StaticProvider {
        responses: vec![
            tool_response,
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done"),
        ],
        seen_requests,
    }
}

#[test]
fn turn_start_runs_new_core_with_tools_and_session_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut server = app_server(store)
        .with_test_provider(Arc::new(tool_using_static_provider(Arc::clone(&seen_requests))));
    server
        .event_filter
        .lock()
        .expect("event filter")
        .event_types = Some(vec![
        "turn/started".to_string(),
        "item/started".to_string(),
        "item/agentMessage/delta".to_string(),
        "item/completed".to_string(),
        "turn/completed".to_string(),
    ]);

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "write hello.txt"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    assert_eq!(result.turn.agent_loop_status, AgentStatus::Completed.as_str());
    // 事件流：item/started + item/agentMessage/delta + item/completed + turn/completed。
    let methods: Vec<&str> = responses
        .iter()
        .filter_map(|message| message["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        vec![
            "turn/started",
            "item/started",
            "item/agentMessage/delta",
            "item/completed",
            "turn/completed",
        ]
    );
    // 工具真实执行：write 写入 workspace。
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).expect("read hello.txt"),
        "hello"
    );
    // 两轮 provider 调用，第二轮上下文重放 assistant tool call + tool result。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.messages[1].role, ModelRole::Assistant);
    assert_eq!(second.messages[1].tool_calls.len(), 1);
    assert_eq!(second.messages[2].role, ModelRole::Tool);
    // session 文件生成：thread_id.jsonl，消息完整。
    let session_file = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join(format!("{}.jsonl", thread.thread_id));
    assert!(session_file.exists());
    let session = SessionManager::open(&session_file).expect("session");
    let entries = session.build_context_entries().expect("entries");
    let messages: Vec<&singularity_agent::message::AgentMessage> = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            singularity_agent::session::SessionEntryType::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, singularity_agent::message::AgentMessageRole::User);
    assert_eq!(messages[0].content, "write hello.txt");
    assert_eq!(
        messages[1].tool_name.as_deref(),
        Some("write"),
        "assistant tool call message persisted"
    );
    assert!(messages[2].content.contains("Successfully wrote"));
    assert_eq!(messages[3].content, "done");
    // 工具执行 trace 事件已投影。
    let traces = server
        .store
        .list_trace(&thread.thread_id)
        .expect("trace");
    assert!(traces.iter().any(|event| {
        event.payload["observation"] == "tool_execution"
            && event.payload["tool_name"] == "write"
    }));
}

#[test]
fn turn_input_queued_before_run_is_injected_into_steer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    store
        .append_turn_input(
            &turn.turn_id,
            "input-pre-run",
            TurnInputDelivery::Steer,
            &json!([{"type": "text", "text": "steer before run"}]),
        )
        .expect("queued input");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let server = app_server(store).with_test_provider(Arc::new(provider));
    let mut events = AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
    let mut emitted = Vec::new();
    let status = server
        .run_agent_core(
            &thread,
            &turn,
            "main input",
            &CancellationToken::new(),
            &mut events,
            &mut |message| emitted.push(message),
        )
        .expect("run agent core");
    assert_eq!(status.status, AgentStatus::Completed);
    // run 前已排队输入按 steer 注入：第一轮请求 user(main) 后紧跟 steer 消息。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    let texts: Vec<&str> = requests[0]
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect();
    assert_eq!(texts, &["main input", "steer before run"]);
    // 会话文件同步写入。
    assert!(
        workspace
            .join(".singularity")
            .join("agent-sessions")
            .join(format!("{}.jsonl", thread.thread_id))
            .exists()
    );
}

#[test]
fn turn_input_during_run_pushes_the_registered_steer_handle() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    let thread = server
        .store
        .create_thread(None, None)
        .expect("thread");
    let turn = server
        .store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let handle = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    server
        .steer_handles
        .lock()
        .expect("steer handles")
        .insert(turn.turn_id.clone(), handle.clone());

    let responses = server
        .turn_input(
            JsonRpcMessage::request(
                Method::TurnInput,
                1,
                json!({
                    "turnId": turn.turn_id,
                    "inputId": "input-live",
                    "delivery": "steer",
                    "input": [{"type": "text", "text": "live steer"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn input");
    assert_eq!(responses.len(), 1);
    // 运行中注入：共享 steer 队列收到消息（run 下一轮 drain）。
    let queued = handle.lock().expect("handle");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0], "live steer");
    // store 侧同时记录了 pending input（idempotent key 复用返回同一 turn）。
    let boundary = server
        .store
        .turn_boundary_state(&turn.turn_id, true)
        .expect("boundary");
    assert_eq!(boundary.inputs.len(), 1);
}

#[test]
fn turn_resume_reopens_session_and_runs_new_core() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Paused.as_str(),
            json!([{"type": "text", "text": "resume me"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Paused,
            AgentStatus::Paused.as_str(),
        )
        .expect("paused turn");
    // 模拟旧链 pause 数据：Paused turn 携带 checkpoint（store 恢复逻辑对
    // 无 checkpoint 的 Paused turn 会终态化；新链路不再产生这种状态）。
    store
        .save_turn_checkpoint(
            &turn.turn_id,
            &thread.thread_id,
            &json!({ "checkpoint_version": 1, "legacy": true }),
            1,
        )
        .expect("legacy checkpoint");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "resumed answer",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    let responses = server
        .turn_resume(
            JsonRpcMessage::request(Method::TurnResume, 1, json!({ "turnId": turn.turn_id }))
                .expect("request"),
        )
        .expect("turn resume");
    let result: TurnResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    // 恢复输入来自 turn 行；会话文件被创建并写入。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content == "resume me")
    );
    let session_file = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join(format!("{}.jsonl", thread.thread_id));
    assert!(session_file.exists());
    let session = SessionManager::open(&session_file).expect("session");
    let texts: Vec<String> = session
        .build_context_entries()
        .expect("entries")
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            singularity_agent::session::SessionEntryType::Message(message) => {
                Some(message.content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["resume me", "resumed answer"]);
}
