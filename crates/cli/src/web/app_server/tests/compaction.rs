use super::*;
use singularity_runtime::test_support::seed_compaction_history;

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
