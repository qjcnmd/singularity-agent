use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use singularity_agent::{AgentRecoveryMetrics, PendingToolCall};
use singularity_model::{
    ModelError, ModelErrorCategory, ModelErrorKind, ModelMessage, ModelRole, ModelToolCall,
    ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, ModelUsage,
    Provider, ProviderAttemptMetadata, ProviderError, ProviderProtocolContract,
    ProviderStreamEvent,
};
use singularity_policy::{ToolId, WorkspaceRelativePath};
use singularity_protocol::ItemKind;
use singularity_sandbox::{CommandScriptRequest, WorkspaceChangeSummary, WorkspaceMutation};
use singularity_tools::{CommandRequest, CommandResult};

use super::*;

fn tool_id(value: &str) -> ToolId {
    ToolId::new(value).expect("valid tool id")
}

fn workspace_resource(value: &str) -> PermissionResource {
    PermissionResource::WorkspacePath(
        WorkspaceRelativePath::from_canonical(value).expect("canonical workspace path"),
    )
}

fn app_server(store: SessionStore) -> AppServer {
    AppServer::new(store, ProviderConfigSnapshot::capture(|_| None))
}

fn pending_approval_for_test(
    request: &ApprovalRequest,
    arguments: Value,
) -> PendingApprovalOccurrence {
    let tool_call_id = request.tool_call_id.clone().expect("tool call id");
    let raw_arguments = arguments.to_string();
    let payload = json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": &tool_call_id,
        "tool_name": &request.action,
        "raw_arguments": &raw_arguments,
        "resources": &request.resources,
        "checkpoint_version": 3,
        "project_instructions_digest": null,
        "messages": [{
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "tool_call_id": &tool_call_id,
                "tool_name": &request.action,
                "arguments": arguments,
                "raw_arguments": &raw_arguments,
                "parse_status": "valid",
                "validation_errors": []
            }]
        }],
        "tool_result_occurrences": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {
            "workspace_mutated": false,
            "workspace_revision": null,
            "successful_command_count": 0,
            "required_command_counts": {},
            "terminal_command_scope_digests": [],
            "terminal_command_revisions": [],
            "unresolved_failures": []
        },
        "repair_attempts": 0,
        "last_completion_error": null,
        "plan": null,
        "plan_update_count": 0,
        "recovery_metrics": AgentRecoveryMetrics::default(),
        "model_usage": ModelUsage::default(),
        "provider_attempts": ProviderAttemptMetadata::default(),
        "context_trace": null,
        "seen_tool_call_fingerprints": [],
        "last_repair_failure": null
    });
    decode_pending_approval(request, Some(&payload))
        .expect("pending approval")
        .expect("pending occurrence")
}

#[test]
fn app_server_checkpoint_codec_rejects_legacy_resources() {
    let request = ApprovalRequest::new(
        "approval_legacy_resource",
        "thread_legacy_resource",
        "turn_legacy_resource",
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    let pending = pending_approval_for_test(&request, json!({}));
    let mut legacy = pending.encode_checkpoint().expect("checkpoint");
    legacy["checkpoint_version"] = json!(1);
    legacy["resources"] = json!(["README.md"]);
    legacy["tool_results"] = legacy["tool_result_occurrences"].clone();
    legacy
        .as_object_mut()
        .expect("checkpoint object")
        .remove("tool_result_occurrences");
    legacy["tool_result_context_bindings"] = json!([]);

    assert_eq!(
        decode_pending_approval(&request, Some(&legacy))
            .expect_err("legacy checkpoint must fail closed")
            .to_string(),
        "store error: invalid store state: approval request requires an internal AgentLoop checkpoint: unsupported approval checkpoint version"
    );
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
fn ordinary_and_evaluation_traces_share_safe_audit_projection_and_store_roundtrip() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let mut status = AgentRunStatus::failed("safe failure");
    status.audit_events.push(project_audit_event(&json!({
            "cwd": "C:/sensitive/workspace",
            "raw_arguments": {"command": "echo secret"},
            "approval_reason": "operator reason",
            "approval_request_id": "approval-secret",
            "approval_grant_id": "grant-secret",
            "sandbox_mode": "workspace_write",
            "network_access": "allowed",
            "sandbox_backend": "test_backend",
            "sandbox_enforcement": "strict",
            "local_process_fallback": false,
            "command_scope_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "command_provenance": "agent_requested",
            "approval_policy": "on-request",
            "approval_decision": "approved",
            "timeout_seconds": 5,
        })));

    let ordinary = agent_loop_trace(&turn, &status);
    let evaluation = json!({"audit_events": &status.audit_events});
    for serialized in [
        serde_json::to_string(&ordinary).expect("ordinary trace JSON"),
        serde_json::to_string(&evaluation).expect("evaluation trace JSON"),
    ] {
        for forbidden in [
            "C:/sensitive/workspace",
            "raw_arguments",
            "operator reason",
            "approval-secret",
            "grant-secret",
        ] {
            assert!(!serialized.contains(forbidden), "leaked {forbidden}");
        }
    }

    store.append_trace(&ordinary).expect("append trace");
    let restored = store
        .show_trace(&ordinary.event_id)
        .expect("roundtrip trace");
    assert!(restored.redaction_applied);
    assert_eq!(
        restored.payload["audit_events"][0]["sandbox_mode"],
        "workspace_write"
    );
    let restored_json = serde_json::to_string(&restored).expect("restored trace JSON");
    assert!(!restored_json.contains("raw_arguments"));
    assert!(!restored_json.contains("approval-secret"));
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
    let failure_status = AgentRunStatus::failed("late monitor failure");
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
            &AgentRunStatus::failed("late user result"),
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
    let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);
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
                &AgentRunStatus::failed("stale run failure"),
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
                AgentRunStatus::failed("invalid completion").with_status(AgentStatus::Completed);
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
fn approval_failure_terminalizes_failed_without_raw_cause() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "approval"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_terminalize_failure",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1");
    let checkpoint = json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": TOOL_EDIT,
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint.clone()),
            "approval",
            "approval requested",
        )
        .expect("pending approval");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("claim approval execution");
    let server = app_server(store);

    let terminal = server
        .terminalize_claimed_approval_error(
            &request,
            &decision,
            None,
            ApprovalTerminalizationContext {
                turn: &turn,
                thread: &thread,
                prior_status: None,
                cancellation: &CancellationToken::new(),
                monitor_outcome: None,
                failure: TurnFailureStage::ApprovalCheckpoint.into(),
            },
        )
        .expect("terminalize approval failure");
    let committed = match terminal {
        TurnTerminalizationResult::Committed(committed) => committed,
        TurnTerminalizationResult::Preserved => panic!("approval must be terminalized"),
    };
    assert_eq!(committed.turn.status, TurnStatus::Failed);
    assert_eq!(
        committed.turn.agent_loop_status,
        AgentStatus::Failed.as_str()
    );
    assert!(
        !server
            .store
            .has_pending_tool_call(&request.request_id)
            .expect("resolved approval")
    );
    let trace_json = serde_json::to_string(&committed.trace).expect("trace json");
    assert!(!trace_json.contains("sqlite"));
    assert!(!trace_json.contains("raw_arguments"));
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
fn workspace_tool_binding_failure_is_a_typed_app_server_error() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace_sentinel = "workspace-path-sentinel";
    let missing = dir.path().join(workspace_sentinel);

    assert!(matches!(
        workspace_tools(missing, Arc::new(CompletedSandboxBackend)),
        Err(AppServerError::Workspace(message))
            if message == SAFE_WORKSPACE_FAILURE && !message.contains(workspace_sentinel)
    ));
}

#[test]
fn workspace_binding_failure_precedes_running_turn_persistence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace-path-sentinel");
    std::fs::create_dir(&workspace).expect("create workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    std::fs::remove_dir(&workspace).expect("remove workspace before turn");
    let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

    let response = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "must not persist"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn response");

    assert!(
        response[0]["error"]["message"]
            .as_str()
            .expect("error message")
            == SAFE_WORKSPACE_FAILURE
    );
    assert!(!response[0].to_string().contains("workspace-path-sentinel"));
    let history = server
        .store
        .read_thread_history(&thread.thread_id, None, 8)
        .expect("thread history");
    assert!(history.messages.is_empty());
}

#[cfg(windows)]
#[test]
fn persisted_workspace_replacement_with_junction_is_not_rebound() {
    use std::os::windows::process::CommandExt as _;

    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    let retained = dir.path().join("retained-workspace");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&workspace).expect("create workspace");
    std::fs::create_dir(&outside).expect("create outside");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    std::fs::rename(&workspace, &retained).expect("replace workspace namespace");
    let link_arg = format!("\"{}\"", workspace.display());
    let target_arg = format!("\"{}\"", outside.display());
    let output = std::process::Command::new("cmd.exe")
        .raw_arg("/d /c ")
        .raw_arg("mklink")
        .raw_arg("/J")
        .raw_arg(&link_arg)
        .raw_arg(&target_arg)
        .output()
        .expect("create junction process");
    if !output.status.success() {
        return;
    }

    let error = workspace_tools_for_thread(&thread, Arc::new(CompletedSandboxBackend))
        .expect_err("replacement junction must fail closed");
    assert_eq!(error, SAFE_WORKSPACE_FAILURE);
    std::fs::remove_dir(&workspace).expect("remove junction");
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
    let mut server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);
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

#[test]
fn stopped_execution_does_not_consume_a_pending_tool_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
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
    let request = ApprovalRequest::new(
        "approval_stopped_execution",
        thread.thread_id,
        turn.turn_id,
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1");
    let checkpoint = json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": TOOL_EDIT,
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("pending approval");
    let mut server = app_server(store);
    server
        .request_execution_stop()
        .expect("request execution stop");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let message = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "approval/decision",
        "id": 1,
        "params": decision,
    }))
    .expect("approval decision message");

    let response = server
        .approval_decision(message)
        .expect("decision response");

    assert_eq!(response[0]["error"]["message"], EXECUTION_STOPPED);
    assert_eq!(
        server
            .store
            .get_pending_approval(&request.request_id)
            .expect("approval remains pending"),
        request
    );
    assert!(
        server
            .store
            .has_pending_tool_call(&request.request_id)
            .expect("checkpoint remains pending")
    );
}

struct StaticProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

struct StreamingProvider {
    responses: Vec<(Vec<ProviderStreamEvent>, ModelTurnResponse)>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

fn typed_final_review_fixture(
    request: &ModelTurnRequest,
    mut response: ModelTurnResponse,
) -> ModelTurnResponse {
    if !request.tools.is_empty() {
        return response;
    }
    let Some(answer) = response
        .assistant_message
        .as_ref()
        .map(|message| message.content.clone())
        .filter(|content| !content.trim().is_empty())
    else {
        return response;
    };
    if serde_json::from_str::<Value>(&answer).is_ok() {
        return response;
    }
    let Some(template) = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == ModelRole::Developer)
        .and_then(|message| message.content.split_once("with no markdown: "))
        .and_then(|(_, value)| value.split_once(". The revision"))
        .map(|(value, _)| value)
    else {
        return response;
    };
    let Ok(mut value) =
        serde_json::from_str::<Value>(&template.replace("accept|reject|repair", "accept"))
    else {
        return response;
    };
    value["final_answer"] = json!(answer);
    value["reason"] = json!("");
    response.assistant_message = Some(ModelMessage::text(ModelRole::Assistant, value.to_string()));
    response
}

impl Provider for StreamingProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        let response_index = seen_requests.len();
        seen_requests.push(request.clone());
        let (events, response) = self
            .responses
            .get(response_index)
            .unwrap_or_else(|| self.responses.last().expect("streaming response"));
        let original_text = response
            .assistant_message
            .as_ref()
            .map(|message| message.content.clone());
        let mut response = response.clone();
        response.request_id = request.request_id.clone();
        let response = typed_final_review_fixture(request, response);
        let terminal_text = response
            .assistant_message
            .as_ref()
            .map(|message| message.content.as_str());
        if terminal_text != original_text.as_deref() {
            let chars = terminal_text
                .unwrap_or_default()
                .chars()
                .collect::<Vec<_>>();
            let chunks = events.len().max(1);
            for index in 0..chunks {
                let start = chars.len() * index / chunks;
                let end = chars.len() * (index + 1) / chunks;
                on_event(ProviderStreamEvent::OutputTextDelta {
                    delta: chars[start..end].iter().collect(),
                });
            }
        } else {
            for event in events {
                on_event(event.clone());
            }
        }
        Ok(response)
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        panic!("streaming provider must use complete_stream")
    }
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
        Ok(typed_final_review_fixture(request, response))
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
fn app_server_preserves_typed_provider_failure_category() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "user goal".to_string(),
        }],
    };
    let provider_sentinel = "provider-body-sentinel";
    let mut provider_error =
        ModelError::new(ModelErrorKind::AuthError, provider_sentinel.to_string());
    provider_error.validation_errors = vec![provider_sentinel.to_string()];
    let provider = StaticProvider {
        responses: vec![failed_model_response(provider_error)],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };

    let server = app_server(store);
    let status = server
        .run_agent_loop_with_provider(
            provider,
            &thread,
            &params,
            &turn.turn_id,
            &[],
            &CancellationToken::new(),
        )
        .expect("agent loop");

    assert_eq!(status.status, AgentStatus::Failed);
    assert_eq!(
        status.error_category,
        Some(ModelErrorCategory::Authentication)
    );
    let status_json = serde_json::to_string(&status).expect("serialize status");
    assert_eq!(status.error.as_deref(), Some(SAFE_AGENT_LOOP_FAILURE));
    assert!(!status_json.contains(provider_sentinel));
    assert!(!status_json.contains("validation_errors"));
    let committed = server
        .commit_turn_run_status(turn, &status, None, &CancellationToken::new(), None)
        .expect("commit provider failure");
    let trace_json = serde_json::to_string(&committed.trace).expect("trace json");
    assert!(!trace_json.contains(provider_sentinel));
    assert_eq!(committed.trace.payload["error"], SAFE_AGENT_LOOP_FAILURE);
    assert!(
        committed.trace.payload["provider_diagnostic"]
            .get("validation_errors")
            .is_none()
    );
}

#[test]
fn agent_loop_loads_bounded_agents_md_from_thread_cwd() {
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
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "user goal".to_string(),
        }],
    };
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let server = app_server(store);

    let status = server
        .run_agent_loop_with_provider(
            provider,
            &thread,
            &params,
            "turn_1",
            &[],
            &CancellationToken::new(),
        )
        .expect("agent loop");

    assert_eq!(status.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages.len(), 2);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    assert_eq!(requests[0].messages[1].role, ModelRole::User);
    let developer = &requests[0].messages[0].content;
    assert!(developer.starts_with("You are a coding agent working in the current workspace."));
    assert!(developer.ends_with("Project instructions:\nworkspace override"));
    assert!(!developer.contains("ancestor instructions"));
    assert_eq!(requests[0].messages[1].content, "user goal");
    let hidden_workspace_marker = workspace.to_string_lossy();
    assert!(!requests[0].tools.iter().any(|tool| {
        serde_json::to_string(tool)
            .expect("serialize tool")
            .contains(hidden_workspace_marker.as_ref())
    }));
    assert!(
        !serde_json::to_string(&status)
            .expect("serialize status")
            .contains(hidden_workspace_marker.as_ref())
    );
}

#[test]
fn agent_loop_replays_only_completed_store_history_in_order() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    std::fs::write(workspace.join("AGENTS.md"), "project instructions")
        .expect("agents instructions");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");

    let (prior, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "previous user"}]),
            "app_server",
            "prior turn",
        )
        .expect("prior turn");
    store
        .append_item(
            &prior.turn_id,
            ItemKind::AgentMessage,
            json!({"delta": "previous assistant"}),
        )
        .expect("prior assistant");
    store
        .append_item(
            &prior.turn_id,
            ItemKind::Reasoning,
            json!({"summary": "private tool metadata must not replay"}),
        )
        .expect("private prior item");
    store
        .update_turn_state(
            &prior.turn_id,
            TurnStatus::Completed,
            AgentStatus::Completed.as_str(),
        )
        .expect("complete prior turn");

    let (failed, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "failed user must not replay"}]),
            "app_server",
            "failed turn",
        )
        .expect("failed turn");
    store
        .append_item(
            &failed.turn_id,
            ItemKind::AgentMessage,
            json!({"delta": "failed assistant must not replay"}),
        )
        .expect("failed assistant");
    store
        .update_turn_state(
            &failed.turn_id,
            TurnStatus::Failed,
            AgentStatus::Failed.as_str(),
        )
        .expect("fail turn");

    let (blocked, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "blocked user must not replay"}]),
            "app_server",
            "blocked turn",
        )
        .expect("blocked turn");
    store
        .update_turn_state(
            &blocked.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("block turn");
    store
        .update_turn_state(
            &blocked.turn_id,
            TurnStatus::Interrupted,
            AgentStatus::Cancelled.as_str(),
        )
        .expect("release blocked fixture");

    let started = store
        .create_turn_with_input_trace_and_history(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "current user"}]),
            "app_server",
            "current turn",
            DEFAULT_THREAD_HISTORY_TURN_LIMIT,
        )
        .expect("current turn");
    assert_eq!(
        started
            .history
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["previous user", "previous assistant"]
    );

    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "current user".to_string(),
        }],
    };
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let server = app_server(store);

    let status = server
        .run_agent_loop_with_provider(
            provider,
            &thread,
            &params,
            &started.turn.turn_id,
            &started.history.messages,
            &CancellationToken::new(),
        )
        .expect("agent loop");

    assert_eq!(status.status, AgentStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .map(|message| message.role.clone())
            .collect::<Vec<_>>(),
        vec![
            ModelRole::Developer,
            ModelRole::User,
            ModelRole::Assistant,
            ModelRole::User,
        ]
    );
    let developer = &requests[0].messages[0].content;
    assert!(developer.starts_with("You are a coding agent working in the current workspace."));
    assert!(developer.ends_with("Project instructions:\nproject instructions"));
    assert_eq!(
        requests[0].messages[1..]
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["previous user", "previous assistant", "current user",]
    );
    let request_json = serde_json::to_string(&requests[0]).expect("request json");
    for forbidden in [
        "private tool metadata must not replay",
        "failed user must not replay",
        "failed assistant must not replay",
        "blocked user must not replay",
    ] {
        assert!(!request_json.contains(forbidden), "leaked {forbidden}");
    }
}

#[test]
fn sandbox_command_schema_does_not_expose_permission_expansion_fields() {
    let command = workspace_tool_entries()
        .into_iter()
        .find(|entry| entry.spec.name == TOOL_COMMAND)
        .expect("command tool entry");
    let properties = command
        .spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("command properties");

    assert!(!properties.contains_key("sandbox_mode"));
    assert!(!properties.contains_key("network_access"));
}

#[test]
fn app_server_registers_the_agent_control_tools() {
    let registry = workspace_tool_registry();
    let plan = registry
        .get(singularity_agent::UPDATE_PLAN_TOOL)
        .expect("plan tool registered");
    assert_eq!(plan.input_schema["properties"]["steps"]["maxItems"], 64);
    assert_eq!(plan.input_schema["additionalProperties"], false);
}

#[test]
fn committed_plan_terminal_path_emits_independent_item_events() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "inspect the workspace"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "inspect the workspace".to_string(),
        }],
    };
    let plan_arguments = json!({
        "steps": [{"step": "inspect the workspace", "status": "completed"}]
    });
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    plan_response.tool_calls.push(ModelToolCall {
        tool_call_id: "plan_call_1".to_string(),
        tool_name: "update_plan".to_string(),
        raw_arguments: plan_arguments.to_string(),
        arguments: plan_arguments,
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let server = app_server(store);
    let status = server
        .run_agent_loop_with_provider(
            StaticProvider {
                responses: vec![plan_response, final_response],
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            },
            &thread,
            &params,
            &turn.turn_id,
            &[],
            &CancellationToken::new(),
        )
        .expect("agent loop");
    assert_eq!(status.status, AgentStatus::Completed);
    assert!(status.plan.is_some());
    server
        .event_filter
        .lock()
        .expect("event filter")
        .event_types = Some(vec![
        "item/started".to_string(),
        "item/completed".to_string(),
        "turn/plan/updated".to_string(),
        "item/agentMessage/delta".to_string(),
        "turn/completed".to_string(),
    ]);

    let assistant_events = AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
    let committed = server
        .commit_turn_run_status(
            turn,
            &status,
            Some(&assistant_events.item_id),
            &CancellationToken::new(),
            None,
        )
        .expect("commit terminal outcome");
    let plan_item = committed.plan_item.as_ref().expect("plan item");
    assert_eq!(plan_item.kind, ItemKind::Plan);
    let events = server
        .committed_turn_events(&committed, Some(&assistant_events))
        .expect("terminal events");
    let methods = events
        .iter()
        .map(|event| event["method"].as_str().expect("event method"))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        vec![
            "item/started",
            "item/completed",
            "turn/plan/updated",
            "item/started",
            "item/agentMessage/delta",
            "item/completed",
            "turn/completed",
        ]
    );
    assert_eq!(events[0]["params"]["item"]["item_id"], plan_item.item_id);
    assert_eq!(events[1]["params"]["item"]["item_id"], plan_item.item_id);
    assert_eq!(events[2]["params"]["plan"], plan_item.payload);
    assert!(events[0]["params"].get("delta").is_none());
    assert!(events[1]["params"].get("delta").is_none());
}

#[test]
fn responses_finalization_deltas_share_item_id_with_terminal_store_item() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "say hello"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "say hello".to_string(),
        }],
    };
    let server = app_server(store).with_sandbox_backend(MutatingCommandSandboxBackend {
        calls: AtomicUsize::new(0),
    });
    server
        .event_filter
        .lock()
        .expect("event filter")
        .event_types = Some(vec![
        "item/started".to_string(),
        "item/agentMessage/delta".to_string(),
        "item/completed".to_string(),
        "turn/completed".to_string(),
    ]);
    let mut assistant_events =
        AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
    let mut events = Vec::new();
    let mut mutation_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "mutating");
    mutation_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_mutate".to_string(),
        tool_name: "command".to_string(),
        arguments: json!({
            "command": "test-program mutate",
            "timeout_seconds": 5
        }),
        raw_arguments: json!({
            "command": "test-program mutate",
            "timeout_seconds": 5
        })
        .to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let mut verification_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "verifying");
    verification_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_verify".to_string(),
        tool_name: "command".to_string(),
        arguments: json!({
            "command": "test-program verify",
            "timeout_seconds": 5
        }),
        raw_arguments: json!({
            "command": "test-program verify",
            "timeout_seconds": 5
        })
        .to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let plan_arguments = json!({
        "steps": [{"step": "verify the workspace mutation", "status": "completed"}],
        "verification": [{
            "risk": "general_mutation",
            "evidence": ". changed by the workspace command",
            "affected_symbol": ".::workspace",
            "current_gap": "the changed workspace has not been verified",
            "action": {
                "command": "test-program verify",
                "cwd": ".",
                "timeout_seconds": 5,
                "sandbox_mode": "workspace_write",
                "network_access": "denied"
            },
            "required": 1
        }]
    });
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "planning");
    plan_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_plan".to_string(),
        tool_name: "update_plan".to_string(),
        arguments: plan_arguments.clone(),
        raw_arguments: plan_arguments.to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let status = server
        .run_agent_loop_with_provider_and_text_deltas(
            StreamingProvider {
                responses: vec![
                    (
                        vec![ProviderStreamEvent::OutputTextDelta {
                            delta: "mutating".to_string(),
                        }],
                        mutation_response,
                    ),
                    (
                        vec![ProviderStreamEvent::OutputTextDelta {
                            delta: "planning".to_string(),
                        }],
                        plan_response,
                    ),
                    (
                        vec![ProviderStreamEvent::OutputTextDelta {
                            delta: "verifying".to_string(),
                        }],
                        verification_response,
                    ),
                    (
                        vec![
                            ProviderStreamEvent::OutputTextDelta {
                                delta: "do".to_string(),
                            },
                            ProviderStreamEvent::OutputTextDelta {
                                delta: "ne".to_string(),
                            },
                        ],
                        ModelTurnResponse::completed(
                            "model_request_turn_1_3",
                            "response_4",
                            "done",
                        ),
                    ),
                ],
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            },
            &thread,
            &params,
            &turn.turn_id,
            &[],
            &CancellationToken::new(),
            &mut |delta| {
                events.extend(
                    server
                        .project_assistant_delta(&mut assistant_events, delta)
                        .expect("project delta"),
                );
            },
        )
        .expect("agent loop");
    assert_eq!(status.status, AgentStatus::Completed);
    assert_eq!(status.final_answer.as_deref(), Some("done"));
    assert!(status.verification.required);
    assert!(status.verification.passed);

    let committed = server
        .commit_turn_run_status(
            turn,
            &status,
            Some(&assistant_events.item_id),
            &CancellationToken::new(),
            None,
        )
        .expect("commit terminal outcome");
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
            "item/agentMessage/delta",
            "item/started",
            "item/completed",
            "item/completed",
            "turn/completed",
        ]
    );
    assert_eq!(events[1]["params"]["delta"], "do");
    assert_eq!(events[2]["params"]["delta"], "ne");
    let events_json = serde_json::to_string(&events).expect("events json");
    assert!(!events_json.contains("mutating"));
    assert!(!events_json.contains("planning"));
    assert!(!events_json.contains("verifying"));
    let item_id = assistant_events.item_id.as_str();
    assert!(
        events[..3]
            .iter()
            .all(|event| event["params"]["item"]["item_id"] == item_id)
    );
    assert_eq!(events[5]["params"]["item"]["item_id"], item_id);
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .map(|item| item.item_id.as_str()),
        Some(item_id)
    );
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .and_then(|item| item.payload["delta"].as_str()),
        Some("done")
    );
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
        let mut status = AgentRunStatus::failed(label);
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
                plan: None,
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
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "complete".to_string(),
        }],
    };
    let status = server
        .run_agent_loop_with_provider(
            StaticProvider {
                responses: vec![ModelTurnResponse::completed(
                    "model_request_turn_1_0",
                    "response_1",
                    "complete",
                )],
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            },
            &thread,
            &params,
            &turn.turn_id,
            &[],
            &CancellationToken::new(),
        )
        .expect("agent loop");
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

#[cfg(windows)]
#[test]
fn agent_loop_approval_resume_without_pending_tool_call_fails_closed_after_gate() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let file_path = workspace.join("README.md");
    std::fs::write(&file_path, "before").expect("write readme");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Blocked.as_str(),
            json!([{"type": "text", "text": "edit readme"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let server = app_server(store);
    let request = ApprovalRequest::new(
        format!("approval_{}_call_1", turn.turn_id),
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "done");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![final_response],
        seen_requests: Arc::clone(&seen_requests),
    };

    let resumed = server
        .resume_agent_loop_after_gate(
            &request,
            &decision,
            None,
            provider,
            &CancellationToken::new(),
            None,
        )
        .expect("resume");

    assert!(resumed.is_none());
    assert_eq!(
        std::fs::read_to_string(&file_path).expect("read readme"),
        "before"
    );
    assert!(seen_requests.lock().expect("seen requests").is_empty());
}

#[test]
fn approval_resume_uses_stored_policy_snapshot_instead_of_defaults() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let file_path = workspace.join("README.md");
    std::fs::write(&file_path, "before").expect("write readme");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread_with_policy(
            Some("gpt-test"),
            Some(&workspace.to_string_lossy()),
            PermissionProfileName::WorkspaceWrite,
            ApprovalPolicy::Never,
        )
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Blocked.as_str(),
            json!([{"type": "text", "text": "edit readme"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");

    let request = ApprovalRequest::new(
        format!("approval_{}_call_1", turn.turn_id),
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1")
    .with_resources([workspace_resource("README.md")]);
    let arguments = json!({
        "path": "README.md",
        "expected": "before",
        "replacement": "after"
    });
    let pending_payload = pending_approval_for_test(&request, arguments.clone())
        .encode_checkpoint()
        .expect("current checkpoint");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let resumed = app_server(store)
        .resume_agent_loop_after_gate(
            &request,
            &decision,
            Some(pending_payload),
            StaticProvider {
                responses: vec![ModelTurnResponse::completed(
                    "model_request_turn_1_0",
                    "response_1",
                    "done",
                )],
                seen_requests: Arc::clone(&seen_requests),
            },
            &CancellationToken::new(),
            Some(
                WorkspaceTools::new(&workspace)
                    .expect("bind workspace tools")
                    .with_sandbox_backend(CompletedSandboxBackend),
            ),
        )
        .expect("resume")
        .expect("terminal status");

    assert_eq!(resumed.1.status, AgentStatus::Failed);
    assert_eq!(
        std::fs::read_to_string(&file_path).expect("read readme"),
        "before"
    );
    assert!(
        seen_requests.lock().expect("seen requests").len() <= 1,
        "the denied continuation must not execute or continue through another model turn"
    );
}

#[test]
fn agent_loop_approval_no_resume_status_records_session_and_command_audit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Blocked.as_str(),
            json!([{"type": "text", "text": "run command"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        format!("approval_{}_call_1", turn.turn_id),
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_COMMAND),
    )
    .with_tool_call_id("call_1");
    let pending_approval = pending_approval_for_test(
        &request,
        json!({
            "command": "test-program success",
            "timeout_seconds": 5
        }),
    );
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let server = app_server(store);

    let (_turn, run_status) = server
        .approval_no_resume_status(&request, &decision, &turn, &thread, Some(&pending_approval))
        .expect("status")
        .expect("terminal status");

    assert_eq!(
        run_status.audit_events[0]["sandbox_mode"],
        "workspace_write"
    );
    assert_eq!(run_status.audit_events[0]["network_access"], "denied");
    assert_eq!(
        run_status.audit_events[0]["sandbox_backend"],
        "not_executed"
    );
    assert_eq!(
        run_status.audit_events[0]["sandbox_enforcement"],
        "not_executed"
    );
    assert_eq!(
        run_status.audit_events[0]["command_scope_digest"],
        "unavailable"
    );
    assert_eq!(
        run_status.audit_events[0]["policy_scope_binding"],
        "unavailable"
    );
    let serialized = serde_json::to_string(&run_status.audit_events[0]).expect("audit JSON");
    assert!(!serialized.contains("raw_arguments"));
    assert!(!serialized.contains("test-program success"));
    assert!(!serialized.contains("approval_request_id"));
    assert!(!serialized.contains("approval_decision_id"));
    assert_eq!(
        run_status.audit_events[0]["approval_decision"],
        "unavailable"
    );
}

#[test]
fn approval_resolution_cancellation_wins_without_persisting_a_next_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Blocked.as_str(),
            json!([{"type": "text", "text": "edit"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let checkpoint = |request: &ApprovalRequest, tool_call_id: &str| {
        json!({
            "request_id": &request.request_id,
            "thread_id": &request.thread_id,
            "turn_id": &request.turn_id,
            "tool_call_id": tool_call_id,
            "tool_name": &request.action,
            "raw_arguments": "{}",
            "resources": &request.resources,
            "checkpoint_version": 1,
            "messages": [],
            "tool_results": [],
            "used_approval_grants": [],
            "approval_count": 1,
            "model_turns": 1,
            "completion": {}
        })
    };
    let request = ApprovalRequest::new(
        "approval_cancel_race",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1");
    let pending_payload = checkpoint(&request, "call_1");
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(pending_payload.clone()),
            "approval",
            "approval requested",
        )
        .expect("approval");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("claim execution");
    let cancellation_trace = TraceEvent {
        payload: json!({"turn_id": &turn.turn_id, "agent_loop_status": "cancel_requested"}),
        ..TraceEvent::for_turn(
            "trace_cancel_race",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "app_server",
            "turn interrupt requested",
        )
    };
    store
        .request_turn_cancellation(&turn.turn_id, &cancellation_trace)
        .expect("request cancellation");
    let pending_approval = pending_approval_for_test(&request, json!({}));
    let server = app_server(store);
    let current_turn = server
        .store
        .get_turn(&turn.turn_id)
        .expect("cancelled turn");
    let (_turn, no_resume_status) = server
        .approval_no_resume_status(
            &request,
            &decision,
            &current_turn,
            &thread,
            Some(&pending_approval),
        )
        .expect("no-resume status")
        .expect("terminal status");
    assert_eq!(no_resume_status.status, AgentStatus::Cancelled);

    let next = ApprovalRequest::new(
        "approval_must_not_persist",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_2");
    let stale_status = AgentRunStatus::failed("stale local result");
    let committed = server
        .commit_effective_turn_status_resolving_approval(
            &request.request_id,
            &turn,
            &stale_status,
            &[],
            None,
            None,
        )
        .expect("cancellation wins approval resolution");
    assert_eq!(committed.turn.status, TurnStatus::Interrupted);
    assert_eq!(committed.turn.agent_loop_status, "cancelled");
    assert!(
        !server
            .store
            .has_pending_tool_call(&request.request_id)
            .expect("old execution")
    );
    assert!(matches!(
        server.store.get_pending_approval(&next.request_id),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn initial_approval_handoff_interruption_is_an_idempotent_terminal_commit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "edit"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    let request = ApprovalRequest::new(
        "approval_initial_interrupt",
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_EDIT),
    )
    .with_tool_call_id("call_1");
    let checkpoint = json!({
        "request_id": &request.request_id,
        "thread_id": &request.thread_id,
        "turn_id": &request.turn_id,
        "tool_call_id": "call_1",
        "tool_name": TOOL_EDIT,
        "raw_arguments": "{}",
        "resources": [],
        "checkpoint_version": 1,
        "messages": [],
        "tool_results": [],
        "used_approval_grants": [],
        "approval_count": 1,
        "model_turns": 1,
        "completion": {}
    });
    store
        .create_approval_with_pending_tool_call_and_trace(
            &request,
            Some(checkpoint),
            "approval",
            "approval requested",
        )
        .expect("persist initial approval");
    let interrupt_trace = TraceEvent {
        payload: json!({"turn_id": &turn.turn_id, "agent_loop_status": "cancel_requested"}),
        ..TraceEvent::for_turn(
            "trace_interrupt_initial_handoff",
            thread.thread_id.clone(),
            turn.turn_id.clone(),
            "app_server",
            "turn interrupt requested",
        )
    };
    let interrupted = store
        .request_turn_cancellation(&turn.turn_id, &interrupt_trace)
        .expect("interrupt pending approval");
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    let server = app_server(store);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let stale_blocked =
        AgentRunStatus::failed("stale blocked result").with_status(AgentStatus::Blocked);
    let committed = server
        .commit_turn_run_status(turn.clone(), &stale_blocked, None, &cancellation, None)
        .expect("interrupted handoff is idempotent");
    assert_eq!(committed.turn.status, TurnStatus::Interrupted);
    assert_eq!(committed.turn.agent_loop_status, "cancelled");
    assert!(
        server
            .store
            .list_pending_approvals()
            .expect("pending")
            .is_empty()
    );
    server
        .store
        .recover_unowned_workspace_executions()
        .expect("recovery");
}

#[test]
fn agent_loop_approval_resume_rejects_untyped_checkpoint_payloads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Blocked.as_str(),
            json!([{"type": "text", "text": "run command"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let request = ApprovalRequest::new(
        format!("approval_{}_call_1", turn.turn_id),
        thread.thread_id.clone(),
        turn.turn_id.clone(),
        tool_id(TOOL_COMMAND),
    )
    .with_tool_call_id("call_1");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let mismatched_pending = PendingToolCall {
        request_id: "approval_other_call_1".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: tool_id(TOOL_COMMAND),
        raw_arguments: json!({
            "command": "test-program success",
            "timeout_seconds": 5
        })
        .to_string(),
        resources: Vec::new(),
    };
    let invalid_args_pending = PendingToolCall {
        request_id: request.request_id.clone(),
        tool_call_id: "call_1".to_string(),
        tool_name: tool_id(TOOL_COMMAND),
        raw_arguments: "{not-json".to_string(),
        resources: Vec::new(),
    };
    let server = app_server(store);

    let mismatch_error = server
        .resume_agent_loop_after_gate(
            &request,
            &decision,
            Some(serde_json::to_value(&mismatched_pending).expect("pending payload")),
            StaticProvider {
                responses: Vec::new(),
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            },
            &CancellationToken::new(),
            None,
        )
        .expect_err("mismatched checkpoint must fail closed");
    assert!(matches!(
        mismatch_error,
        AppServerError::Store(StoreError::InvalidState(_))
    ));

    let invalid_args_error = server
        .resume_agent_loop_after_gate(
            &request,
            &decision,
            Some(serde_json::to_value(&invalid_args_pending).expect("pending payload")),
            StaticProvider {
                responses: Vec::new(),
                seen_requests: Arc::new(Mutex::new(Vec::new())),
            },
            &CancellationToken::new(),
            None,
        )
        .expect_err("invalid checkpoint arguments must fail closed");
    assert!(matches!(
        invalid_args_error,
        AppServerError::Store(StoreError::InvalidState(_))
    ));
}

#[test]
fn agent_loop_approval_resume_uses_stored_pending_tool_call_after_gate() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "stable project instructions")
        .expect("stable agents");
    let file_path = workspace.join("README.md");
    std::fs::write(&file_path, "before").expect("write readme");
    let store = SessionStore::open(&db_path).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (prior, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "previous approval user"}]),
            "app_server",
            "prior turn",
        )
        .expect("prior turn");
    store
        .append_item(
            &prior.turn_id,
            ItemKind::AgentMessage,
            json!({"delta": "previous approval assistant"}),
        )
        .expect("prior assistant");
    store
        .update_turn_state(
            &prior.turn_id,
            TurnStatus::Completed,
            AgentStatus::Completed.as_str(),
        )
        .expect("complete prior turn");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Blocked.as_str(),
            json!([{"type": "text", "text": "edit readme"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let history = store
        .read_thread_history_before_turn(
            &thread.thread_id,
            &turn.turn_id,
            DEFAULT_THREAD_HISTORY_TURN_LIMIT,
        )
        .expect("history before approval turn");
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "edit readme".to_string(),
        }],
    };
    let mut initial_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "before approval");
    initial_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_1".to_string(),
        tool_name: TOOL_EDIT.to_string(),
        arguments: json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
        raw_arguments: json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        })
        .to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let mut verification_response =
        ModelTurnResponse::completed("model_request_turn_1_2", "response_3", "");
    verification_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_verify".to_string(),
        tool_name: TOOL_COMMAND.to_string(),
        arguments: json!({
            "command": "cmd.exe /C \"echo verified\"",
            "timeout_seconds": 5
        }),
        raw_arguments: json!({
            "command": "cmd.exe /C \"echo verified\"",
            "timeout_seconds": 5
        })
        .to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let plan_arguments = json!({
        "steps": [{"step": "verify README.md", "status": "completed"}],
        "verification": [{
            "risk": "general_mutation",
            "evidence": "README.md changed by the approved edit",
            "affected_symbol": "README.md::document",
            "current_gap": "the edited document has not been verified",
            "action": {
                "command": "cmd.exe /C \"echo verified\"",
                "cwd": ".",
                "timeout_seconds": 5,
                "sandbox_mode": "workspace_write",
                "network_access": "denied"
            },
            "required": 1
        }]
    });
    let mut plan_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "");
    plan_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_plan".to_string(),
        tool_name: "update_plan".to_string(),
        arguments: plan_arguments.clone(),
        raw_arguments: plan_arguments.to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_3", "response_4", "done");
    let initial_seen_requests = Arc::new(Mutex::new(Vec::new()));
    let initial_provider = StaticProvider {
        responses: vec![initial_response],
        seen_requests: Arc::clone(&initial_seen_requests),
    };
    let resumed_seen_requests = Arc::new(Mutex::new(Vec::new()));
    let resumed_provider = StreamingProvider {
        responses: vec![
            (Vec::new(), plan_response),
            (Vec::new(), verification_response),
            (
                vec![
                    ProviderStreamEvent::OutputTextDelta {
                        delta: "do".to_string(),
                    },
                    ProviderStreamEvent::OutputTextDelta {
                        delta: "ne".to_string(),
                    },
                ],
                final_response,
            ),
        ],
        seen_requests: Arc::clone(&resumed_seen_requests),
    };
    let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

    let cancellation = CancellationToken::new();
    let blocked_status = server
        .run_agent_loop_with_provider(
            initial_provider,
            &thread,
            &params,
            &turn.turn_id,
            &history.messages,
            &cancellation,
        )
        .expect("initial agent loop");
    assert_eq!(blocked_status.status, AgentStatus::Blocked);
    assert_eq!(blocked_status.approval_count, 1);
    server
        .commit_turn_run_status(turn.clone(), &blocked_status, None, &cancellation, None)
        .expect("commit blocked turn");
    let blocked_json = serde_json::to_string(&blocked_status).expect("blocked status json");
    assert!(!blocked_json.contains("checkpoint_version"));
    assert!(!blocked_json.contains("raw_arguments"));
    for trace in server
        .store
        .list_trace(&thread.thread_id)
        .expect("thread trace")
    {
        let trace_json = serde_json::to_string(&trace.payload).expect("trace payload json");
        assert!(!trace_json.contains("checkpoint_version"));
        assert!(!trace_json.contains("raw_arguments"));
    }
    drop(server);
    let server = app_server(SessionStore::open(&db_path).expect("reopen store"))
        .with_sandbox_backend(CompletedSandboxBackend);
    let request = server
        .store
        .get_pending_approval(&format!("approval_{}_call_1", turn.turn_id))
        .expect("stored approval");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let recorded = server
        .store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("record approval");
    let pending_payload = recorded.pending_tool_call.expect("checkpoint payload");
    assert_eq!(pending_payload["checkpoint_version"], 3);
    assert!(
        pending_payload["project_instructions_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:"))
    );
    assert!(pending_payload["messages"].is_array());
    assert!(pending_payload["tool_result_occurrences"].is_array());

    server
        .event_filter
        .lock()
        .expect("event filter")
        .event_types = Some(vec![
        "item/started".to_string(),
        "item/agentMessage/delta".to_string(),
        "item/completed".to_string(),
        "turn/completed".to_string(),
    ]);
    let mut assistant_events =
        AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
    let mut realtime_events = Vec::new();
    let resumed = server
        .resume_agent_loop_after_gate_with_text_deltas(
            &request,
            &decision,
            Some(pending_payload),
            resumed_provider,
            &CancellationToken::new(),
            Some(
                WorkspaceTools::new(&workspace)
                    .expect("bind workspace tools")
                    .with_sandbox_backend(CompletedSandboxBackend),
            ),
            &mut |delta| {
                realtime_events.extend(
                    server
                        .project_assistant_delta(&mut assistant_events, delta)
                        .expect("project approval delta"),
                );
            },
        )
        .expect("resume")
        .expect("resumed");

    assert_eq!(resumed.0.turn_id, turn.turn_id);
    assert_eq!(resumed.1.status, AgentStatus::Completed);
    assert_eq!(resumed.1.final_answer.as_deref(), Some("done"));
    assert_eq!(resumed.1.model_turns, 4);
    assert_eq!(resumed.1.tool_calls, 3);
    assert_eq!(resumed.1.approval_count, 1);
    assert!(resumed.1.verification.required);
    assert!(resumed.1.verification.passed);
    assert_eq!(resumed.1.verification.successful_command_count, 1);
    let committed = server
        .commit_effective_turn_status_resolving_approval(
            &request.request_id,
            &resumed.0,
            &resumed.1,
            &resumed.2,
            Some(&assistant_events.item_id),
            None,
        )
        .expect("commit resumed outcome");
    realtime_events.extend(
        server
            .committed_turn_events(&committed, Some(&assistant_events))
            .expect("approval terminal events"),
    );
    assert_eq!(committed.turn.status, TurnStatus::Completed);
    assert_eq!(
        realtime_events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>(),
        vec![
            "item/started",
            "item/agentMessage/delta",
            "item/agentMessage/delta",
            "item/started",
            "item/completed",
            "item/completed",
            "turn/completed",
        ]
    );
    assert_eq!(realtime_events[1]["params"]["delta"], "do");
    assert_eq!(realtime_events[2]["params"]["delta"], "ne");
    assert!(
        realtime_events[..3].iter().all(|event| {
            event["params"]["item"]["item_id"] == assistant_events.item_id.as_str()
        })
    );
    assert_eq!(
        realtime_events[5]["params"]["item"]["item_id"],
        assistant_events.item_id.as_str()
    );
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .map(|item| item.item_id.as_str()),
        Some(assistant_events.item_id.as_str())
    );
    let terminal_trace = server
        .store
        .list_trace(&thread.thread_id)
        .expect("thread trace")
        .into_iter()
        .find(|trace| trace.component == "agent_loop" && trace.payload["status"] == "completed")
        .expect("terminal agent trace");
    assert!(terminal_trace.payload.get("tool_outcomes").is_none());
    let terminal_trace_json =
        serde_json::to_string(&terminal_trace.payload).expect("terminal trace json");
    for full_result_field in ["content", "preview", "artifact_refs", "result_id"] {
        assert!(!terminal_trace_json.contains(full_result_field));
    }
    assert!(
        !server
            .store
            .has_pending_tool_call(&request.request_id)
            .expect("pending lookup")
    );
    assert_eq!(
        std::fs::read_to_string(&file_path).expect("read readme"),
        "after"
    );
    let requests = resumed_seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    assert_eq!(requests[0].messages[1].role, ModelRole::User);
    assert_eq!(requests[0].messages[1].content, "previous approval user");
    assert_eq!(requests[0].messages[2].role, ModelRole::Assistant);
    assert_eq!(
        requests[0].messages[2].content,
        "previous approval assistant"
    );
    assert_eq!(requests[0].messages[3].role, ModelRole::User);
    assert_eq!(requests[0].messages[3].content, "edit readme");
    assert_eq!(requests[0].messages[4].role, ModelRole::Assistant);
    assert_eq!(requests[0].messages[4].content, "before approval");
    assert_eq!(requests[0].messages[4].tool_calls.len(), 1);
    assert_eq!(requests[0].messages[5].role, ModelRole::Tool);
    assert_eq!(
        requests[0].messages[5].tool_call_id.as_deref(),
        Some("call_1")
    );
}

#[test]
fn agent_loop_approval_resume_fails_closed_when_project_instructions_change() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "initial project instructions")
        .expect("initial agents");
    let file_path = workspace.join("README.md");
    std::fs::write(&file_path, "before").expect("write readme");
    let store = SessionStore::open(&db_path).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _, _) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "edit readme"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "edit readme".to_string(),
        }],
    };
    let mut initial_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "before");
    initial_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_1".to_string(),
        tool_name: TOOL_EDIT.to_string(),
        arguments: json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        }),
        raw_arguments: json!({
            "path": "README.md",
            "expected": "before",
            "replacement": "after"
        })
        .to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let initial_seen_requests = Arc::new(Mutex::new(Vec::new()));
    let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);
    let blocked_status = server
        .run_agent_loop_with_provider(
            StaticProvider {
                responses: vec![initial_response],
                seen_requests: Arc::clone(&initial_seen_requests),
            },
            &thread,
            &params,
            &turn.turn_id,
            &[],
            &CancellationToken::new(),
        )
        .expect("initial agent loop");
    assert_eq!(blocked_status.status, AgentStatus::Blocked);
    server
        .commit_turn_run_status(
            turn.clone(),
            &blocked_status,
            None,
            &CancellationToken::new(),
            None,
        )
        .expect("commit blocked turn");
    assert!(
        !serde_json::to_string(&blocked_status)
            .expect("blocked status json")
            .contains("project_instructions_digest")
    );
    let initial_request_json =
        serde_json::to_string(&initial_seen_requests.lock().expect("initial requests")[0])
            .expect("initial request json");
    assert!(!initial_request_json.contains("AGENTS.md"));
    assert!(!initial_request_json.contains(workspace.to_string_lossy().as_ref()));
    for trace in server
        .store
        .list_trace(&thread.thread_id)
        .expect("thread trace")
    {
        assert!(
            !serde_json::to_string(&trace.payload)
                .expect("trace json")
                .contains("project_instructions_digest")
        );
    }
    drop(server);

    let project_sentinel = "project-instruction-sentinel";
    std::fs::write(workspace.join("AGENTS.override.md"), project_sentinel)
        .expect("override agents");
    let server = app_server(SessionStore::open(&db_path).expect("reopen store"))
        .with_sandbox_backend(CompletedSandboxBackend);
    let request = server
        .store
        .get_pending_approval(&format!("approval_{}_call_1", turn.turn_id))
        .expect("stored approval");
    let decision = ApprovalDecision::new(
        request.request_id.clone(),
        ApprovalOutcome::Allow,
        "approved",
    );
    let recorded = server
        .store
        .record_approval_decision(&decision, "approval", "approval decision recorded")
        .expect("record approval");
    let pending_payload = recorded.pending_tool_call.expect("checkpoint payload");
    let checkpoint_digest = pending_payload["project_instructions_digest"]
        .as_str()
        .expect("checkpoint project instruction digest");
    assert!(checkpoint_digest.starts_with("sha256:"));

    let resumed_seen_requests = Arc::new(Mutex::new(Vec::new()));
    let resumed = server
        .resume_agent_loop_after_gate(
            &request,
            &decision,
            Some(pending_payload),
            StaticProvider {
                responses: vec![ModelTurnResponse::completed(
                    "model_request_turn_1_0",
                    "response_1",
                    "must not run",
                )],
                seen_requests: Arc::clone(&resumed_seen_requests),
            },
            &CancellationToken::new(),
            Some(
                WorkspaceTools::new(&workspace)
                    .expect("bind workspace tools")
                    .with_sandbox_backend(CompletedSandboxBackend),
            ),
        )
        .expect("resume")
        .expect("terminal status");

    assert_eq!(resumed.1.status, AgentStatus::Failed);
    assert_eq!(resumed.1.error.as_deref(), Some(SAFE_AGENT_LOOP_FAILURE));
    let resumed_json = serde_json::to_string(&resumed.1).expect("resumed status json");
    assert!(!resumed_json.contains(project_sentinel));
    assert!(
        resumed_seen_requests
            .lock()
            .expect("resumed requests")
            .is_empty()
    );
    let committed = server
        .commit_effective_turn_status_resolving_approval(
            &request.request_id,
            &resumed.0,
            &resumed.1,
            &resumed.2,
            Some(&SessionStore::allocate_assistant_item_id()),
            None,
        )
        .expect("commit project instruction failure");
    let trace_json = serde_json::to_string(&committed.trace).expect("trace json");
    assert!(!trace_json.contains(project_sentinel));
    assert_eq!(committed.trace.payload["error"], SAFE_AGENT_LOOP_FAILURE);
    let history_json = serde_json::to_string(
        &server
            .store
            .read_thread_history(&thread.thread_id, None, DEFAULT_THREAD_HISTORY_TURN_LIMIT)
            .expect("history"),
    )
    .expect("history json");
    assert!(!history_json.contains(project_sentinel));
    assert_eq!(
        std::fs::read_to_string(&file_path).expect("read readme"),
        "before"
    );
}

struct CompletedSandboxBackend;

struct MutatingCommandSandboxBackend {
    calls: AtomicUsize,
}

impl SandboxBackend for MutatingCommandSandboxBackend {
    fn name(&self) -> &'static str {
        "mutating_command_test"
    }

    fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
        singularity_tools::SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        self.result(&request.command_id)
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        self.result(&request.command_id)
    }
}

impl MutatingCommandSandboxBackend {
    fn result(&self, command_id: &str) -> CommandResult {
        let changed = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let result = CommandResult::completed(command_id, "command ok")
            .with_workspace_mutation(if changed {
                WorkspaceMutation::Changed
            } else {
                WorkspaceMutation::Unchanged
            })
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            );
        if changed {
            result.with_workspace_change_summary(WorkspaceChangeSummary::new(
                vec![".".to_string()],
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ))
        } else {
            result
        }
    }
}

impl SandboxBackend for CompletedSandboxBackend {
    fn name(&self) -> &'static str {
        "completed_test"
    }

    fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
        singularity_tools::SandboxCapabilities::strict().with_change_detection()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "app-server-sandbox-ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::completed(&request.command_id, "app-server-sandbox-ok")
            .with_workspace_mutation(WorkspaceMutation::Unchanged)
            .with_sandbox_execution(
                self.name(),
                singularity_tools::SandboxBackendEnforcement::Strict,
            )
    }
}

struct UnavailableSandboxBackend;

impl SandboxBackend for UnavailableSandboxBackend {
    fn name(&self) -> &'static str {
        "unavailable_test"
    }

    fn capabilities(&self) -> singularity_tools::SandboxCapabilities {
        singularity_tools::SandboxCapabilities::unavailable()
    }

    fn execute(&self, request: &CommandRequest) -> CommandResult {
        CommandResult::sandbox_backend_unavailable(&request.command_id)
            .with_workspace_mutation(WorkspaceMutation::Unknown)
    }

    fn execute_script(&self, request: &CommandScriptRequest) -> CommandResult {
        CommandResult::sandbox_backend_unavailable(&request.command_id)
            .with_workspace_mutation(WorkspaceMutation::Unknown)
    }
}

#[test]
fn agent_loop_capability_is_projected_from_the_bound_sandbox_backend() {
    let available = agent_loop_capability(&CompletedSandboxBackend);
    assert!(available.available);
    assert!(available.blockers.is_empty());
    assert!(available.reason.contains("completed_test"));

    let unavailable = agent_loop_capability(&UnavailableSandboxBackend);
    assert!(!unavailable.available);
    assert_eq!(
        unavailable.blockers,
        vec![STRICT_COMMAND_SANDBOX_UNAVAILABLE]
    );
    assert!(unavailable.reason.contains("unavailable_test"));
}

#[test]
fn agent_loop_command_uses_bound_sandbox_backend_without_approval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let params = TurnStartParams {
        thread_id: thread.thread_id.clone(),
        input: vec![singularity_protocol::InputItem::Text {
            text: "run command".to_string(),
        }],
    };
    let mut command_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    command_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_1".to_string(),
        tool_name: "command".to_string(),
        arguments: json!({
            "command": "cmd.exe /C \"echo app-server-sandbox-ok\"",
            "timeout_seconds": 5
        }),
        raw_arguments: json!({
            "command": "cmd.exe /C \"echo app-server-sandbox-ok\"",
            "timeout_seconds": 5
        })
        .to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    let final_response =
        ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done");
    let provider = StaticProvider {
        responses: vec![command_response, final_response],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let server = app_server(store).with_sandbox_backend(CompletedSandboxBackend);

    let status = server
        .run_agent_loop_with_provider(
            provider,
            &thread,
            &params,
            "turn_1",
            &[],
            &CancellationToken::new(),
        )
        .expect("agent loop");

    assert_eq!(status.status, AgentStatus::Completed);
    assert_eq!(status.final_answer.as_deref(), Some("done"));
    assert_eq!(status.tool_calls, 1);
    assert_eq!(status.approval_count, 0);
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

/// Verification Span 按 occurrence_id 分组，每个 ID 恰好一个 Start 和一个 End。
#[test]
fn verification_spans_are_paired_by_occurrence_id() {
    use singularity_agent::{
        AgentLoopEvent, AgentObservation, OccurrenceIdentity, OccurrenceLifecycle,
        VerificationObservation, VerificationStatus,
    };
    use singularity_protocol::{TraceSpanKind, TraceSpanPhase};
    use std::collections::BTreeMap;

    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _item, _trace) = store
        .create_turn_with_input_and_trace(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "verify"}]),
            "app_server",
            "turn started",
        )
        .expect("turn");

    let mut projector =
        observability::TraceProjector::new(&store, &thread.thread_id, &turn.turn_id)
            .expect("projector");

    // 投影三个不同 occurrence_id 的 verification observation
    for ordinal in 0..3u32 {
        // 每个 ordinal 使用不同的 occurrence_id
        let identity = OccurrenceIdentity {
            occurrence_id: format!("sha256:{}{:02x}", "cd".repeat(31), ordinal),
            parent_occurrence_id: None,
            ordinal,
        };
        let started = VerificationObservation {
            identity: identity.clone(),
            lifecycle: OccurrenceLifecycle::Started {
                queued_at_unix_ms: 100 + u64::from(ordinal),
                started_at_unix_ms: 101 + u64::from(ordinal),
            },
            required_command_count: 1,
            satisfied_command_count: 0,
            occurrence_count: ordinal + 1,
            command_duration_ms: None,
        };
        projector
            .project_event(AgentLoopEvent::Observation(AgentObservation::Verification(
                started,
            )))
            .expect("project start");

        let finished = VerificationObservation {
            identity,
            lifecycle: OccurrenceLifecycle::Finished {
                queued_at_unix_ms: 100 + u64::from(ordinal),
                started_at_unix_ms: 101 + u64::from(ordinal),
                ended_at_unix_ms: 200 + u64::from(ordinal),
                duration_ms: 99,
                status: VerificationStatus::CommandPassed,
            },
            required_command_count: 1,
            satisfied_command_count: 1,
            occurrence_count: ordinal + 1,
            command_duration_ms: Some(50),
        };
        projector
            .project_event(AgentLoopEvent::Observation(AgentObservation::Verification(
                finished,
            )))
            .expect("project end");
    }

    // 读取 trace 并按 span_id 分组
    let trace = store.list_trace(&thread.thread_id).expect("trace");
    let mut spans: BTreeMap<String, Vec<TraceSpanPhase>> = BTreeMap::new();
    for event in &trace {
        if event.span_kind == Some(TraceSpanKind::Verification)
            && let (Some(span_id), Some(phase)) = (&event.span_id, event.span_phase)
        {
            spans.entry(span_id.clone()).or_default().push(phase);
        }
    }

    // 每个 occurrence_id 恰好一个 Start 和一个 End
    assert_eq!(spans.len(), 3, "three distinct verification span ids");
    for (span_id, phases) in &spans {
        let starts = phases
            .iter()
            .filter(|p| **p == TraceSpanPhase::Start)
            .count();
        let ends = phases.iter().filter(|p| **p == TraceSpanPhase::End).count();
        assert_eq!(starts, 1, "span {span_id} must have exactly one Start");
        assert_eq!(ends, 1, "span {span_id} must have exactly one End");
    }
}
