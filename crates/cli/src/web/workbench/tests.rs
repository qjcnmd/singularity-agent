#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use singularity_agent::session::test_support::WorkspaceFixture;
use singularity_model::{
    ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderStreamEvent,
};
use singularity_protocol::{HistoryItem, RpcErrorCode, StreamEvent};

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
                .history
                .summary
                .thread_id
        })
        .collect();

    for (index, session_id) in sessions.iter().enumerate() {
        fixture
            .workbench
            .submit(
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
        &workspace.workspace_id,
        &sessions[0],
        "keep this text".to_string(),
    );
    assert!(matches!(duplicate, Err(ref error)
            if error.code == RpcErrorCode::SessionBusy
                && error.preserved_input.as_deref() == Some("keep this text")));

    for session_id in &sessions {
        fixture
            .workbench
            .abort(&workspace.workspace_id, session_id)
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
    let id = session.history.summary.thread_id;
    fixture
        .workbench
        .submit(&workspace.workspace_id, &id, "panic-provider".to_string())
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
        .submit(&workspace.workspace_id, &id, "retry".to_string())
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
    let id = created.history.summary.thread_id;
    let external = Conversation::new(
        Arc::clone(&host.runner),
        host.catalog.resume_thread(&id).unwrap(),
    );
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
    let id = created.history.summary.thread_id;
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
            let result = reservation.run("first", &mut |event| {
                event_host.on_turn_event(&event_id, &event_slot, event)
            });
            (result, reservation)
        })
    };
    started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    host.follow_up(&workspace.workspace_id, &id, "next".into())
        .unwrap();
    let pending = slot.conversation.pending_controls()[0].clone();
    host.abort(&workspace.workspace_id, &id).unwrap();
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();

    // The single runtime reservation remains held until projection settlement.
    let rejected = host.queue_send_now(&workspace.workspace_id, &id, &pending.control_id);
    assert!(matches!(rejected, Err(error) if error.code == RpcErrorCode::SessionBusy));
    assert_eq!(slot.conversation.pending_controls(), vec![pending.clone()]);
    assert_eq!(slot.conversation.phase(), SessionPhase::Reserved);
    host.on_session_settled(&id, &slot, turn_terminal(outcome), reservation);
    host.queue_send_now(&workspace.workspace_id, &id, &pending.control_id)
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
    let id = created.history.summary.thread_id;
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
            let result = reservation.run("first", &mut |event| {
                event_host.on_turn_event(&event_id, &event_slot, event)
            });
            (result, reservation)
        })
    };
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "first"
    );
    host.follow_up(&workspace.workspace_id, &id, "next".into())
        .unwrap();
    release_tx.send(()).unwrap();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "next"
    );

    let snapshot = slot.snapshot();
    assert!(snapshot.pending_controls.is_empty());
    let published = std::iter::from_fn(|| stream.try_recv().ok()).any(|frame| {
        matches!(frame.event, StreamEvent::SessionChanged { payload, .. }
        if payload.pending_controls.is_empty())
    });
    assert!(
        published,
        "the consumed queue state is published while the next turn runs"
    );

    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();
    host.on_session_settled(&id, &slot, turn_terminal(outcome), reservation);
}

/// 快照发布失败不得推翻已提交的操作：工作区仍然存在，RPC 返回成功，
/// 读侧恢复经重同步通道表达。
#[test]
fn snapshot_failure_does_not_fail_a_committed_mutation() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let first = fixture
        .workbench
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("workspace");
    let mut receiver = host.subscribe();
    // Make the catalog scan fail (read_dir on a file) so the next full
    // snapshot cannot be built while the mutation itself stays local.
    let sessions = fixture._home.path().join("sessions");
    std::fs::remove_dir_all(&sessions).unwrap();
    std::fs::write(&sessions, b"not a directory").unwrap();
    let second_root = fixture._home.path().join("second-workspace");
    std::fs::create_dir_all(&second_root).unwrap();
    let added = host.add_workspace(&second_root.to_string_lossy());
    assert!(
        added.is_ok(),
        "a committed add must not be reported as failed"
    );
    assert!(host.workspaces.find(&first.workspace_id).is_some());
    let frame = receiver.try_recv().unwrap();
    assert!(
        matches!(frame.event, StreamEvent::ResyncRequired { .. }),
        "clients are asked to resync instead of seeing a fake mutation failure"
    );
    // 读侧恢复后，快照发布回归正常通道。
    std::fs::remove_file(&sessions).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    host.publish_workbench_snapshot();
    let frame = receiver.try_recv().unwrap();
    assert!(matches!(frame.event, StreamEvent::WorkbenchChanged { .. }));
}

/// 结算保留执行链的可信终态；历史读取失败由会话读取路径独立呈现，
/// 不再把 Completed 改写成 Failed。
#[test]
fn settlement_keeps_the_trusted_terminal_when_history_cannot_be_read() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([
            singularity_model::test_support::ScriptedAttempt::success("done"),
        ]),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let id = host
        .create_session(&workspace.workspace_id, None)
        .unwrap()
        .history
        .summary
        .thread_id;
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut reservation = slot.conversation.reserve_start().unwrap();
    let outcome = reservation.run("first", &mut |_event| {}).unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    std::fs::remove_file(
        fixture
            ._home
            .path()
            .join("sessions")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    host.on_session_settled(&id, &slot, turn_terminal(Ok(outcome)), reservation);
    assert_eq!(
        slot.snapshot().terminal.expect("terminal").status,
        TurnStatus::Completed,
        "the trusted terminal survives a broken history read"
    );
    assert!(
        host.read_session(&workspace.workspace_id, &id, 40, None)
            .is_err(),
        "the read-side failure stays visible through the session read path"
    );
    assert_eq!(
        slot.snapshot().terminal.expect("terminal").status,
        TurnStatus::Completed,
        "a failed read must not rewrite the terminal"
    );
}

#[test]
fn unopened_history_does_not_block_removing_a_project() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    host.catalog.create_thread(&workspace.root, None).unwrap();
    assert!(host.lock_sessions().is_empty());
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
    let workbench = Workbench::new(runner, catalog, workspaces, models);
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
