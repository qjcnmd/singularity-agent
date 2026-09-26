use super::*;
use singularity_runtime::test_support::seed_compaction_history;

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
