#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use singularity_agent::session::test_support::WorkspaceFixture;
use singularity_model::{
    ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderStreamEvent,
};
use singularity_protocol::{EmptyParams, HistoryItem, RpcErrorCode, StreamEvent};

use super::*;

#[test]
fn model_discovery_errors_preserve_recovery_category() {
    let network = model_discovery_error(ProviderError::new(
        ModelErrorKind::NetworkError,
        "network unavailable",
    ));
    assert_eq!(network.code, RpcErrorCode::ProviderUnavailable);
    assert!(network.recovery.contains("稍后重试"));
    assert!(network.recovery.contains("手动添加模型"));

    let configuration = model_discovery_error(
        ProviderError::new(ModelErrorKind::InvalidRequest, "invalid provider")
            .with_code("provider_configuration_invalid"),
    );
    assert_eq!(configuration.code, RpcErrorCode::ConfigurationInvalid);
    assert!(configuration.recovery.contains("模型设置"));

    let authentication = model_discovery_error(ProviderError::new(
        ModelErrorKind::AuthError,
        "invalid credential",
    ));
    assert_eq!(authentication.code, RpcErrorCode::ConfigurationInvalid);
    assert!(authentication.recovery.contains("API 地址和密钥"));
}

struct BlockingProvider {
    started: Sender<String>,
    release: Mutex<Receiver<()>>,
}

impl Provider for BlockingProvider {
    fn model_configuration(&self) -> singularity_model::ModelConfigurationSnapshot {
        singularity_runtime::test_support::test_model_configuration()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        _on_event: &mut dyn FnMut(ProviderStreamEvent),
        _record_attempt: &mut dyn FnMut(
            singularity_model::ProviderAttemptEvent,
        ) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, singularity_model::ProviderCallError> {
        let input = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == singularity_model::ModelRole::User)
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let panic_requested = input == "panic-provider";
        self.started.send(input).expect("report request");
        assert!(!panic_requested, "injected provider panic");
        self.release.lock().expect("release lock").recv().ok();
        if cancellation.is_cancelled() {
            return Err(ProviderError::new(ModelErrorKind::Cancelled, "cancelled by test").into());
        }
        Ok(ModelTurnResponse::completed("done"))
    }
}

#[test]
fn three_sessions_run_without_a_browser_and_keep_inputs_isolated() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let provider = Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
    });
    let fixture = fixture(provider);
    let workspace = fixture
        .workbench
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("workspace");
    let sessions: Vec<_> = (0..3)
        .map(|_| {
            fixture
                .workbench
                .create_session(&workspace.workspace_id, None)
                .expect("session")
                .summary
                .thread_id
        })
        .collect();

    for (index, session_id) in sessions.iter().enumerate() {
        fixture
            .workbench
            .submit(
                &format!("request-{index}"),
                &workspace.workspace_id,
                session_id,
                format!("input-{index}"),
            )
            .expect("submit");
    }
    let mut started: Vec<_> = (0..3)
        .map(|_| {
            started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("all sessions reached provider")
        })
        .collect();
    started.sort();
    assert_eq!(started, vec!["input-0", "input-1", "input-2"]);

    for session_id in &sessions {
        let snapshot = fixture
            .workbench
            .read_session(&workspace.workspace_id, session_id, 100, None)
            .expect("running snapshot");
        assert!(
            snapshot
                .history
                .turns
                .iter()
                .all(|turn| turn.turn_id.is_none()),
            "live turns are excluded from durable history until settled"
        );
        assert!(snapshot.runtime.active_turn.is_some());
    }

    let duplicate = fixture.workbench.submit(
        "duplicate",
        &workspace.workspace_id,
        &sessions[0],
        "keep this text".to_string(),
    );
    assert!(matches!(duplicate, Err(ref error)
            if error.code == RpcErrorCode::SessionBusy
                && error.preserved_input.as_deref() == Some("keep this text")));

    for (index, session_id) in sessions.iter().enumerate() {
        fixture
            .workbench
            .abort(
                &format!("abort-{index}"),
                &workspace.workspace_id,
                session_id,
            )
            .expect("abort");
    }
    for _ in 0..3 {
        release_tx.send(()).expect("release provider");
    }
    wait_for_idle(&fixture.workbench, &workspace, &sessions);

    for (index, session_id) in sessions.iter().enumerate() {
        let snapshot = fixture
            .workbench
            .read_session(&workspace.workspace_id, session_id, 100, None)
            .expect("read session");
        let messages: Vec<_> = snapshot
            .history
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter_map(|item| match item {
                HistoryItem::Message { role, text, .. } if role == "user" => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec![format!("input-{index}")]);
        assert_eq!(snapshot.runtime.phase, SessionPhase::Idle);
    }
}

#[test]
fn stream_is_bounded_and_reports_lag_without_blocking_emitters() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([
            singularity_model::test_support::ScriptedAttempt::success("done"),
        ]),
    ));
    let mut receiver = fixture.workbench.subscribe();
    for _ in 0..=STREAM_CAPACITY {
        fixture.workbench.emit(StreamEvent::Ready {
            payload: EmptyParams {},
        });
    }
    assert!(matches!(
        receiver.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(1))
    ));
}

#[test]
fn later_workbench_revision_does_not_publish_an_older_snapshot() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let mut receiver = host.subscribe();
    let (built_tx, built_rx) = channel();
    let (release_tx, release_rx) = channel();
    let first = {
        let host = Arc::clone(host);
        std::thread::spawn(move || {
            let snapshot_host = Arc::clone(&host);
            host.emit_workbench_changed_with(move || {
                let snapshot = snapshot_host.bootstrap()?;
                built_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(snapshot)
            })
        })
    };
    built_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("older snapshot is built before publishing");
    assert!(matches!(
        host.workbench_publication.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    host.workspaces
        .rename(&workspace.workspace_id, "new name")
        .unwrap();
    let (second_started_tx, second_started_rx) = channel();
    let second = {
        let host = Arc::clone(host);
        std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            host.emit_workbench_changed()
        })
    };
    second_started_rx.recv().unwrap();
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();

    let earlier = receiver.try_recv().unwrap();
    let last = receiver.try_recv().unwrap();
    assert!(earlier.revision < last.revision);
    let StreamEvent::WorkbenchChanged { payload } = last.event else {
        panic!("expected a workbench snapshot");
    };
    assert_eq!(
        payload
            .workspaces
            .iter()
            .find(|item| item.workspace_id == workspace.workspace_id)
            .map(|item| item.name.as_str()),
        Some("new name")
    );
}

#[test]
fn creating_a_session_preserves_its_requested_selector() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let workspace = fixture
        .workbench
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("workspace");
    let snapshot = fixture
        .workbench
        .create_session(
            &workspace.workspace_id,
            Some("openai_compatible/chosen-model".to_string()),
        )
        .expect("session");
    assert_eq!(
        snapshot.runtime.selector.as_deref(),
        Some("openai_compatible/chosen-model")
    );
}

#[test]
fn worker_panic_settles_the_slot_and_allows_another_turn() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
    }));
    let workspace = fixture
        .workbench
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("workspace");
    let session = fixture
        .workbench
        .create_session(&workspace.workspace_id, None)
        .expect("session");
    let id = session.summary.thread_id;
    fixture
        .workbench
        .submit(
            "panic",
            &workspace.workspace_id,
            &id,
            "panic-provider".to_string(),
        )
        .expect("submit");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("started");
    wait_for_idle(&fixture.workbench, &workspace, std::slice::from_ref(&id));
    let snapshot = fixture
        .workbench
        .read_session(&workspace.workspace_id, &id, 100, None)
        .expect("settled snapshot");
    assert_eq!(
        snapshot.runtime.terminal.expect("terminal").status,
        TurnStatus::Failed
    );
    fixture
        .workbench
        .submit("retry", &workspace.workspace_id, &id, "retry".to_string())
        .expect("next submit");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("next started");
    release_tx.send(()).expect("release");
    wait_for_idle(&fixture.workbench, &workspace, &[id]);
}

#[test]
fn idle_reads_and_new_chains_use_the_latest_durable_history() {
    let provider = Arc::new(singularity_model::test_support::ScriptedProvider::new([
        singularity_model::test_support::ScriptedAttempt::success("first"),
        singularity_model::test_support::ScriptedAttempt::success("second"),
    ]));
    let fixture = fixture(provider);
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.summary.thread_id;
    let external = Conversation::new(
        Arc::clone(&host.runner),
        host.catalog.resume_thread(&id).unwrap(),
    )
    .unwrap();
    external
        .run_turn("first external input", &mut |_| {})
        .unwrap();
    let bootstrap = host.bootstrap().unwrap();
    assert_eq!(
        bootstrap.sessions_by_workspace[&workspace.workspace_id][0].turn_count,
        1
    );
    let read = host
        .read_session(&workspace.workspace_id, &id, 40, None)
        .unwrap();
    assert_eq!(
        read.history
            .turns
            .iter()
            .filter(|turn| turn.turn_id.is_some())
            .count(),
        1
    );
    external
        .run_turn("second external input", &mut |_| {})
        .unwrap();

    // Starting without a prior browser read must freeze both external turns.
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let reservation = slot.conversation.reserve_start().unwrap();
    host.begin_turn(&slot, "new chain").unwrap();
    let read = host
        .read_session(&workspace.workspace_id, &id, 40, None)
        .unwrap();
    assert_eq!(
        read.history
            .turns
            .iter()
            .filter(|turn| turn.turn_id.is_some())
            .count(),
        2
    );
    assert_eq!(read.runtime.phase, SessionPhase::Reserved);
    assert!(read.runtime.active_turn.is_none());
    host.on_session_settled(&id, &slot, None, reservation);
}

#[test]
fn send_now_waits_for_workbench_settlement_and_keeps_the_pending_input() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
    }));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.summary.thread_id;
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut reservation = slot.conversation.reserve_start().unwrap();
    host.begin_turn(&slot, "first").unwrap();
    let worker = {
        let host = Arc::clone(host);
        let slot = Arc::clone(&slot);
        let id = id.clone();
        std::thread::spawn(move || {
            let event_host = Arc::clone(&host);
            let event_slot = Arc::clone(&slot);
            let event_id = id.clone();
            let result = reservation.run_with_control_updates(
                "first",
                &mut |event| event_host.on_turn_event(&event_id, &event_slot, event),
                &mut || host.on_controls_changed(&id, &slot),
            );
            (result, reservation)
        })
    };
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let pending = host
        .follow_up("queue", &workspace.workspace_id, &id, "next".into())
        .unwrap()
        .control
        .unwrap();
    host.abort("abort", &workspace.workspace_id, &id).unwrap();
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();

    // The single runtime reservation remains held until projection settlement.
    let rejected = host.queue_send_now("early", &workspace.workspace_id, &id, &pending.control_id);
    assert!(matches!(rejected, Err(error) if error.code == RpcErrorCode::SessionBusy));
    assert_eq!(slot.conversation.pending_controls(), vec![pending.clone()]);
    assert_eq!(slot.conversation.phase(), SessionPhase::Reserved);
    host.on_session_settled(&id, &slot, turn_terminal(outcome), reservation);
    host.queue_send_now("retry", &workspace.workspace_id, &id, &pending.control_id)
        .unwrap();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "next"
    );
    release_tx.send(()).unwrap();
    wait_for_idle(host, &workspace, &[id]);
}

#[test]
fn automatic_follow_up_start_publishes_the_consumed_control_projection() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
    }));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.summary.thread_id;
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut stream = host.subscribe();
    let mut reservation = slot.conversation.reserve_start().unwrap();
    host.begin_turn(&slot, "first").unwrap();
    let worker = {
        let host = Arc::clone(host);
        let slot = Arc::clone(&slot);
        let id = id.clone();
        std::thread::spawn(move || {
            let event_host = Arc::clone(&host);
            let event_slot = Arc::clone(&slot);
            let event_id = id.clone();
            let result = reservation.run_with_control_updates(
                "first",
                &mut |event| event_host.on_turn_event(&event_id, &event_slot, event),
                &mut || host.on_controls_changed(&id, &slot),
            );
            (result, reservation)
        })
    };
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "first"
    );
    let control = host
        .follow_up("queue", &workspace.workspace_id, &id, "next".into())
        .unwrap()
        .control
        .unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "next"
    );

    let snapshot = slot.snapshot();
    assert!(snapshot.pending_controls.is_empty());
    assert!(snapshot.controls.iter().any(|candidate| {
        candidate.control_id == control.control_id
            && candidate.disposition
                == singularity_agent::session::ControlDisposition::StartedAsNewTurn
    }));
    let published = std::iter::from_fn(|| stream.try_recv().ok()).any(|frame| {
        matches!(frame.event, StreamEvent::SessionChanged { payload, .. }
        if payload.pending_controls.is_empty()
            && payload.controls.iter().any(|candidate| {
                candidate.control_id == control.control_id
                    && candidate.disposition
                        == singularity_agent::session::ControlDisposition::StartedAsNewTurn
            }))
    });
    assert!(
        published,
        "the consumed queue state is published while the next turn runs"
    );

    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();
    host.on_session_settled(&id, &slot, turn_terminal(outcome), reservation);
}

#[test]
fn settled_control_projection_is_not_replaced_by_a_late_pending_receipt() {
    let script = Arc::new(singularity_model::test_support::ScriptedProvider::new([
        singularity_model::test_support::ScriptedAttempt::success("first done"),
        singularity_model::test_support::ScriptedAttempt::success("follow-up done"),
    ]));
    let (gate, started_rx) = singularity_runtime::test_support::GatedProvider::new(
        script as Arc<dyn Provider + Send + Sync>,
    );
    let (release_tx, release_rx) = channel();
    gate.with_release(release_rx);
    let fixture = fixture(gate as Arc<dyn Provider + Send + Sync>);
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.summary.thread_id;
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut reservation = slot.conversation.reserve_start().unwrap();
    host.begin_turn(&slot, "first").unwrap();
    let worker = {
        std::thread::spawn(move || {
            let result = reservation.run("first", &mut |_event| {});
            (result, reservation)
        })
    };
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first turn reaches provider");
    let (accepted_tx, accepted_rx) = channel();
    let (publish_tx, publish_rx) = channel();
    let control_worker = {
        let host = Arc::clone(host);
        let slot = Arc::clone(&slot);
        let id = id.clone();
        std::thread::spawn(move || {
            host.apply_control("late", &id, &slot, "follow-up".into(), |conversation| {
                let control = conversation.submit_follow_up("follow-up").map(Some);
                accepted_tx.send(()).unwrap();
                publish_rx.recv().unwrap();
                control
            })
        })
    };
    accepted_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("follow-up is accepted before its projection is published");
    assert!(matches!(
        slot.state.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();
    let (settling_tx, settling_rx) = channel();
    let settlement = {
        let host = Arc::clone(host);
        let slot = Arc::clone(&slot);
        std::thread::spawn(move || {
            settling_tx.send(()).unwrap();
            host.on_session_settled(&id, &slot, turn_terminal(outcome), reservation);
        })
    };
    settling_rx.recv().unwrap();
    publish_tx.send(()).unwrap();
    let receipt = control_worker.join().unwrap().unwrap();
    settlement.join().unwrap();
    let control_id = receipt.control.unwrap().control_id;
    assert!(slot.snapshot().controls.iter().any(|control| {
        control.control_id == control_id
            && control.disposition
                == singularity_agent::session::ControlDisposition::StartedAsNewTurn
    }));
}

#[test]
fn unopened_pending_inputs_keep_the_project_registered() {
    use singularity_agent::session::{
        ControlChannel, ControlDisposition, ControlRequest, SessionManager, control_id,
    };
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let thread = host.catalog.create_thread(&workspace.root, None).unwrap();
    let mut writer = SessionManager::open_existing(
        &fixture
            ._home
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", thread.thread_id)),
    )
    .unwrap();
    let turn_id = Uuid::new_v4().to_string();
    let pending = ControlRequest {
        control_id: control_id(&turn_id, ControlChannel::FollowUp, 0),
        turn_id,
        channel: ControlChannel::FollowUp,
        sequence: 0,
        text: Some("keep this input".into()),
    };
    writer
        .append_record(pending.record(ControlDisposition::Pending))
        .unwrap();
    drop(writer);
    assert!(host.lock_sessions().is_empty());
    let error = host.remove_workspace(&workspace.workspace_id).unwrap_err();
    assert_eq!(error.code, RpcErrorCode::WorkspaceBusy);
    assert!(host.workspaces.find(&workspace.workspace_id).is_some());
    let mut writer = SessionManager::open_existing(
        &fixture
            ._home
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", thread.thread_id)),
    )
    .unwrap();
    writer
        .append_record(pending.record(ControlDisposition::Cancelled))
        .unwrap();
    drop(writer);
    host.remove_workspace(&workspace.workspace_id).unwrap();
}

#[cfg(windows)]
#[test]
fn failed_credential_removal_refreshes_future_model_selection() {
    use std::os::windows::fs::OpenOptionsExt;

    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let selector = "openai_compatible/base-model";
    host.runner.validate_model_selector(Some(selector)).unwrap();
    let auth_path = fixture._home.path().join("auth.json");
    let auth_guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x00000001 | 0x00000002)
        .open(&auth_path)
        .unwrap();
    host.remove_provider("openai_compatible")
        .expect_err("credential file cannot be replaced");
    assert!(host.runner.validate_model_selector(Some(selector)).is_err());
    assert!(host.lock_models().redacted_catalog().providers.is_empty());
    drop(auth_guard);
    assert!(
        std::fs::read_to_string(&auth_path)
            .unwrap()
            .contains("openai_compatible")
    );
    host.remove_provider("openai_compatible")
        .expect("retry finishes credential removal");
    assert!(
        !std::fs::read_to_string(auth_path)
            .unwrap()
            .contains("openai_compatible")
    );
}

struct Fixture {
    _home: tempfile::TempDir,
    _runtime: tokio::runtime::Runtime,
    workspace: WorkspaceFixture,
    workbench: Arc<Workbench>,
}

fn fixture(provider: Arc<dyn Provider + Send + Sync>) -> Fixture {
    let home = tempfile::tempdir().expect("home");
    std::fs::create_dir_all(home.path().join("sessions")).expect("sessions");
    singularity_runtime::test_support::write_provider_fixture(home.path(), "chosen-model");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let models = ModelConfigOwner::open_at(home.path().to_path_buf(), runtime.handle().clone());
    let runner = Arc::new(
        TurnRunner::new(home.path().join("sessions"), models.snapshot())
            .with_provider_override(provider),
    );
    let catalog = ThreadCatalog::new(&runner);
    let workspaces = WorkspaceStore::open(home.path()).expect("workspace store");
    let workbench = Workbench::new(
        "127.0.0.1:3080".to_string(),
        runner,
        catalog,
        workspaces,
        models,
    );
    Fixture {
        _home: home,
        _runtime: runtime,
        workspace: WorkspaceFixture::new(),
        workbench,
    }
}

fn wait_for_idle(workbench: &Workbench, workspace: &Workspace, sessions: &[String]) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let idle = sessions.iter().all(|session_id| {
            workbench
                .read_session(&workspace.workspace_id, session_id, 100, None)
                .is_ok_and(|snapshot| snapshot.runtime.phase == SessionPhase::Idle)
        });
        if idle {
            return;
        }
        assert!(Instant::now() < deadline, "sessions did not settle");
        std::thread::sleep(Duration::from_millis(10));
    }
}
