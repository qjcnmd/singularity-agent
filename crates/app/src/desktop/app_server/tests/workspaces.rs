use super::*;

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
