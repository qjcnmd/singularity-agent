use super::*;

#[test]
fn send_now_waits_for_app_settlement_and_keeps_the_pending_input() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let fixture = fixture(Arc::new(BlockingProvider {
        started: started_tx,
        release: Mutex::new(release_rx),
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
    let (host, workspace, id) = session_in(&fixture);

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
    let (host, workspace, id) = session_in(&fixture);
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
