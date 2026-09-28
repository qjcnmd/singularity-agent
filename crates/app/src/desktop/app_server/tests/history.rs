use super::*;

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
        release: Arc::new(Mutex::new(release_rx)),
        deltas: 20_000,
    }));
    let (host, _, id) = session_in(&fixture);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let host = Arc::clone(host);
        let id = id.clone();
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut reads = 0usize;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let snapshot = host
                    .read_session(&id, 100, None)
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

    host.submit(&id, "first input".to_string()).unwrap();
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the provider reached the model");
    release_tx.send(()).unwrap();
    wait_for_idle(host, std::slice::from_ref(&id));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(
        reader.join().unwrap() > 0,
        "the reader observed the frozen-history hot path"
    );
}

/// 目录始终读取完整记录，未闭合尾行不影响已有任务的列出。
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
    let writer = singularity_agent::session::SessionManager::open_existing_with_access(
        &path,
        &fixture._sessions.coordinator,
        &id,
        singularity_agent::session::SessionAccess::Append,
    )
    .expect("active writer");
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
    drop(writer);
    assert!(host.bootstrap().is_ok());

    // 同项目里再建一个从未被读过的会话并撕裂尾部，同样可以读取完整记录。
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
    let bootstrap = host.bootstrap().unwrap();
    assert!(
        bootstrap.sessions_by_workspace[&workspace.workspace_id]
            .iter()
            .any(|session| session.thread_id == unknown.thread_id)
    );
}
