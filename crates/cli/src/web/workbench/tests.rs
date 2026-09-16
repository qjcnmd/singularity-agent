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
    /// 放行后额外发布的增量条数：0 表示只发布一条固定增量。
    deltas: usize,
}

impl Provider for BlockingProvider {
    fn model_configuration(&self) -> singularity_model::ModelConfigurationSnapshot {
        singularity_runtime::test_support::test_model_configuration()
    }

    fn complete_stream(
        &self,
        request: &ModelTurnRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProviderStreamEvent),
        record_attempt: &mut dyn FnMut(
            singularity_model::ProviderAttemptEvent,
        ) -> std::io::Result<()>,
    ) -> Result<ModelTurnResponse, singularity_model::ProviderCallError> {
        use singularity_model::{
            ProviderApiProtocol, ProviderAttemptEvent, ProviderAttemptOccurrence,
            ProviderAttemptStarted, ProviderAttemptStatus,
        };

        let input = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == singularity_model::ModelRole::User)
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let panic_requested = input == "panic-provider";
        let protocol = ProviderApiProtocol::Chat;
        record_attempt(ProviderAttemptEvent::Started(ProviderAttemptStarted {
            provider_name: "blocking".into(),
            model_name: "blocking-model".into(),
            actual_api_protocol: protocol,
        }))?;
        self.started.send(input).expect("report request");
        assert!(!panic_requested, "injected provider panic");
        self.release.lock().expect("release lock").recv().ok();
        let error = if cancellation.is_cancelled() {
            Some(ProviderError::new(
                ModelErrorKind::Cancelled,
                "cancelled by test",
            ))
        } else {
            None
        };
        record_attempt(ProviderAttemptEvent::Finished(Box::new(
            ProviderAttemptOccurrence {
                provider_name: "blocking".into(),
                model_name: "blocking-model".into(),
                actual_api_protocol: protocol,
                terminal_status: if error.is_some() {
                    ProviderAttemptStatus::Cancelled
                } else {
                    ProviderAttemptStatus::Ok
                },
                attempt_duration_ms: 0,
                error_category: error.as_ref().map(ProviderError::category),
                diagnostic_code: error.as_ref().and_then(|error| error.code.clone()),
                retry_after_ms: None,
                retry_after_source: None,
                usage: None,
            },
        )))?;
        if let Some(error) = error {
            return Err(error.into());
        }
        on_event(ProviderStreamEvent::OutputTextDelta { delta: "do".into() });
        for index in 0..self.deltas {
            on_event(ProviderStreamEvent::OutputTextDelta {
                delta: format!("{index} "),
            });
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
        deltas: 0,
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
    // 被拒绝的输入不进入执行，草稿由浏览器自己保存：错误只说明原因与恢复方式。
    assert!(matches!(duplicate, Err(ref error) if error.code == RpcErrorCode::SessionBusy));

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
        deltas: 0,
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

    // 在浏览器读取之前启动时，必须冻结两个外部 turn。
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let reservation = slot.conversation.reserve_start().unwrap();
    {
        let history = host.freeze_history(&slot).unwrap();
        let mut state = slot.lock_state();
        host.begin_turn_locked(&mut state, history);
        host.publish_session_locked(&id, &slot, &mut state);
    }
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
fn running_chain_keeps_the_catalog_summary_current_and_the_read_page_frozen() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let provider = Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 0,
    });
    let fixture = fixture(provider);
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.history.summary.thread_id;
    let before = host.bootstrap().unwrap();
    assert!(
        before.sessions_by_workspace[&workspace.workspace_id][0]
            .title
            .is_none()
    );

    host.submit(&workspace.workspace_id, &id, "first input".to_string())
        .unwrap();
    // 提供方被调用说明首条用户消息已经耐久；冻结页在此之前建立。
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "first input"
    );

    // 运行期间目录回答当前事实：首条用户标题立即可见，阶段来自 Conversation。
    let running = host.bootstrap().unwrap();
    let listed = &running.sessions_by_workspace[&workspace.workspace_id][0];
    assert!(
        listed
            .title
            .as_deref()
            .is_some_and(|title| title.starts_with("first")),
        "expected the durable first-user title, got {:?}",
        listed.title
    );
    assert_eq!(running.session_phases[&id], SessionPhase::Running);

    // 同一时刻内容恢复仍是冻结页＋活动事件，不提前重复本轮内容。
    let read = host
        .read_session(&workspace.workspace_id, &id, 40, None)
        .unwrap();
    assert!(read.history.turns.iter().all(|turn| turn.turn_id.is_none()));
    assert_ne!(read.runtime.phase, SessionPhase::Idle);
    assert!(!read.active_events.is_empty());

    release_tx.send(()).unwrap();
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));

    // 结算后目录与内容恢复都回到耐久事实，用户消息只出现一次。
    let settled = host.bootstrap().unwrap();
    assert_eq!(
        settled.sessions_by_workspace[&workspace.workspace_id][0].turn_count,
        1
    );
    let read = host
        .read_session(&workspace.workspace_id, &id, 40, None)
        .unwrap();
    let messages: Vec<_> = read
        .history
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            HistoryItem::Message { role, text, .. } if role == "user" => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec!["first input"]);
}

#[test]
fn send_now_waits_for_workbench_settlement_and_keeps_the_pending_input() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 0,
    }));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.history.summary.thread_id;
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut reservation = slot.conversation.reserve_start().unwrap();
    {
        let history = host.freeze_history(&slot).unwrap();
        let mut state = slot.lock_state();
        host.begin_turn_locked(&mut state, history);
        host.publish_session_locked(&id, &slot, &mut state);
    }
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
    let pending = slot.conversation.snapshot().pending_controls[0].clone();
    host.abort(&workspace.workspace_id, &id).unwrap();
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();

    // 唯一的 runtime 预留会保留到投影结算完成。
    let rejected = host.queue_send_now(&workspace.workspace_id, &id, &pending.control_id);
    assert!(matches!(rejected, Err(error) if error.code == RpcErrorCode::SessionBusy));
    assert_eq!(
        slot.conversation.snapshot().pending_controls,
        vec![pending.clone()]
    );
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
fn automatic_follow_up_start_publishes_queue_state_and_compacts_finished_progress() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 0,
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
    {
        let history = host.freeze_history(&slot).unwrap();
        let mut state = slot.lock_state();
        host.begin_turn_locked(&mut state, history);
        host.publish_session_locked(&id, &slot, &mut state);
    }
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

    let snapshot = slot.runtime_from(&slot.lock_state());
    assert!(snapshot.pending_controls.is_empty());
    let frames: Vec<_> = std::iter::from_fn(|| stream.try_recv().ok()).collect();
    let published = frames.iter().any(|frame| {
        matches!(&frame.event, StreamEvent::SessionChanged { payload, .. }
        if payload.pending_controls.is_empty())
    });
    assert!(
        published,
        "the consumed queue state is published while the next turn runs"
    );
    assert!(frames.iter().any(|frame| matches!(
        &frame.event,
        StreamEvent::TurnEvent { payload, .. }
            if matches!(&payload.event, TurnEvent::AssistantDelta { delta, .. } if delta == "do")
    )), "live clients receive incremental progress");
    {
        let state = slot.lock_state();
        let events = &state.active_turn.as_ref().unwrap().events;
        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                TurnEvent::ItemCompleted {
                    content: Some(HistoryItem::Message { text, .. }), ..
                } if text == "done"
            )),
            "recovery includes the full completed content"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event.event,
                TurnEvent::AssistantDelta { .. } | TurnEvent::ItemStarted { .. }
            )),
            "completed content replaces its buffered progress"
        );
    }

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
    // 让 catalog 扫描失败（对文件调用 read_dir），使下一次完整
    // 快照无法构建，而变更本身仍停留在本地。
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
            .join(singularity_agent::session::session_file_name(&id)),
    )
    .unwrap();
    host.on_session_settled(&id, &slot, turn_terminal(Ok(outcome)), reservation);
    assert_eq!(
        slot.runtime_from(&slot.lock_state())
            .terminal
            .expect("terminal")
            .status,
        TurnStatus::Completed,
        "the trusted terminal survives a broken history read"
    );
    assert!(
        host.read_session(&workspace.workspace_id, &id, 40, None)
            .is_err(),
        "the read-side failure stays visible through the session read path"
    );
    assert_eq!(
        slot.runtime_from(&slot.lock_state())
            .terminal
            .expect("terminal")
            .status,
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

/// 取任务目录只是查询：既不为拿 cwd 恢复会话并创建 Conversation，也不写日志。
#[test]
fn session_directory_reads_cwd_without_opening_a_conversation() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let thread = host
        .catalog
        .create_thread(&workspace.root, None)
        .expect("create thread");
    let file =
        fixture
            ._home
            .path()
            .join("sessions")
            .join(singularity_agent::session::session_file_name(
                &thread.thread_id,
            ));
    let durable_before = std::fs::read(&file).expect("session file");

    assert_eq!(
        host.session_directory(&workspace.workspace_id, &thread.thread_id)
            .expect("cwd query"),
        thread.cwd
    );
    assert!(
        host.lock_sessions().is_empty(),
        "a cwd query never creates a Conversation slot"
    );
    assert_eq!(
        std::fs::read(&file).expect("session file"),
        durable_before,
        "a cwd query never writes the session log"
    );
}

#[cfg(windows)]
#[test]
fn provider_save_publishes_once_and_reports_a_retryable_credential_failure() {
    use singularity_protocol::ModelConfigurationInput;
    use std::os::windows::fs::OpenOptionsExt;

    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let provider = ProviderConfigurationInput {
        provider_id: "combined".into(),
        display_name: None,
        base_url: "https://example.invalid/v1".into(),
        models: vec![ModelConfigurationInput {
            model_id: "model".into(),
            display_name: None,
            api_protocol: Some("chat".into()),
            max_context_tokens: Some(128_000),
            max_output_tokens: Some(8192),
            reasoning_variants: Vec::new(),
            default_variant: None,
            thinking_wire_format: None,
            chat_output_tokens_field: None,
        }],
    };
    let auth_guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x00000001 | 0x00000002)
        .open(fixture._home.path().join("auth.json"))
        .unwrap();
    let mut stream = host.subscribe();
    let error = host
        .save_provider(provider.clone(), Some("synthetic-key"))
        .unwrap_err();
    assert_eq!(error.code, RpcErrorCode::ConfigurationPartiallySaved);
    let StreamEvent::WorkbenchChanged { payload } = stream.try_recv().unwrap().event else {
        panic!("expected the final catalog snapshot");
    };
    assert!(
        payload
            .model_catalog
            .providers
            .iter()
            .any(|entry| { entry.provider_id == "combined" && !entry.credential_configured })
    );
    assert!(
        stream.try_recv().is_err(),
        "one publication per save action"
    );
    assert!(
        host.runner
            .validate_model_selector(Some("combined/model"))
            .is_err()
    );

    drop(auth_guard);
    // 该命令不返回 payload；结果由发布的快照和读取路径承载。
    host.save_provider(provider, Some("synthetic-key")).unwrap();
    let catalog = host.lock_models().redacted_catalog();
    assert!(
        catalog
            .providers
            .iter()
            .any(|entry| { entry.provider_id == "combined" && entry.credential_configured })
    );
    host.runner
        .validate_model_selector(Some("combined/model"))
        .unwrap();
    assert!(matches!(
        stream.try_recv().unwrap().event,
        StreamEvent::WorkbenchChanged { .. }
    ));
    assert!(
        stream.try_recv().is_err(),
        "retry also publishes only the final snapshot"
    );
    assert!(
        !serde_json::to_string(&catalog)
            .unwrap()
            .contains("synthetic-key")
    );
}

/// 冷路径读盘期间启动的回合必须保留：读取不得把新投影清掉，否则后续
/// assistant 增量就再也追加不到活动回合上。
///
/// 交错由注入点与 barrier 控制：读取在锁外取得快照后停住，回合在这期间建立
/// 活动投影并投递事件，然后放行读取。第二次取样时 slot 已有冻结 history，
/// 因此只需一次加锁即完成捕获，不再经过注入点。
#[test]
fn a_cold_read_never_erases_an_active_turn_started_during_its_history_load() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 0,
    }));
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

    let (entered_tx, entered_rx) = channel();
    let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
    let reader_entered = Arc::clone(&entered);
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    // 注入点取值即被取走，只会拦下这一次冷读取。
    *host.read_capture_pause.lock().unwrap() = Some(Arc::new(move || {
        let _ = entered_tx.send(());
        reader_entered.wait();
    }));
    let reader = {
        let host = Arc::clone(host);
        let workspace_id = workspace.workspace_id.clone();
        let id = id.clone();
        std::thread::spawn(move || host.read_session(&workspace_id, &id, 100, None))
    };
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reader reached the pause point");
    host.submit(&workspace.workspace_id, &id, "first input".to_string())
        .unwrap();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "first input"
    );
    assert!(
        slot.lock_state().active_turn.is_some(),
        "the worker has published the active turn projection"
    );
    entered.wait();
    let snapshot = reader.join().unwrap().expect("consistent read");

    assert!(
        snapshot.runtime.active_turn.is_some(),
        "a read must not clear the active turn it found"
    );
    assert_eq!(snapshot.runtime.phase, SessionPhase::Running);
    assert!(
        snapshot.runtime.session_revision > 0,
        "the read reports the projection revision it captured"
    );
    assert!(
        snapshot
            .active_events
            .iter()
            .any(|event| matches!(&event.event, TurnEvent::TurnStarted { .. })),
        "the captured active events belong to the running turn"
    );
    assert_eq!(
        snapshot
            .history
            .turns
            .iter()
            .filter(|turn| turn.turn_id.is_some())
            .count(),
        0,
        "live content stays out of durable history until settlement"
    );

    release_tx.send(()).unwrap();
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));
    let settled = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    assert_eq!(settled.runtime.phase, SessionPhase::Idle);
    assert_eq!(
        settled
            .history
            .turns
            .iter()
            .flat_map(|turn| &turn.items)
            .filter(|item| matches!(item, HistoryItem::Message { role, .. } if role == "user"))
            .count(),
        1,
        "the user message appears once after settlement"
    );
}

/// 冷路径读盘期间完成的回合必须让该次读取重新取样：读取在回合开始前读到的空
/// history 不能配上回合完成后的终态与 sessionRevision——前端按 revision 接纳，
/// 认不出「版本新、内容旧」的结果。
#[test]
fn a_cold_read_resamples_when_the_turn_settles_during_its_history_load() {
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

    // 读取读到的是「回合开始前」的空历史；它停在注入点上，而回合在此期间完成
    // 持久化并结算。旧实现会把这份空 history 与结算后的运行态拼成同一响应。
    let (entered_tx, entered_rx) = channel();
    let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
    let reader_entered = Arc::clone(&entered);
    *host.read_capture_pause.lock().unwrap() = Some(Arc::new(move || {
        let _ = entered_tx.send(());
        reader_entered.wait();
    }));
    let reader = {
        let host = Arc::clone(host);
        let workspace_id = workspace.workspace_id;
        let id = id.clone();
        std::thread::spawn(move || host.read_session(&workspace_id, &id, 100, None))
    };
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("reader reached the pause point");
    let mut reservation = slot.conversation.reserve_start().unwrap();
    let outcome = reservation.run("durable input", &mut |_event| {}).unwrap();
    host.on_session_settled(&id, &slot, turn_terminal(Ok(outcome)), reservation);
    entered.wait();

    let snapshot = reader.join().unwrap().expect("consistent read");
    assert!(
        snapshot.runtime.terminal.is_some(),
        "the settled terminal is part of the captured projection"
    );
    assert!(snapshot.runtime.active_turn.is_none());
    let user_messages: Vec<_> = snapshot
        .history
        .turns
        .iter()
        .flat_map(|turn| &turn.items)
        .filter_map(|item| match item {
            HistoryItem::Message { role, text, .. } if role == "user" => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_messages,
        vec!["durable input"],
        "a settled read must not pair a pre-turn history page with the post-settle runtime"
    );
}

/// 冻结 history 期间的读取是热路径：它必须在一次持锁内完成，因此即使增量在
/// 持续发布也不消耗冷读重试预算，更不会返回 busy。每次返回的三项还必须属于
/// 同一捕获，不能拼出「事件来自更晚的 revision」这类自相矛盾的快照。
///
/// 这条路径的竞争频率本身不需要证明（报告 S01 已说明）：断言的是不变量——
/// 投影持续变化期间，每一次读取都成功，且 history/runtime/events 同源。
#[test]
fn a_frozen_history_read_never_reports_contention_while_deltas_stream() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    // 放行后持续发布增量：让回合保持运行，并不断推进 session_revision，
    // 使热路径读取真正与事件发布并发。
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 20_000,
    }));
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

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let host = Arc::clone(host);
        let workspace_id = workspace.workspace_id.clone();
        let id = id.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut reads = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let snapshot = host
                    .read_session(&workspace_id, &id, 100, None)
                    .expect("a hot-path read never reports contention");
                if let Some(active) = &snapshot.runtime.active_turn {
                    assert!(
                        snapshot
                            .active_events
                            .iter()
                            .all(|event| match &event.event {
                                TurnEvent::TurnStarted { turn, .. } =>
                                    turn.turn_id == active.turn_id,
                                _ => true,
                            }),
                        "captured events belong to the captured turn"
                    );
                    assert!(
                        snapshot.active_events.iter().all(|event| {
                            event.session_revision <= snapshot.runtime.session_revision
                        }),
                        "captured events never come from a later revision than the runtime"
                    );
                }
                reads += 1;
                // 不额外放缓读取节奏：这个用例要的正是读者与投影发布持续竞争。
                std::thread::yield_now();
            }
            reads
        })
    };

    host.submit(&workspace.workspace_id, &id, "first input".to_string())
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the provider reached the model");
    release_tx.send(()).unwrap();
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        reader.join().unwrap() > 0,
        "the reader observed the frozen-history hot path"
    );
}

/// 未打开任务的目录读盘不占用会话 map 锁：该读盘被停住时，另一个任务的会话
/// 查找仍然完成。旧实现把 map 锁跨在这次读盘上，第二个查询只能等读盘结束。
#[test]
fn an_unopened_task_directory_read_does_not_hold_the_session_map_lock() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.workbench;
    let Workspace {
        workspace_id, root, ..
    } = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let opened = host
        .catalog
        .create_thread(&root, None)
        .expect("create opened thread");
    let unopened = host
        .catalog
        .create_thread(&root, None)
        .expect("create unopened thread");
    // 只打开其中一个任务：另一个在会话 map 里没有 slot，查询走目录摘要读盘。
    host.open_slot(&workspace_id, &opened.thread_id).unwrap();

    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel();
    let release = Arc::new(Mutex::new(release_rx));
    *host.directory_read_pause.lock().unwrap() = Some(Arc::new(move || {
        let _ = entered_tx.send(());
        let _ = release.lock().expect("release lock").recv();
    }));
    let reader = {
        let host = Arc::clone(host);
        let workspace_id = workspace_id.clone();
        let unopened = unopened.thread_id;
        std::thread::spawn(move || host.session_directory(&workspace_id, &unopened))
    };
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the reader reached the directory load");

    // 读盘仍停住：另一个任务的会话查找必须已经能完成，因此它不依赖这次读盘结束。
    let (done_tx, done_rx) = channel();
    let opened = opened.thread_id;
    {
        let host = Arc::clone(host);
        std::thread::spawn(move || {
            let _ = done_tx.send(host.session_directory(&workspace_id, &opened));
        });
    }
    let looked_up = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("a second session lookup must not wait for the blocked directory read");
    assert!(looked_up.is_ok());
    release_tx.send(()).unwrap();
    assert!(reader.join().unwrap().is_ok());
}

/// 冷路径读盘失败仍是可诊断的读取错误，且不改变活动投影。
#[test]
fn a_failed_history_load_stays_a_read_error_and_leaves_the_projection_alone() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
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
    let before = slot.lock_state().session_revision;
    std::fs::remove_file(
        fixture
            ._home
            .path()
            .join("sessions")
            .join(singularity_agent::session::session_file_name(&id)),
    )
    .unwrap();
    assert!(
        host.read_session(&workspace.workspace_id, &id, 100, None)
            .is_err(),
        "a broken history file is reported through the read path"
    );
    let state = slot.lock_state();
    assert_eq!(
        state.session_revision, before,
        "a failed read is not a lifecycle transition"
    );
    assert!(state.active_turn.is_none());
    assert!(state.terminal.is_none());
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
    let models = Arc::new(Mutex::new(ModelConfigOwner::open(
        home.path().to_path_buf(),
        runtime.handle().clone(),
    )));
    let runner = Arc::new(
        TurnRunner::new(home.path().join("sessions"), Arc::clone(&models))
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
