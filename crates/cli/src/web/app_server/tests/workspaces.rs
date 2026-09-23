use super::*;

/// 快照发布失败不得推翻已提交的操作：工作区仍然存在，RPC 返回成功，
/// 读侧恢复经重同步通道表达。
#[test]
fn snapshot_failure_does_not_fail_a_committed_mutation() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.app_server;
    let first = fixture
        .app_server
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
    host.publish_app_snapshot();
    let frame = receiver.try_recv().unwrap();
    assert!(matches!(frame.event, StreamEvent::AppChanged { .. }));
}

#[test]
fn unopened_history_does_not_block_removing_a_project() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::new([]),
    ));
    let host = &fixture.app_server;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    host.catalog.create_thread(&workspace.root, None).unwrap();
    host.remove_workspace(&workspace.workspace_id).unwrap();
}

/// 冷打开（未打开任务）的归属校验发生在恢复写入之前：传错工作区时，即使该
/// 会话文件需要尾部修复，也必须原样保留；所属工作区仍能正常恢复。
#[test]
fn foreign_workspace_open_leaves_the_session_file_untouched() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("unused"),
    ));
    let host = &fixture.app_server;
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
    let host = &fixture.app_server;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .unwrap();
    let created = host.create_session(&workspace.workspace_id).unwrap();
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

/// 归属由会话持久化的规范 cwd 决定：嵌套项目各自成组，registry 不缓存关系。
#[test]
fn workspace_grouping_is_recomputed_from_exact_canonical_thread_cwd() {
    let fixture = fixture(Arc::new(
        singularity_model::test_support::ScriptedProvider::ok("done"),
    ));
    let host = &fixture.app_server;
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
        .create_session(&outer_workspace.workspace_id)
        .expect("outer thread")
        .history
        .summary
        .thread_id;
    let nested_thread = host
        .create_session(&nested_workspace.workspace_id)
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
    let host = &fixture.app_server;
    let workspace = host
        .add_workspace(&fixture.workspace.path().to_string_lossy())
        .expect("add workspace");
    for _ in 0..3 {
        host.create_session(&workspace.workspace_id)
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
