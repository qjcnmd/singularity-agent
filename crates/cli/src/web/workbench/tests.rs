#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use singularity_agent::session::test_support::WorkspaceFixture;
use singularity_core::CancellationToken;
use singularity_model::{
    ModelErrorKind, ModelTurnRequest, ModelTurnResponse, Provider, ProviderError,
    ProviderStreamEvent,
};
use singularity_protocol::{HistoryItem, RpcErrorCode, StreamEvent, Workspace};
use singularity_runtime::test_support::SessionsFixture;

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
        // 释放信号决定本次 attempt 何时结束；panic 场景也需要它，测试才能先
        // 交付已接受的控制输入，再确定性地观察 panic 之后的交还。
        self.release.lock().expect("release lock").recv().ok();
        assert!(!panic_requested, "injected provider panic");
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

/// 宿主故障后的输入交还沿用正常失败路径的规则：本轮已接受但未交付的输入按
/// 接受序号回到队列，界面不以“仍在运行”悬挂；槽位结算后同一会话仍可开始下一轮。
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
    release_tx.send(()).expect("release into the panic");
    wait_for_idle(&fixture.workbench, &workspace, std::slice::from_ref(&id));
    let snapshot = fixture
        .workbench
        .read_session(&workspace.workspace_id, &id, 100, None)
        .expect("settled snapshot");
    let terminal = snapshot.runtime.terminal.expect("terminal");
    assert_eq!(terminal.status, TurnStatus::Failed);
    assert!(
        terminal
            .message
            .expect("message")
            .contains("injected provider panic"),
        "the host failure keeps its real reason"
    );
    fixture
        .workbench
        .submit(&workspace.workspace_id, &id, "retry".to_string())
        .expect("next submit");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("next started");
    release_tx.send(()).expect("release");
    wait_for_idle(&fixture.workbench, &workspace, std::slice::from_ref(&id));

    // 第二轮：故障时已接受但未交付的 steer 回到队列，槽位不留在“仍在运行”。
    fixture
        .workbench
        .submit(&workspace.workspace_id, &id, "panic-provider".to_string())
        .expect("submit again");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("started again");
    fixture
        .workbench
        .steer(&workspace.workspace_id, &id, "late input".to_string())
        .expect("a running turn accepts a steer");
    release_tx.send(()).expect("release into the panic");
    wait_for_idle(&fixture.workbench, &workspace, std::slice::from_ref(&id));
    let snapshot = fixture
        .workbench
        .read_session(&workspace.workspace_id, &id, 100, None)
        .expect("settled snapshot");
    assert!(snapshot.runtime.active_turn.is_none());
    assert_eq!(snapshot.runtime.phase, SessionPhase::Idle);
    assert_eq!(
        snapshot
            .runtime
            .pending_controls
            .iter()
            .map(|control| control.text.clone())
            .collect::<Vec<_>>(),
        vec!["late input".to_string()],
        "an accepted but undelivered input returns to the queue"
    );
}

/// 结算路径本身因共享状态中毒而无法发布时，界面不留在“仍在运行”：按既有
/// 重同步通道要求客户端重拉基线，不伪造终态；执行窗口与输入仍按既有规则归还。
#[test]
fn a_settle_that_cannot_publish_requires_resync_instead_of_hanging() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 1,
    }));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("workspace");
    let id = host
        .create_session(&workspace.workspace_id, None)
        .expect("session")
        .history
        .summary
        .thread_id;
    let mut events = host.subscribe();
    host.submit(&workspace.workspace_id, &id, "input".to_string())
        .expect("submit");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("started");
    let slot = host.open_slot(&workspace.workspace_id, &id).expect("slot");
    // 回合事件需要 slot 锁：注入的 panic 发生在持有该锁的路径上。
    slot.poison_state();
    release_tx.send(()).expect("release");

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut resynced = false;
    while Instant::now() < deadline && !resynced {
        match events.try_recv() {
            Ok(envelope) => {
                if let StreamEvent::ResyncRequired { .. } = envelope.event {
                    resynced = true;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    assert!(
        resynced,
        "a settle that cannot publish must ask the client to resync"
    );
    // 投影无法发布，但执行窗口确实归还：中毒的 slot 不再占用该会话。
    assert_eq!(slot.conversation().phase(), SessionPhase::Idle);
}

/// worker 未启动是普通可报告的启动错误：三类入口都不残留活动投影，输入保留，
/// RPC 不声称接受执行，预订也一并归还。
#[test]
fn a_failed_worker_start_reports_the_error_and_returns_the_projection() {
    for entry in ["submit", "send_now", "compact"] {
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let provider: Arc<dyn Provider + Send + Sync> = if entry == "send_now" {
            Arc::new(BlockingProvider {
                started: started_tx,
                release: Mutex::new(release_rx),
                deltas: 0,
            })
        } else {
            Arc::new(singularity_model::test_support::ScriptedProvider::ok(
                "unused",
            ))
        };
        let fixture = fixture(provider);
        let host = &fixture.workbench;
        let workspace = host
            .add_workspace(&fixture.workspace.path().to_string_lossy())
            .expect("workspace");
        let id = host
            .create_session(&workspace.workspace_id, None)
            .expect("session")
            .history
            .summary
            .thread_id;
        if entry == "send_now" {
            // 空闲会话不能直接排队后续输入：先在一次运行中的回合里排队，再用
            // 已接受的停止让它留在队列里。
            host.submit(&workspace.workspace_id, &id, "first".to_string())
                .expect("submit");
            started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("started");
            host.follow_up(&workspace.workspace_id, &id, "queued".to_string())
                .expect("queue a follow-up");
            host.abort(&workspace.workspace_id, &id).expect("stop");
            release_tx.send(()).expect("release");
            wait_for_idle(host, &workspace, std::slice::from_ref(&id));
            assert_eq!(
                host.read_session(&workspace.workspace_id, &id, 100, None)
                    .expect("queued snapshot")
                    .runtime
                    .pending_controls
                    .len(),
                1,
                "the accepted stop leaves the queued input in place"
            );
        }
        host.fail_next_spawn("no threads available");
        let error = match entry {
            "submit" => host.submit(&workspace.workspace_id, &id, "input".to_string()),
            "send_now" => host.queue_send_now(&workspace.workspace_id, &id, None),
            _ => host.compact(&workspace.workspace_id, &id),
        }
        .expect_err("a worker that never started must be reported");
        assert_eq!(error.code, RpcErrorCode::Internal, "{entry}");
        assert!(
            error.message.contains("无法启动任务执行线程"),
            "{entry}: {}",
            error.message
        );

        let snapshot = host
            .read_session(&workspace.workspace_id, &id, 100, None)
            .expect("snapshot after a failed start");
        assert_eq!(snapshot.runtime.phase, SessionPhase::Idle, "{entry}");
        assert!(snapshot.runtime.active_turn.is_none(), "{entry}");
        assert!(snapshot.runtime.active_compaction.is_none(), "{entry}");
        assert!(snapshot.runtime.terminal.is_none(), "{entry}");
        assert_eq!(
            snapshot.runtime.pending_controls.len(),
            usize::from(entry == "send_now"),
            "{entry}: a promoted input returns to the queue"
        );

        // 预订确实归还：同一会话可以立刻再预订一次。
        let slot = host.open_slot(&workspace.workspace_id, &id).expect("slot");
        let reservation = slot
            .conversation()
            .reserve_start()
            .expect("the reservation was released");
        drop(reservation);
    }
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
        host.catalog.resume_thread(&id, &workspace.root).unwrap(),
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
    let reservation = slot.conversation().reserve_start().unwrap();
    {
        let history = host.read_persisted_history(&slot).unwrap();
        let mut state = slot.lock_state();
        state.begin_turn(history);
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
    let mut reservation = slot.conversation().reserve_start().unwrap();
    {
        let history = host.read_persisted_history(&slot).unwrap();
        let mut state = slot.lock_state();
        state.begin_turn(history);
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
    let pending = slot.conversation().snapshot().pending_controls[0].clone();
    host.abort(&workspace.workspace_id, &id).unwrap();
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();

    // 唯一的 runtime 预留会保留到投影结算完成。
    let rejected = host.queue_send_now(&workspace.workspace_id, &id, Some(&pending.control_id));
    assert!(matches!(rejected, Err(error) if error.code == RpcErrorCode::SessionBusy));
    assert_eq!(
        slot.conversation().snapshot().pending_controls,
        vec![pending.clone()]
    );
    assert_eq!(slot.conversation().phase(), SessionPhase::Reserved);
    host.on_session_settled(&id, &slot, Some(turn_terminal(outcome)), reservation);
    host.queue_send_now(&workspace.workspace_id, &id, Some(&pending.control_id))
        .unwrap();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "next"
    );
    release_tx.send(()).unwrap();
    wait_for_idle(host, &workspace, &[id]);
}

/// 批量“全部立即发送”是一次 RPC：目标集合由队列 owner 在当前队列上读取。
/// 空队列安全结束，指定不存在的单条仍报原来的错误，整批在一次交接内进入活动
/// turn 的下一份请求。
#[test]
fn sending_the_whole_queue_is_one_operation_and_an_empty_queue_is_a_no_op() {
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

    // 空闲且队列为空：批量操作不报错，也不产生交接。
    host.queue_send_now(&workspace.workspace_id, &id, None)
        .expect("an empty queue is a no-op, not a failure");
    // 单条目标不存在仍是原来的错误分类。
    let missing = host
        .queue_send_now(&workspace.workspace_id, &id, Some("missing-control"))
        .unwrap_err();
    assert_eq!(missing.code, RpcErrorCode::ControlNotFound);

    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut reservation = slot.conversation().reserve_start().unwrap();
    {
        let history = host.read_persisted_history(&slot).unwrap();
        let mut state = slot.lock_state();
        state.begin_turn(history);
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
    host.follow_up(&workspace.workspace_id, &id, "queued one".into())
        .unwrap();
    host.follow_up(&workspace.workspace_id, &id, "queued two".into())
        .unwrap();
    assert_eq!(slot.conversation().snapshot().pending_controls.len(), 2);

    // 一次调用把整批交给活动 turn：前端不再逐条请求，也不会按过期快照重复请求。
    host.queue_send_now(&workspace.workspace_id, &id, None)
        .expect("the whole queue is sent in one operation");
    assert!(
        slot.conversation().snapshot().pending_controls.is_empty(),
        "one batch call drains the whole pending queue"
    );
    let snapshot = host
        .read_session(&workspace.workspace_id, &id, 20, None)
        .unwrap();
    assert!(snapshot.runtime.pending_controls.is_empty());

    // 放行本轮第一份响应后，注入的整批在同一次交接里进入下一份请求：队列末条
    // 就是这次请求的最后一条输入。
    release_tx.send(()).unwrap();
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        "queued two"
    );
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();
    host.on_session_settled(&id, &slot, Some(turn_terminal(outcome)), reservation);
    wait_for_idle(host, &workspace, &[id]);
}

/// 立即发送先决定动作，再按动作需要校验：现轮注入与空队列 no-op 不受未来
/// 模型配置影响，只有真正要启动新轮的预留分支才解析未来 selector；解析失败
/// 时已提升的输入按原接受序回到队列，不会丢失。
#[test]
fn send_now_decides_the_action_before_validating_the_next_turn_model() {
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
    let mut reservation = slot.conversation().reserve_start().unwrap();
    {
        let history = host.read_persisted_history(&slot).unwrap();
        let mut state = slot.lock_state();
        state.begin_turn(history);
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
    host.follow_up(&workspace.workspace_id, &id, "queued".into())
        .unwrap();
    let pending = slot.conversation().snapshot().pending_controls[0].clone();

    // 破坏未来模型配置：当前轮已经冻结自己的配置，未来的 selector 不再可解析。
    host.remove_provider("openai_compatible").unwrap();

    // 现轮注入与空队列 no-op 都不需要未来的模型快照。
    host.queue_send_now(&workspace.workspace_id, &id, Some(&pending.control_id))
        .expect("injecting into the running turn does not need the next turn's model");
    assert!(slot.conversation().snapshot().pending_controls.is_empty());
    host.queue_send_now(&workspace.workspace_id, &id, None)
        .expect("an empty queue is a no-op even when the future selector is broken");

    // 结束本轮，队列里再留一条输入等待下一轮。
    host.follow_up(&workspace.workspace_id, &id, "second".into())
        .unwrap();
    host.abort(&workspace.workspace_id, &id).unwrap();
    release_tx.send(()).unwrap();
    let (outcome, reservation) = worker.join().unwrap();
    host.on_session_settled(&id, &slot, Some(turn_terminal(outcome)), reservation);
    let queued = slot.conversation().snapshot().pending_controls;
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].text, "second");

    // 真正要启动新轮：未来 selector 解析失败，输入必须留在队列里。
    let error = host
        .queue_send_now(&workspace.workspace_id, &id, Some(&queued[0].control_id))
        .expect_err("starting a new turn needs a resolvable model selector");
    assert_eq!(error.code, RpcErrorCode::ConfigurationInvalid);
    let after = slot.conversation().snapshot().pending_controls;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].control_id, queued[0].control_id);
    assert_eq!(slot.conversation().phase(), SessionPhase::Idle);
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
    let mut reservation = slot.conversation().reserve_start().unwrap();
    {
        let history = host.read_persisted_history(&slot).unwrap();
        let mut state = slot.lock_state();
        state.begin_turn(history);
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
        let events = state.active_events();
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
    host.on_session_settled(&id, &slot, Some(turn_terminal(outcome)), reservation);
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
    let sessions = fixture._sessions.home().join("sessions");
    std::fs::remove_dir_all(&sessions).unwrap();
    std::fs::write(&sessions, b"not a directory").unwrap();
    let second_root = fixture._sessions.home().join("second-workspace");
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
    let (host, workspace, id) = session_in(&fixture);
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let mut reservation = slot.conversation().reserve_start().unwrap();
    let outcome = reservation.run("first", &mut |_event| {}).unwrap();
    assert_eq!(outcome.turn_status, TurnStatus::Completed);
    std::fs::remove_file(
        fixture
            ._sessions
            .home()
            .join("sessions")
            .join(singularity_agent::session::session_file_name(&id)),
    )
    .unwrap();
    host.on_session_settled(&id, &slot, Some(turn_terminal(Ok(outcome))), reservation);
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
    let file = fixture._sessions.home().join("sessions").join(
        singularity_agent::session::session_file_name(&thread.thread_id),
    );
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
        .open(fixture._sessions.home().join("auth.json"))
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
        host.validate_model_selector(Some("combined/model"))
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
    host.validate_model_selector(Some("combined/model"))
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
    let (host, workspace, id) = session_in(&fixture);

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
        slot.lock_state().has_active_turn(),
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
    let (host, workspace, id) = session_in(&fixture);
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
    let mut reservation = slot.conversation().reserve_start().unwrap();
    let outcome = reservation.run("durable input", &mut |_event| {}).unwrap();
    host.on_session_settled(&id, &slot, Some(turn_terminal(Ok(outcome))), reservation);
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
    let (host, workspace, id) = session_in(&fixture);

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
    let (host, workspace, id) = session_in(&fixture);
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();
    let before = slot.lock_state().revision();
    std::fs::remove_file(
        fixture
            ._sessions
            .home()
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
        state.revision(),
        before,
        "a failed read is not a lifecycle transition"
    );
    assert!(!state.has_active_turn());
    assert!(slot.runtime_from(&state).terminal.is_none());
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
    host.validate_model_selector(Some(selector)).unwrap();
    let auth_path = fixture._sessions.home().join("auth.json");
    let auth_guard = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x00000001 | 0x00000002)
        .open(&auth_path)
        .unwrap();
    host.remove_provider("openai_compatible")
        .expect_err("credential file cannot be replaced");
    assert!(host.validate_model_selector(Some(selector)).is_err());
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

/// 冷打开（未打开任务）的归属校验发生在恢复写入之前：传错工作区时，即使该
/// 会话文件需要尾部修复，也必须原样保留；所属工作区仍能正常恢复。
#[test]
fn foreign_workspace_open_leaves_the_session_file_untouched() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("unused"),
    ));
    let host = &fixture.workbench;
    let owner = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    // 直接建 thread 而不经 create_session：这里要的正是未打开任务（冷路径），
    // 工作台里不能已经有它的 slot 与冻结历史。
    let thread = host
        .catalog
        .create_thread(&owner.root, None)
        .expect("create thread");
    let id = thread.thread_id;

    // 半条 JSON 结尾：正常恢复会截掉它并补写换行。
    let path = fixture
        ._sessions
        .home()
        .join("sessions")
        .join(format!("{id}.jsonl"));
    let mut torn = std::fs::read(&path).unwrap();
    torn.extend_from_slice(b"{\"type\":\"message\",\"id\":\"");
    std::fs::write(&path, &torn).unwrap();

    let foreign_dir = WorkspaceFixture::new();
    let foreign = host
        .add_workspace(&foreign_dir.path().to_string_lossy())
        .unwrap();
    let error = host
        .read_session(&foreign.workspace_id, &id, 20, None)
        .unwrap_err();
    assert_eq!(error.code, RpcErrorCode::Conflict);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        torn,
        "拒绝的冷打开不得修复或改写目标文件"
    );

    let owned = host
        .read_session(&owner.workspace_id, &id, 20, None)
        .expect("the owning workspace still repairs and opens the session");
    assert_eq!(owned.history.summary.thread_id, id);
    assert!(
        std::fs::read(&path).unwrap().ends_with(b"\n"),
        "the owning workspace repairs the torn tail"
    );
}

/// 归档与压缩共用的占用判断读取同一份待处理集合：一条留在队列里的普通提交
/// （启动写者失败后归还）也算占用，不会被当成空闲会话。
#[test]
fn a_queued_submission_occupies_the_session_for_archive_and_compaction() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("done"),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id, None).unwrap();
    let id = created.history.summary.thread_id;
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();

    // 同一会话已有一个存活写者：普通提交在打开写者时失败，输入因此留在
    // 待处理队列里（这正是「启动写者失败」这条控制链）。
    let session_path = fixture._sessions.dir.join(format!("{id}.jsonl"));
    let held = singularity_agent::session::SessionManager::open_existing_with_access(
        &session_path,
        &fixture._sessions.coordinator,
        singularity_agent::session::ExpectedSession { id: &id, cwd: None },
        singularity_agent::session::SessionAccess::Append,
    )
    .expect("hold the session writer");
    assert!(
        slot.conversation()
            .run_turn("queued submission", &mut |_| {})
            .is_err(),
        "the session already has a live writer"
    );
    drop(held);
    let queued = slot.conversation().snapshot().pending_controls;
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0].channel,
        singularity_protocol::ControlChannel::Submit
    );

    assert_eq!(
        host.archive_session(&workspace.workspace_id, &id)
            .unwrap_err()
            .code,
        RpcErrorCode::SessionBusy,
        "a queued submission occupies the session"
    );
    assert_eq!(
        host.compact(&workspace.workspace_id, &id).unwrap_err().code,
        RpcErrorCode::SessionBusy,
        "compaction reads the same pending set"
    );
    assert_eq!(
        host.remove_workspace(&workspace.workspace_id)
            .unwrap_err()
            .code,
        RpcErrorCode::WorkspaceBusy,
        "the same pending set blocks removing the project"
    );

    // 按同一身份撤回后会话恢复空闲，归档成功。
    host.queue_withdraw(&workspace.workspace_id, &id, &queued[0].control_id)
        .unwrap();
    host.archive_session(&workspace.workspace_id, &id)
        .expect("the session is free once the queue is empty");
}

/// 手动压缩的失败终态落盘后必须完整进入冷读公开投影：slot 重建（宿主重启
/// 等价物）后的读取与热读给出同一操作反馈，前一个 Run 的完成状态不被改写。
#[test]
fn a_failed_manual_compaction_stays_visible_after_the_slot_is_rebuilt() {
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};

    let fixture = fixture(Arc::new(ScriptedProvider::new([
        ScriptedAttempt::success("done"),
        // 摘要校验失败：空正文不是可用的检查点，operation 终态为 Failed。
        ScriptedAttempt::success(""),
    ])));
    let (host, workspace, id) = session_in(&fixture);
    // 先完成一个 Run：它的完成状态必须留在自己的轮次里。
    host.submit(&workspace.workspace_id, &id, "first turn".to_string())
        .unwrap();
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));
    seed_compaction_history(&fixture._sessions.dir, &id);

    host.compact(&workspace.workspace_id, &id)
        .expect("the compaction operation is accepted");
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));
    let hot = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    let hot_terminal = hot
        .runtime
        .terminal
        .expect("the failure is visible while the slot is alive");
    assert_eq!(hot_terminal.status, TurnStatus::Failed);

    // slot 重建后的冷读：反馈来自同一份持久账本。
    host.lock_sessions().remove(&id);
    let cold = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    assert_eq!(
        cold.runtime.terminal,
        Some(hot_terminal),
        "the rebuilt slot recovers the same operation feedback"
    );
    let runs: Vec<_> = cold
        .history
        .turns
        .iter()
        .filter(|turn| turn.turn_id.is_some())
        .collect();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].status,
        Some(TurnStatus::Completed),
        "the previous run keeps its own completion state"
    );
}

/// 手动压缩在 Agent 已成功、提交边界尚未冻结时接受停止：调用结果、持久日志与
/// 公开终态消费同一次冻结事实，slot 重建后的冷读给出同一反馈。
#[test]
fn a_compaction_stopped_at_its_commit_boundary_settles_as_interrupted() {
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};

    let fixture = fixture(Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "summary text",
    )])));
    let (host, workspace, id) = session_in(&fixture);
    seed_compaction_history(&fixture._sessions.dir, &id);

    // 确定性停在「Agent 已成功、提交边界尚未冻结」这一刻，此时接受停止。
    let (reached_tx, reached_rx) = channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    {
        let host = Arc::clone(host);
        let runner = Arc::clone(&host.runner);
        let workspace_id = workspace.workspace_id.clone();
        let session_id = id.clone();
        let boundary_release = Arc::clone(&release);
        runner.pause_next_compaction_commit(Arc::new(move || {
            let _ = reached_tx.send(());
            boundary_release.wait();
            host.abort(&workspace_id, &session_id)
                .expect("the stop is accepted before the boundary freezes");
        }));
    }
    host.compact(&workspace.workspace_id, &id)
        .expect("the compaction operation is accepted");
    reached_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("compaction reaches its commit boundary");
    release.wait();
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));

    let hot = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    let hot_terminal = hot.runtime.terminal.expect("the interruption is visible");
    assert_eq!(hot_terminal.status, TurnStatus::Interrupted);
    assert_eq!(
        hot_terminal.message, None,
        "an accepted stop carries no generic cancellation text"
    );
    assert_eq!(
        compaction_terminals(&fixture._sessions.dir, &id),
        vec![(TurnStatus::Interrupted, true)],
        "the durable terminal consumes the same frozen fact as the call result"
    );

    host.lock_sessions().remove(&id);
    let cold = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    assert_eq!(cold.runtime.terminal, Some(hot_terminal));
}

/// 手动压缩没有可替换内容时是正常结果：运行态给出无消息的完成终态（界面照常
/// 显示“没有可压缩的内容”），它不是失败，slot 重建后的冷读从同一份账本得出同
/// 一条反馈。
#[test]
fn a_manual_compaction_without_compaction_content_reports_a_normal_outcome() {
    use singularity_model::test_support::ScriptedProvider;

    let fixture = fixture(Arc::new(ScriptedProvider::ok("done")));
    // 全新任务没有可摘要的历史：手动压缩不发送请求，也不写任何压缩条目。
    let (host, workspace, id) = session_in(&fixture);

    host.compact(&workspace.workspace_id, &id)
        .expect("the compaction operation is accepted");
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));
    let hot = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    let terminal = hot
        .runtime
        .terminal
        .expect("the no-op outcome is visible while the slot is alive");
    assert_eq!(terminal.source, SessionTerminalSource::Compaction);
    assert_eq!(terminal.status, TurnStatus::Completed);
    assert_eq!(terminal.message, None);
    assert_eq!(
        compaction_terminals(&fixture._sessions.dir, &id),
        vec![(TurnStatus::Completed, false)],
        "the no-op operation still closes its durable operation"
    );

    // slot 重建后的冷读：同一条反馈来自账本里「完成但没有落盘压缩条目」。
    host.lock_sessions().remove(&id);
    let cold = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    assert_eq!(cold.runtime.terminal, Some(terminal));
}

/// 成功压缩不产生终态反馈：摘要条目本身就是那条反馈，冷读也不能把它误判成
/// “没有可压缩的内容”。
#[test]
fn a_successful_manual_compaction_leaves_no_terminal_feedback() {
    use singularity_model::test_support::{ScriptedAttempt, ScriptedProvider};
    use singularity_protocol::HistoryItem;

    let fixture = fixture(Arc::new(ScriptedProvider::new([ScriptedAttempt::success(
        "summary text",
    )])));
    let (host, workspace, id) = session_in(&fixture);
    seed_compaction_history(&fixture._sessions.dir, &id);

    host.compact(&workspace.workspace_id, &id)
        .expect("the compaction operation is accepted");
    wait_for_idle(host, &workspace, std::slice::from_ref(&id));
    let hot = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    assert!(
        hot.runtime.terminal.is_none(),
        "a reduced compaction reports through its summary item, not a terminal"
    );
    assert!(
        hot.history
            .turns
            .iter()
            .flat_map(|turn| turn.items.iter())
            .any(|item| matches!(item, HistoryItem::Compaction { .. })),
        "the summary item is the durable feedback"
    );

    host.lock_sessions().remove(&id);
    let cold = host
        .read_session(&workspace.workspace_id, &id, 100, None)
        .unwrap();
    assert!(
        cold.runtime.terminal.is_none(),
        "the cold read must not turn a reduced compaction into a no-op notice"
    );
}

/// 为手动压缩准备非空历史前缀；摘要校验失败的用例需要可被替换的内容。
fn seed_compaction_history(sessions_dir: &std::path::Path, thread_id: &str) {
    use singularity_agent::message::{AgentMessage, ContentBlock};
    use singularity_agent::session::SessionManager;

    let path = sessions_dir.join(singularity_agent::session::session_file_name(thread_id));
    let mut session = SessionManager::open_existing(&path).expect("open session");
    for (user, text) in [
        (true, "first user ".repeat(5_000)),
        (false, "first assistant ".repeat(5_000)),
        (true, "recent user ".repeat(5_000)),
        (false, "recent assistant ".repeat(5_000)),
    ] {
        let content = vec![ContentBlock::Text { text }];
        let message = if user {
            AgentMessage::User { content }
        } else {
            AgentMessage::Assistant {
                content,
                stop_reason: None,
                provider_reasoning_replay: None,
            }
        };
        session.append_message(message).expect("append history");
    }
}

/// 独立压缩（无 turn 绑定）的持久终态：日志、调用结果与公开反馈的唯一来源。
fn compaction_terminals(
    sessions_dir: &std::path::Path,
    thread_id: &str,
) -> Vec<(TurnStatus, bool)> {
    use singularity_agent::session::{LedgerRecord, SessionData};

    SessionData::open(&sessions_dir.join(singularity_agent::session::session_file_name(thread_id)))
        .expect("reopen session")
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            singularity_agent::session::SessionEntry::Record {
                record:
                    LedgerRecord::OperationFinished {
                        turn_id: None,
                        outcome,
                        user_stopped,
                        ..
                    },
                ..
            } => Some((*outcome, *user_stopped)),
            _ => None,
        })
        .collect()
}

/// 归档的占用检查与持久变更在同一段生命周期交接内：检查之后启动的提交必须
/// 等待，不能插进「尚无磁盘写者」的窗口；归档成功后旧 slot 不接受工作。
#[test]
fn a_submission_cannot_slip_between_the_occupancy_check_and_the_archive() {
    use singularity_model::test_support::ScriptedProvider;

    let fixture = fixture(Arc::new(ScriptedProvider::ok("done")));
    let (host, workspace, id) = session_in(&fixture);

    // 归档线程在占用检查之后、持久变更之前停下；这一刻仍持有生命周期临界区。
    let (checked_tx, checked_rx) = channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    {
        let boundary_release = Arc::clone(&release);
        *host.archive_check_pause.lock().unwrap() = Some(Arc::new(move || {
            let _ = checked_tx.send(());
            boundary_release.wait();
        }));
    }
    let archiver = {
        let host = Arc::clone(host);
        let workspace_id = workspace.workspace_id.clone();
        let session_id = id.clone();
        std::thread::spawn(move || host.archive_session(&workspace_id, &session_id))
    };
    checked_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the archive reached its occupancy check");

    let (submitted_tx, submitted_rx) = channel();
    let submitter = {
        let host = Arc::clone(host);
        let workspace_id = workspace.workspace_id.clone();
        let session_id = id.clone();
        std::thread::spawn(move || {
            let result = host.submit(&workspace_id, &session_id, "late".to_string());
            let _ = submitted_tx.send(result);
        })
    };
    assert!(
        matches!(
            submitted_rx.recv_timeout(Duration::from_millis(300)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "a submission started after the check waits for the same lifecycle handoff"
    );

    release.wait();
    archiver
        .join()
        .unwrap()
        .expect("the archive succeeds once the window is closed");
    let error = submitted_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the submission reports its outcome")
        .unwrap_err();
    assert_eq!(
        error.code,
        RpcErrorCode::SessionNotFound,
        "the stale slot never accepts work after the archive"
    );
    submitter.join().unwrap();
    assert!(
        host.lock_sessions().get(&id).is_none(),
        "an archived session leaves no slot behind"
    );
    // 归档后同一身份不接受任何工作：提交与整理都只得到「不存在」。
    assert_eq!(
        host.compact(&workspace.workspace_id, &id).unwrap_err().code,
        RpcErrorCode::SessionNotFound
    );
}

/// 工作区移除的占用依据是已登记 slot 的运行状态：同一项目里另一份不可读的
/// 会话不会把项目级占用判断变成内部错误。
#[test]
fn removing_a_project_reads_occupancy_from_registered_slots() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
        deltas: 0,
    }));
    let (host, workspace, running) = session_in(&fixture);
    // 同项目里再建一个会话并让它的文件尾部撕裂：本进程从未读过它，目录枚举
    // 因此不再可信，但项目占用判断不依赖那份枚举。
    let broken = host
        .catalog
        .create_thread(&workspace.root, None)
        .expect("second session");
    let broken_path = fixture
        ._sessions
        .dir
        .join(singularity_agent::session::session_file_name(
            &broken.thread_id,
        ));
    let mut bytes = std::fs::read(&broken_path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&broken_path, bytes).expect("torn tail");

    host.submit(&workspace.workspace_id, &running, "go".to_string())
        .unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the turn reaches the provider");
    assert_eq!(
        host.remove_workspace(&workspace.workspace_id)
            .unwrap_err()
            .code,
        RpcErrorCode::WorkspaceBusy,
        "a running turn occupies its project"
    );
    release_tx.send(()).unwrap();
    wait_for_idle(host, &workspace, std::slice::from_ref(&running));
}

/// 活动日志的尾部尚未稳定时，工作台目录快照仍包含该会话：读失败不被当成
/// 删除，选中状态不会因此被清空；没有可信旧摘要的另一份会话则让整次快照
/// 明确失败，而不是返回缺项的成功快照。
#[test]
fn a_bootstrap_during_an_unstable_log_tail_keeps_the_session_listed() {
    use singularity_model::test_support::ScriptedProvider;

    let fixture = fixture(Arc::new(ScriptedProvider::ok("done")));
    let (host, workspace, id) = session_in(&fixture);
    // 创建时已成功读过一次：目录缓存持有该会话的已提交摘要。
    let path = fixture
        ._sessions
        .dir
        .join(singularity_agent::session::session_file_name(&id));
    let mut bytes = std::fs::read(&path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&path, bytes).expect("torn tail");

    let bootstrap = host.bootstrap().expect("the directory read stays usable");
    assert!(
        bootstrap.sessions_by_workspace[&workspace.workspace_id]
            .iter()
            .any(|session| session.thread_id == id),
        "a read failure is not a deletion: the session stays in the directory"
    );

    // 同项目里再建一个从未被读过的会话并撕裂尾部：没有可信旧摘要时，快照
    // 明确失败，绝不返回「看似完整却缺项」的成功结果。
    let unknown = host
        .catalog
        .create_thread(&workspace.root, None)
        .expect("second session");
    let unknown_path = fixture
        ._sessions
        .dir
        .join(singularity_agent::session::session_file_name(
            &unknown.thread_id,
        ));
    let mut bytes = std::fs::read(&unknown_path).expect("session file");
    bytes.extend_from_slice(br#"{"id":"half-written","timestamp":"#);
    std::fs::write(&unknown_path, bytes).expect("torn tail");
    assert_eq!(
        host.bootstrap().unwrap_err().code,
        RpcErrorCode::Internal,
        "an untrustworthy directory read is reported, not silently incomplete"
    );
}

struct Fixture {
    _sessions: SessionsFixture,
    _runtime: tokio::runtime::Runtime,
    workspace: WorkspaceFixture,
    workbench: Arc<Workbench>,
}

fn fixture(provider: Arc<dyn Provider + Send + Sync>) -> Fixture {
    let sessions = SessionsFixture::new();
    singularity_runtime::test_support::write_provider_fixture(sessions.home(), "chosen-model");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let models = Arc::new(Mutex::new(ModelConfigOwner::open(
        sessions.home().to_path_buf(),
    )));
    let runner = TurnRunner::new(
        sessions.dir.clone(),
        Arc::clone(&models),
        Arc::clone(&sessions.coordinator),
        runtime.handle().clone(),
    )
    .with_provider_override(provider);
    let catalog = sessions.catalog();
    let workspaces = WorkspaceStore::open(sessions.home()).expect("workspace store");
    let workbench = Workbench::new(
        Arc::new(runner),
        catalog,
        workspaces,
        models,
        sessions.home().to_path_buf(),
    );
    Fixture {
        _sessions: sessions,
        _runtime: runtime,
        workspace: WorkspaceFixture::new(),
        workbench,
    }
}

/// 这些用例共用的最小前置：登记一个项目并新建一个未命名任务。
fn session_in(fixture: &Fixture) -> (&Arc<Workbench>, Workspace, String) {
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
    (host, workspace, id)
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

/// 归属由会话持久化的规范 cwd 决定：嵌套项目各自成组，registry 不缓存关系。
#[test]
fn workspace_grouping_is_recomputed_from_exact_canonical_thread_cwd() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("done"),
    ));
    let host = &fixture.workbench;
    let outer = tempfile::tempdir().expect("outer workspace");
    let nested = outer.path().join("nested");
    std::fs::create_dir(&nested).expect("nested workspace");
    let outer_workspace = host
        .add_workspace(&outer.path().to_string_lossy())
        .expect("add outer");
    let nested_workspace = host
        .add_workspace(&nested.to_string_lossy())
        .expect("add nested");
    let outer_thread = host
        .create_session(&outer_workspace.workspace_id, None)
        .expect("outer thread")
        .history
        .summary
        .thread_id;
    let nested_thread = host
        .create_session(&nested_workspace.workspace_id, None)
        .expect("nested thread")
        .history
        .summary
        .thread_id;

    let grouped = host.bootstrap().expect("bootstrap").sessions_by_workspace;
    assert_eq!(
        grouped[&outer_workspace.workspace_id][0].thread_id,
        outer_thread
    );
    assert_eq!(
        grouped[&nested_workspace.workspace_id][0].thread_id,
        nested_thread
    );
    assert_eq!(grouped[&outer_workspace.workspace_id].len(), 1);
    assert_eq!(grouped[&nested_workspace.workspace_id].len(), 1);
}

/// 分组保持目录顺序：同一项目内的任务与目录列表顺序逐项一致。
#[test]
fn grouping_preserves_catalog_order() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("done"),
    ));
    let host = &fixture.workbench;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("add workspace");
    for _ in 0..3 {
        host.create_session(&workspace.workspace_id, None)
            .expect("create session");
    }
    let listed: Vec<String> = host
        .catalog
        .list_threads()
        .expect("threads")
        .into_iter()
        .filter(|thread| thread.cwd == workspace.root)
        .map(|thread| thread.thread_id)
        .collect();
    let grouped: Vec<String> = host.bootstrap().expect("bootstrap").sessions_by_workspace
        [&workspace.workspace_id]
        .iter()
        .map(|thread| thread.thread_id.clone())
        .collect();
    assert_eq!(grouped, listed, "grouping keeps catalog order");
}
