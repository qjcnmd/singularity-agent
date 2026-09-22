use super::*;

#[test]
fn idle_reads_and_new_chains_use_the_latest_durable_history() {
    let provider = Arc::new(singularity_model::test_support::ScriptedProvider::new([
        singularity_model::test_support::ScriptedAttempt::success("first"),
        singularity_model::test_support::ScriptedAttempt::success("second"),
    ]));
    let fixture = fixture(provider);
    let host = &fixture.app_server;
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
    let host = &fixture.app_server;
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

/// 取任务目录只是查询：既不为拿 cwd 恢复会话并创建 Conversation，也不写日志。
#[test]
fn session_directory_reads_cwd_without_opening_a_conversation() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.app_server;
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

/// 冷读取整段持有 slot 锁：读盘期间占用该锁的写者（回合开始与结算都在同一把
/// 锁内提交）无法插进读盘与捕获之间，因此「读盘期间插进一个回合」这个交错在
/// 结构上不存在。这里用「占住锁 → 读取不得完成」把这条不变量钉在实现上。
#[test]
fn a_cold_read_holds_the_slot_lock_for_its_whole_history_load() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let (host, workspace, id) = session_in(&fixture);
    let slot = host.open_slot(&workspace.workspace_id, &id).unwrap();

    let guard = slot.lock_state();
    let (entered_tx, entered_rx) = channel();
    let (done_tx, done_rx) = channel();
    let reader = {
        let host = Arc::clone(host);
        let workspace_id = workspace.workspace_id;
        std::thread::spawn(move || {
            let _ = entered_tx.send(());
            let result = host.read_session(&workspace_id, &id, 100, None);
            let _ = done_tx.send(());
            result
        })
    };
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the reader thread is running");
    assert!(
        done_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a read must not complete while the slot lock is held"
    );

    drop(guard);
    let snapshot = reader.join().unwrap().expect("consistent read");
    assert_eq!(snapshot.runtime.phase, SessionPhase::Idle);
    assert!(snapshot.runtime.active_turn.is_none());
}

/// 冻结 history 期间的读取是热路径：一次持锁内完成，因此增量持续发布时每次
/// 返回的三项仍属于同一捕获，不会拼出「事件来自更晚的 revision」这类自相矛盾的
/// 快照。断言的是不变量，不是竞争频率。
#[test]
fn a_frozen_history_read_stays_consistent_while_deltas_stream() {
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
                    .expect("a read never fails on a live session");
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
    let host = &fixture.app_server;
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
