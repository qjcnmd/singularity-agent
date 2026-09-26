use super::*;

#[test]
fn send_now_waits_for_app_settlement_and_keeps_the_pending_input() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Arc::new(Mutex::new(release_rx)),
        deltas: 0,
    }));
    let (host, workspace, id) = session_in(&fixture);
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
            let result = singularity_runtime::test_support::run_async(
                reservation.run("first", &mut |event| {
                    event_host.on_turn_event(&event_id, &event_slot, event)
                }),
            );
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
