use super::*;

/// 宿主故障后的输入交还沿用正常失败路径的规则：本轮已接受但未交付的输入按
/// 接受序号回到队列，界面不以“仍在运行”悬挂；槽位结算后同一会话仍可开始下一轮。
#[test]
fn worker_panic_settles_the_slot_and_allows_another_turn() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Arc::new(Mutex::new(release_rx)),
        deltas: 0,
    }));
    let (_, workspace, id) = session_in(&fixture);
    fixture
        .app_server
        .submit(&workspace.workspace_id, &id, "panic-provider".to_string())
        .expect("submit");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("started");
    release_tx.send(()).expect("release into the panic");
    wait_for_idle(&fixture.app_server, &workspace, std::slice::from_ref(&id));
    let snapshot = fixture
        .app_server
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
        .app_server
        .submit(&workspace.workspace_id, &id, "retry".to_string())
        .expect("next submit");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("next started");
    release_tx.send(()).expect("release");
    wait_for_idle(&fixture.app_server, &workspace, std::slice::from_ref(&id));

    // 第二轮：故障时已接受但未交付的 steer 回到队列，槽位不留在“仍在运行”。
    fixture
        .app_server
        .submit(&workspace.workspace_id, &id, "panic-provider".to_string())
        .expect("submit again");
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("started again");
    fixture
        .app_server
        .steer(&workspace.workspace_id, &id, "late input".to_string())
        .expect("a running turn accepts a steer");
    release_tx.send(()).expect("release into the panic");
    wait_for_idle(&fixture.app_server, &workspace, std::slice::from_ref(&id));
    let snapshot = fixture
        .app_server
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
        release: Arc::new(Mutex::new(release_rx)),
        deltas: 1,
    }));
    let (host, workspace, id) = session_in(&fixture);
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
    let outcome =
        singularity_runtime::test_support::run_async(reservation.run("first", &mut |_event| {}))
            .unwrap();
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
