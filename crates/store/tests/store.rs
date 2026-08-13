//! 验证 SessionStore 的 schema、绑定、恢复、历史与事务不变量。

use schemars::schema_for;
use singularity_protocol::{ItemKind, ThreadStatus, TurnStatus};
use singularity_store::{
    CommitTurnOutcomeParams, ConversationRole, SessionStore, SessionStoreDescriptor, StoreError,
    TurnOutcomeAuthority,
};
use std::sync::{Arc, Barrier};

// 原子 start：turn 与 user input 在同一事务创建；重复 start 在已有非终态 turn 时被拒绝。
#[test]
fn turn_start_with_input_is_atomic_and_single_owner() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let input = serde_json::json!([{"type": "text", "text": "hello"}]);
    let (turn, item) = store
        .create_turn_with_input(&thread.thread_id, "running", input.clone())
        .expect("started turn");

    assert_eq!(turn.status, TurnStatus::Running);
    assert_eq!(item.kind, ItemKind::UserMessage);
    assert_eq!(
        store
            .get_turn_user_input(&turn.turn_id)
            .expect("user input"),
        input
    );
    // 同一 thread 已有非终态 turn：重复 start 必须拒绝，而不是幂等覆盖。
    assert!(matches!(
        store.create_turn_with_input(&thread.thread_id, "running", input),
        Err(StoreError::WorkspaceHasNonterminalTurn { .. })
    ));
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Running
    );
}

// 终态提交在 SQLite 重开后仍然持久化。
#[test]
fn turn_outcome_persists_across_store_reopen() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "hello"}]),
        )
        .expect("started turn");
    drop(store);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("done"),
            },
        )
        .expect("terminal outcome");
    assert_eq!(committed.turn.status, TurnStatus::Completed);
    let history = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("history");
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].content, "hello");
    assert_eq!(history.messages[1].content, "done");
}

#[test]
fn sqlite_store_persists_threads_turns_and_items() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let descriptor = store.descriptor();

    assert_eq!(descriptor.backend, "sqlite");
    assert_eq!(descriptor.schema_version, 13);
    assert_eq!(
        store.applied_migrations().expect("migrations"),
        vec![
            "0001_initial_session_store".to_string(),
            "0002_durable_ledger".to_string(),
            "0004_pending_tool_calls".to_string(),
            "0005_store_hardening".to_string(),
            "0006_conversation_history".to_string(),
            "0007_pending_execution_state".to_string(),
            "0008_approval_execution_recovery".to_string(),
            "0009_thread_policy_snapshot".to_string(),
            "0010_stable_enum_text".to_string(),
            "0011_typed_permission_resources".to_string(),
            "0012_typed_trace_spans".to_string(),
            "0013_turn_resume_checkpoints".to_string()
        ]
    );
    assert_eq!(
        schema_for!(SessionStoreDescriptor)
            .schema
            .metadata
            .unwrap()
            .title
            .unwrap(),
        "SessionStoreDescriptor"
    );

    let thread = store
        .create_thread(Some("gpt-test"), Some("C:/repo"))
        .expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    let item = store
        .append_item(
            &turn.turn_id,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "hello"}]),
        )
        .expect("item");

    let connection =
        rusqlite::Connection::open(dir.path().join("sessions.sqlite3")).expect("open sqlite");
    let thread_binding: (String, String, String) = connection
        .query_row(
            "select thread_id, model, cwd from threads where thread_id = ?1",
            [&thread.thread_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("thread binding projection");
    assert_eq!(
        thread_binding,
        (
            thread.thread_id.clone(),
            "gpt-test".to_string(),
            "C:/repo".to_string()
        )
    );

    assert_eq!(item.kind, ItemKind::UserMessage);
}

// 验证 schema 元数据和 SQLite WAL pragma 在新库中正确写入。
#[test]
fn sqlite_store_writes_schema_meta_and_uses_wal_journal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");

    let store = SessionStore::open(&db_path).expect("open store");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let schema_version: u32 = connection
        .query_row("select schema_version from schema_meta", [], |row| {
            row.get(0)
        })
        .expect("schema version");
    let journal_mode: String = connection
        .query_row("pragma journal_mode", [], |row| row.get(0))
        .expect("journal mode");

    assert_eq!(schema_version, 13);
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
}

// 验证未来 schema 版本会被 fail closed 拒绝打开。
#[test]
fn sqlite_store_rejects_future_schema_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "
            create table schema_meta(schema_version integer not null);
            insert into schema_meta(schema_version) values(999);
            ",
        )
        .expect("future schema");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::UnsupportedSchema {
            found: 999,
            supported: 13
        })
    ));
}

#[test]
fn pause_request_blocks_terminal_commit_until_cleared() {
    let store = SessionStore::open(":memory:").expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "original"}]),
        )
        .expect("started turn");

    store
        .request_turn_pause(&turn.turn_id)
        .expect("pause requested");
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Failed,
                agent_loop_status: "failed",
                assistant_item_id: None,
                assistant_delta: None,
            },
        ),
        Err(StoreError::TurnBoundaryPending { .. })
    ));
    assert_eq!(
        store.get_turn(&turn.turn_id).expect("running turn").status,
        TurnStatus::Running
    );

    // 转 suspended 后 request_turn_pause 清除 pause_requested，终态提交恢复可用。
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspended turn");
    store
        .request_turn_pause(&turn.turn_id)
        .expect("pause cleared");
    store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "cancelled",
                assistant_item_id: None,
                assistant_delta: None,
            },
        )
        .expect("terminal commit after pause cleared");
}



// trusted reopen 仍拒绝已初始化数据库的 marker/结构分裂。
#[test]
fn trusted_reopen_validates_current_markers_before_serving_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "delete from schema_migrations where migration_id = '0010_stable_enum_text'",
            [],
        )
        .expect("remove marker");
    drop(connection);

    assert!(matches!(
        store.trusted_reopen(),
        Err(StoreError::InvalidState(message)) if message.contains("migration markers")
    ));
}

#[cfg(unix)]
#[test]
fn trusted_reopen_rejects_a_hard_linked_store_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let hard_link = dir.path().join("sessions-alias.sqlite3");
    std::fs::hard_link(&db_path, &hard_link).expect("hard link store");

    assert!(matches!(
        store.trusted_reopen(),
        Err(StoreError::InvalidState(message)) if message.contains("hard links")
    ));
}

#[cfg(unix)]
#[test]
fn trusted_reopen_rejects_path_identity_replacement() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let replacement = dir.path().join("replacement.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let replacement_store = SessionStore::open(&replacement).expect("open replacement");
    drop(replacement_store);
    let original = dir.path().join("original.sqlite3");
    std::fs::rename(&db_path, &original).expect("move original");
    std::fs::rename(&replacement, &db_path).expect("replace store path");

    assert!(matches!(
        store.trusted_reopen(),
        Err(StoreError::InvalidState(message)) if message.contains("identity")
    ));
}

#[cfg(unix)]
#[test]
fn store_open_rejects_final_and_parent_symlinks() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().expect("temp dir");
    let real_dir = dir.path().join("real");
    std::fs::create_dir(&real_dir).expect("real dir");
    let real_db = real_dir.join("sessions.sqlite3");
    drop(SessionStore::open(&real_db).expect("create real store"));

    let file_link = dir.path().join("sessions-link.sqlite3");
    symlink(&real_db, &file_link).expect("file symlink");
    assert!(matches!(
        SessionStore::open(&file_link),
        Err(StoreError::InvalidState(message)) if message.contains("without following links")
    ));

    let directory_link = dir.path().join("real-link");
    symlink(&real_dir, &directory_link).expect("directory symlink");
    assert!(matches!(
        SessionStore::open(directory_link.join("sessions.sqlite3")),
        Err(StoreError::InvalidState(message)) if message.contains("without following links")
    ));
}

#[cfg(windows)]
#[test]
fn trusted_reopen_keeps_windows_store_path_non_replaceable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let replacement = dir.path().join("replacement.sqlite3");
    let replacement_store = SessionStore::open(&replacement).expect("open replacement");
    drop(replacement_store);

    assert!(std::fs::rename(&db_path, dir.path().join("original.sqlite3")).is_err());
    store
        .trusted_reopen()
        .expect("protected store remains usable");
}

#[cfg(windows)]
#[test]
fn store_open_rejects_parent_reparse_point_when_creation_is_available() {
    use std::os::windows::fs::symlink_dir;

    let dir = tempfile::tempdir().expect("temp dir");
    let real_dir = dir.path().join("real");
    std::fs::create_dir(&real_dir).expect("real dir");
    let real_db = real_dir.join("sessions.sqlite3");
    drop(SessionStore::open(&real_db).expect("create real store"));
    let directory_link = dir.path().join("real-link");
    if let Err(error) = symlink_dir(&real_dir, &directory_link) {
        if error.raw_os_error() == Some(1314) {
            return;
        }
        panic!("create directory symlink: {error}");
    }

    assert!(matches!(
        SessionStore::open(directory_link.join("sessions.sqlite3")),
        Err(StoreError::InvalidState(message)) if message.contains("without following links")
    ));
}
// 验证当前库缺少 migration marker 时不能被认领。
#[test]
fn missing_migration_marker_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("create current store");
    store
        .create_thread(Some("gpt-test"), Some("C:/repo"))
        .expect("create thread");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch(
            "delete from schema_migrations where migration_id = '0010_stable_enum_text';",
        )
        .expect("remove migration marker");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("migration markers")
    ));
}

// 验证 marker 或列只完成一半时，store 不会猜测并继续运行。
#[test]
fn structure_rejects_weak_check_index_and_trigger_without_mutation() {
    for corruption in ["check", "index", "trigger"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("create current store");
        store.create_thread(None, None).expect("thread");
        drop(store);

        let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
        match corruption {
            "check" => {
                let original_sql: String = connection
                    .query_row(
                        "select sql from sqlite_master where type = 'table' and name = 'threads'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("thread schema");
                let weakened_sql = original_sql.replace(
                    "check(status in ('active', 'archived'))",
                    "check(status in ('active', 'archived', 'future'))",
                );
                assert_ne!(weakened_sql, original_sql);
                connection
                    .pragma_update(None, "writable_schema", "on")
                    .expect("enable writable schema");
                connection
                    .execute(
                        "update sqlite_master set sql = ?1 where type = 'table' and name = 'threads'",
                        [&weakened_sql],
                    )
                    .expect("weaken check");
                connection
                    .pragma_update(None, "writable_schema", "off")
                    .expect("disable writable schema");
            }
            "index" => {
                connection
                    .execute("drop index turns_history_lookup", [])
                    .expect("drop canonical index");
            }
            "trigger" => {
                connection
                    .execute_batch(
                        "create trigger unexpected_trigger after insert on threads
                         begin select 1; end;",
                    )
                    .expect("create unexpected trigger");
            }
            _ => unreachable!(),
        }
        drop(connection);
        let before = sqlite_snapshot(&db_path);

        let error = match SessionStore::open(&db_path) {
            Ok(_) => panic!("corrupt current schema accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, StoreError::InvalidState(_)));
        assert_eq!(sqlite_snapshot(&db_path), before);
    }
}


// 验证新 schema 拒绝孤儿 thread/turn。
#[test]
fn missing_thread_and_turn_fail_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    assert!(matches!(
        store.create_turn("missing_thread", "running"),
        Err(StoreError::NotFound(message)) if message == "thread missing_thread"
    ));
    assert!(matches!(
        store.append_item("missing_turn", ItemKind::UserMessage, serde_json::json!({})),
        Err(StoreError::NotFound(message)) if message == "turn missing_turn"
    ));
}

// 验证终态 turn 状态与 agent_loop_status 不可被后续更新覆盖。
#[test]
fn terminal_turn_status_is_not_overwritten_by_later_updates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");

    store
        .update_turn_state(&turn.turn_id, TurnStatus::Completed, "completed")
        .expect("complete turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Completed, "completed")
        .expect("same terminal status is idempotent");

    assert!(matches!(
        store.update_turn_state(&turn.turn_id, TurnStatus::Running, "running"),
        Err(StoreError::InvalidState(message)) if message == "terminal turn status cannot be overwritten"
    ));
    assert!(matches!(
        store.update_turn_state(&turn.turn_id, TurnStatus::Completed, "failed"),
        Err(StoreError::InvalidState(message)) if message == "terminal turn agent_loop_status cannot be overwritten"
    ));
    assert_eq!(
        store
            .get_turn(&turn.turn_id)
            .expect("turn")
            .agent_loop_status,
        "completed"
    );
}
// 验证取消请求与晚到 completion 之间保持原子边界。
#[test]
fn cancellation_request_is_atomic_and_rejects_late_completion() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");

    let cancel_requested = store
        .request_turn_cancellation(&turn.turn_id)
        .expect("request cancellation");

    assert_eq!(cancel_requested.status, TurnStatus::Running);
    assert_eq!(cancel_requested.agent_loop_status, "cancel_requested");
    assert!(matches!(
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&SessionStore::allocate_assistant_item_id()),
                assistant_delta: Some("too late"),
            },
        ),
        Err(StoreError::InvalidState(message))
            if message == "cancel-requested turn can only finalize as interrupted"
    ));
    let interrupted = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "cancelled",
                assistant_item_id: None,
                assistant_delta: None,
            },
        )
        .expect("finalize cancellation");
    assert_eq!(interrupted.turn.status, TurnStatus::Interrupted);
}

// 验证 monitor 基础设施故障优先于 cancel_requested。
#[test]
fn infrastructure_failure_authority_rejects_non_failed_outcomes() {
    for (status, agent_loop_status) in [
        (TurnStatus::Interrupted, "cancelled"),
        (TurnStatus::Completed, "completed"),
    ] {
        let is_completed = status == TurnStatus::Completed;
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        store
            .request_turn_cancellation(&turn.turn_id)
            .expect("cancel request");

        let assistant_item_id = SessionStore::allocate_assistant_item_id();
        let result = store.commit_turn_outcome_with_authority(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status,
                agent_loop_status,
                assistant_item_id: is_completed.then_some(&assistant_item_id),
                assistant_delta: is_completed.then_some("late result"),
            },
            TurnOutcomeAuthority::InfrastructureFailure,
        );
        assert!(matches!(
            result,
            Err(StoreError::InvalidState(message))
                if message == "infrastructure failure can only finalize as failed"
        ));
        let current = store.get_turn(&turn.turn_id).expect("unchanged turn");
        assert_eq!(current.status, TurnStatus::Running);
        assert_eq!(current.agent_loop_status, "cancel_requested");
    }
}

// 验证 paused/suspended 无 owner turn 被 interrupt 时当场终态化。
#[test]
fn paused_and_suspended_interrupt_terminalizes_without_restart() {
    for (status, agent_loop_status) in [
        (TurnStatus::Paused, "paused"),
        (TurnStatus::Suspended, "suspended"),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("sessions.sqlite3");
        let store = SessionStore::open(&db_path).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        store
            .update_turn_state(&turn.turn_id, status, agent_loop_status)
            .expect("ownerless state");
        let interrupted = store
            .request_turn_cancellation(&turn.turn_id)
            .expect("interrupt");
        assert_eq!(interrupted.status, TurnStatus::Interrupted);
        assert_eq!(interrupted.agent_loop_status, "cancelled");
        // 未重启：当前 Store 会话即可看到终态（同一进程收敛）。
        let persisted = store.get_turn(&turn.turn_id).expect("turn");
        assert_eq!(persisted.status, TurnStatus::Interrupted);
        assert_eq!(persisted.agent_loop_status, "cancelled");
    }
}
// 验证 delete_thread 级联删除绑定行（items/turns/threads）。
#[test]
fn thread_delete_removes_bound_items_and_turns() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "delete me"}]),
        )
        .expect("turn");
    store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Interrupted,
                agent_loop_status: "interrupted",
                assistant_item_id: None,
                assistant_delta: None,
            },
        )
        .expect("terminal turn");

    store
        .delete_thread(&thread.thread_id)
        .expect("delete thread");

    let connection = rusqlite::Connection::open(&db_path).expect("reopen sqlite");
    for table in ["items", "turns", "threads"] {
        let count: i64 = connection
            .query_row(&format!("select count(*) from {table}"), [], |row| {
                row.get(0)
            })
            .expect("deleted table count");
        assert_eq!(count, 0, "{table} rows remain after thread deletion");
    }
}


// 验证 turn user input 可供 turn/resume 读取。
#[test]
fn turn_user_input_can_be_read_for_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let payload = serde_json::json!([{"type": "text", "text": "resume this turn"}]);
    let (turn, _) = store
        .create_turn_with_input(&thread.thread_id, "blocked", payload.clone())
        .expect("turn");

    assert_eq!(
        store
            .get_turn_user_input(&turn.turn_id)
            .expect("turn user input"),
        payload
    );
    assert!(matches!(
        store.get_turn_user_input("missing_turn"),
        Err(StoreError::NotFound(message)) if message == "turn user input missing_turn"
    ));
}

// 验证终态 turn 与 assistant item 在同一事务提交。
#[test]
fn terminal_turn_state_and_assistant_item_commit_atomically() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "user"}]),
        )
        .expect("turn");
    let assistant_item_id = SessionStore::allocate_assistant_item_id();

    let committed = store
        .commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&assistant_item_id),
                assistant_delta: Some("assistant"),
            },
        )
        .expect("commit terminal outcome");

    assert_eq!(committed.turn.status, TurnStatus::Completed);
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .map(|item| item.item_id.as_str()),
        Some(assistant_item_id.as_str())
    );
    assert_eq!(
        committed
            .assistant_item
            .as_ref()
            .and_then(|item| item.payload["delta"].as_str()),
        Some("assistant")
    );
    let history = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("history");
    assert_eq!(
        history
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "assistant"]
    );
}

#[test]
fn terminal_assistant_item_id_and_delta_must_be_paired() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    for (assistant_item_id, assistant_delta) in [
        (None, Some("assistant")),
        (Some(SessionStore::allocate_assistant_item_id()), None),
    ] {
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, "running")
            .expect("turn");
        let error = store
            .commit_turn_outcome(
                &turn.turn_id,
                CommitTurnOutcomeParams {
                    status: TurnStatus::Completed,
                    agent_loop_status: "completed",
                    assistant_item_id: assistant_item_id.as_ref(),
                    assistant_delta,
                },
            )
            .expect_err("unpaired assistant item must fail closed");
        assert!(matches!(
            error,
            StoreError::InvalidState(message)
                if message == "completed turn outcome requires a preallocated item ID and non-empty assistant message"
        ));
        assert_eq!(
            store.get_turn(&turn.turn_id).expect("turn").status,
            TurnStatus::Running
        );
    }
}

#[test]
fn preallocated_assistant_item_id_cannot_be_reused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let first_thread = store.create_thread(None, None).expect("first thread");
    let second_thread = store.create_thread(None, None).expect("second thread");
    let first_turn = store
        .create_turn(&first_thread.thread_id, "running")
        .expect("first turn");
    let second_turn = store
        .create_turn(&second_thread.thread_id, "running")
        .expect("second turn");
    let item_id = SessionStore::allocate_assistant_item_id();
    let commit = |turn: &singularity_protocol::Turn| {
        store.commit_turn_outcome(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: "completed",
                assistant_item_id: Some(&item_id),
                assistant_delta: Some("assistant"),
            },
        )
    };

    commit(&first_turn).expect("first commit");
    let error = commit(&second_turn).expect_err("duplicate item ID must fail closed");

    assert!(matches!(
        error,
        StoreError::InvalidState(message)
            if message == "preallocated assistant item ID is already in use"
    ));
    assert_eq!(
        store
            .get_turn(&second_turn.turn_id)
            .expect("second turn")
            .status,
        TurnStatus::Running
    );
}


// 验证当前库缺少 migration marker 时 fail closed 且不改数据。
#[test]
fn missing_migration_marker_fails_closed_without_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let (_, item) = store
        .create_turn_with_input(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "safe"}]),
        )
        .expect("turn");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "delete from schema_migrations where migration_id = '0006_conversation_history'",
            [],
        )
        .expect("delete migration marker");
    drop(connection);

    assert!(matches!(
        SessionStore::open(&db_path),
        Err(StoreError::InvalidState(message))
            if message.contains("migration markers")
    ));
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let payload: String = connection
        .query_row(
            "select payload from items where item_id = ?1",
            [&item.item_id],
            |row| row.get(0),
        )
        .expect("read unchanged item");
    assert!(payload.contains("safe"));
}
// 验证已完成 history 可持久化、排序并按 turn 分页。
#[test]
fn completed_history_is_durable_ordered_and_paged_by_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn_1 =
        append_completed_conversation(&store, &thread.thread_id, "user one", "assistant one");
    let turn_2 =
        append_completed_conversation(&store, &thread.thread_id, "user two", "assistant two");
    let turn_3 =
        append_completed_conversation(&store, &thread.thread_id, "user three", "assistant three");
    drop(store);

    let store = SessionStore::open(&db_path).expect("reopen store");
    let full = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("full history");
    assert_eq!(
        full.messages
            .iter()
            .map(|message| (
                message.turn_id.as_str(),
                message.role.clone(),
                message.content.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (turn_1.as_str(), ConversationRole::User, "user one"),
            (
                turn_1.as_str(),
                ConversationRole::Assistant,
                "assistant one"
            ),
            (turn_2.as_str(), ConversationRole::User, "user two"),
            (
                turn_2.as_str(),
                ConversationRole::Assistant,
                "assistant two"
            ),
            (turn_3.as_str(), ConversationRole::User, "user three"),
            (
                turn_3.as_str(),
                ConversationRole::Assistant,
                "assistant three"
            ),
        ]
    );
    assert_eq!(full.next_before_turn_sequence, None);

    let latest = store
        .read_thread_history(&thread.thread_id, None, 2)
        .expect("latest page");
    assert_eq!(
        latest
            .messages
            .iter()
            .map(|message| message.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            turn_2.as_str(),
            turn_2.as_str(),
            turn_3.as_str(),
            turn_3.as_str()
        ]
    );
    assert_eq!(latest.next_before_turn_sequence, Some(2));

    let earlier = store
        .read_thread_history(&thread.thread_id, latest.next_before_turn_sequence, 2)
        .expect("earlier page");
    assert_eq!(
        earlier
            .messages
            .iter()
            .map(|message| message.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![turn_1.as_str(), turn_1.as_str()]
    );
    assert_eq!(earlier.next_before_turn_sequence, None);

    let before_third = store
        .read_thread_history_before_turn(&thread.thread_id, &turn_3, 10)
        .expect("history before third turn");
    assert_eq!(
        before_third
            .messages
            .iter()
            .map(|message| message.turn_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            turn_1.as_str(),
            turn_1.as_str(),
            turn_2.as_str(),
            turn_2.as_str()
        ]
    );
}
// 验证 history 排除非 completed turn 与非 conversation item。
#[test]
fn history_excludes_non_completed_turns_and_non_conversation_items() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let completed =
        append_completed_conversation(&store, &thread.thread_id, "safe user", "safe assistant");

    for status in [
        TurnStatus::Blocked,
        TurnStatus::Failed,
        TurnStatus::Interrupted,
    ] {
        let started = store
            .create_turn_with_input_and_history(
                &thread.thread_id,
                "running",
                serde_json::json!([{"type": "text", "text": "must not replay"}]),
                10,
            )
            .expect("start non-completed turn");
        store
            .append_item(
                &started.turn.turn_id,
                ItemKind::AgentMessage,
                serde_json::json!({"delta": "must not replay"}),
            )
            .expect("assistant item");
        store
            .update_turn_status(&started.turn.turn_id, status.clone())
            .expect("terminal status");
        let history = store
            .read_thread_history(&thread.thread_id, None, 20)
            .expect("history excludes non-completed turn");
        assert_eq!(history.messages.len(), 2);
        if status == TurnStatus::Blocked {
            store
                .update_turn_status(&started.turn.turn_id, TurnStatus::Interrupted)
                .expect("release blocked fixture");
        }
    }

    let incomplete = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "incomplete completed turn"}]),
            10,
        )
        .expect("start incomplete turn");
    store
        .update_turn_status(&incomplete.turn.turn_id, TurnStatus::Completed)
        .expect("mark incomplete turn completed");
    let malformed = append_completed_conversation(
        &store,
        &thread.thread_id,
        "malformed user",
        "malformed assistant",
    );
    store
        .append_item(
            &malformed,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "orphan trailing user"}]),
        )
        .expect("orphan trailing user");

    store
        .append_item(
            &completed,
            ItemKind::Reasoning,
            serde_json::json!({"summary": "private reasoning"}),
        )
        .expect("reasoning");
    store
        .append_item(
            &completed,
            ItemKind::CommandExecution,
            serde_json::json!({"command": "secret tool metadata"}),
        )
        .expect("tool item");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values('started_agent_item', ?1, 5, ?2, '{\"delta\":\"not completed\"}', ?3, 0)",
            rusqlite::params![
                completed,
                ItemKind::AgentMessage.as_storage_text(),
                singularity_protocol::ItemStatus::Started.as_storage_text(),
            ],
        )
        .expect("started item");
    drop(connection);

    let history = store
        .read_thread_history(&thread.thread_id, None, 20)
        .expect("history");
    assert_eq!(history.messages.len(), 2);
    assert_eq!(history.messages[0].content, "safe user");
    assert_eq!(history.messages[1].content, "safe assistant");
}

// 带追加输入（steer/follow-up）的 completed turn 应完整进入下一轮历史。
#[test]
fn completed_turn_with_follow_up_messages_is_included_in_history() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn =
        append_completed_conversation(&store, &thread.thread_id, "user one", "assistant one");
    store
        .append_item(
            &turn,
            ItemKind::UserMessage,
            serde_json::json!([{"type": "text", "text": "follow-up user"}]),
        )
        .expect("follow-up user item");
    store
        .append_item(
            &turn,
            ItemKind::AgentMessage,
            serde_json::json!({"delta": "assistant two"}),
        )
        .expect("second assistant item");

    let started = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "current user"}]),
            10,
        )
        .expect("start next turn");

    assert_eq!(started.history.messages.len(), 4);
    assert_eq!(
        started
            .history
            .messages
            .iter()
            .map(|message| message.role.clone())
            .collect::<Vec<_>>(),
        vec![
            ConversationRole::User,
            ConversationRole::Assistant,
            ConversationRole::User,
            ConversationRole::Assistant,
        ]
    );
}
#[test]
fn history_decodes_all_selected_item_status_and_kind_values_before_projection() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = append_completed_conversation(&store, &thread.thread_id, "user", "assistant");
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute_batch("pragma ignore_check_constraints = on;")
        .expect("ignore checks");
    connection
        .execute(
            "update items set kind = 'unknown_kind' where turn_id = ?1 and kind = 'userMessage'",
            rusqlite::params![turn],
        )
        .expect("tamper item kind");
    drop(connection);

    assert!(matches!(
        store.read_thread_history(&thread.thread_id, None, 10),
        Err(StoreError::InvalidState(message)) if message.contains("item kind")
    ));
}

// 验证 user 与 assistant 文本存储和 history 投影都会脱敏。
#[test]
fn item_storage_and_history_redact_sensitive_user_and_assistant_text() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let raw_user_secret = "sk-abcdefghijklmnopqrstuvwxyz123456";
    let raw_assistant_secret = "Bearer abcdefghijklmnopqrstuvwxyz123456";
    let raw_tool_secret = "token=abcdefghijklmnopqrstuvwxyz123456";
    let started = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": raw_user_secret}]),
            10,
        )
        .expect("start turn");
    store
        .append_item(
            &started.turn.turn_id,
            ItemKind::AgentMessage,
            serde_json::json!({"delta": raw_assistant_secret}),
        )
        .expect("assistant item");
    store
        .append_item(
            &started.turn.turn_id,
            ItemKind::Reasoning,
            serde_json::json!({"token": raw_tool_secret}),
        )
        .expect("sensitive non-conversation item");
    store
        .update_turn_status(&started.turn.turn_id, TurnStatus::Completed)
        .expect("complete turn");

    let stored_input = store
        .get_turn_user_input(&started.turn.turn_id)
        .expect("stored input");
    assert_eq!(
        stored_input,
        serde_json::json!([{"type": "text", "text": "[redacted sensitive user input]"}])
    );
    let history = store
        .read_thread_history(&thread.thread_id, None, 10)
        .expect("history");
    assert_eq!(
        history.messages[0].content,
        "[redacted sensitive user input]"
    );
    assert!(history.messages[0].redacted);
    assert_eq!(
        history.messages[1].content,
        "[redacted sensitive assistant output]"
    );
    assert!(history.messages[1].redacted);

    drop(store);
    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let stored_items = connection
        .prepare("select payload, redacted from items order by item_sequence")
        .expect("prepare items")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .expect("query items")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect items");
    assert!(stored_items.iter().all(|(_, redacted)| *redacted));
    let serialized = serde_json::to_string(&stored_items).expect("serialize stored items");
    assert!(!serialized.contains(raw_user_secret));
    assert!(!serialized.contains(raw_assistant_secret));
    assert!(!serialized.contains(raw_tool_secret));
    assert_eq!(stored_items[2].0, "{\"redacted\":true}");
}

// 验证 malformed conversation payload 会 fail closed。
#[test]
fn malformed_conversation_payload_fails_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn_id = append_completed_conversation(&store, &thread.thread_id, "user", "assistant");

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    connection
        .execute(
            "update items set payload = '{\"delta\":42}' where turn_id = ?1 and kind = ?2",
            rusqlite::params![turn_id, ItemKind::AgentMessage.as_storage_text()],
        )
        .expect("tamper payload");

    assert!(matches!(
        store.read_thread_history(&thread.thread_id, None, 10),
        Err(StoreError::InvalidState(_))
    ));
}

// 验证 archived thread 不能启动新 turn。
#[test]
fn archived_thread_cannot_start_a_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    store
        .update_thread_status(&thread.thread_id, ThreadStatus::Archived)
        .expect("archive thread");

    assert!(matches!(
        store.create_turn(&thread.thread_id, "running"),
        Err(StoreError::InvalidState(_))
    ));
    assert!(matches!(
        store.create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "cannot start"}]),
            10,
        ),
        Err(StoreError::InvalidState(_))
    ));
}
// 验证 turn 与 item 的唯一 sequence 索引拒绝重复值。
#[test]
fn turn_and_item_sequence_unique_indexes_reject_duplicates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("turn");
    store
        .append_item(
            &turn.turn_id,
            ItemKind::Reasoning,
            serde_json::json!({"summary": "safe"}),
        )
        .expect("item");
    drop(store);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let indexes = connection
        .prepare("select name from sqlite_master where type = 'index' order by name")
        .expect("prepare indexes")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query indexes")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect indexes");
    assert!(indexes.contains(&"turns_history_lookup".to_string()));
    assert!(indexes.contains(&"items_history_lookup".to_string()));
    assert!(connection
        .execute(
            "insert into turns(turn_id, thread_id, turn_sequence, status, agent_loop_status) values('duplicate_turn', ?1, 1, 'running', 'running')",
            [&thread.thread_id],
        )
        .is_err());
    assert!(connection
        .execute(
            "insert into items(item_id, turn_id, item_sequence, kind, payload, status, redacted) values('duplicate_item', ?1, 1, 'reasoning', '{}', 'completed', 0)",
            [&turn.turn_id],
        )
        .is_err());
}

// 验证并发连接只允许一个 turn，并分配唯一 item sequence。
#[test]
fn concurrent_connections_admit_one_turn_and_allocate_unique_item_sequences() {
    const WORKERS: usize = 12;

    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let store = SessionStore::open(&db_path).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let shared_turn = store
        .create_turn(&thread.thread_id, "running")
        .expect("shared turn");
    store
        .update_turn_state(&shared_turn.turn_id, TurnStatus::Completed, "completed")
        .expect("complete shared turn");
    drop(store);

    let stores = (0..WORKERS)
        .map(|_| SessionStore::open(&db_path).expect("open concurrent store"))
        .collect::<Vec<_>>();
    let turn_barrier = Arc::new(Barrier::new(WORKERS));
    let item_barrier = Arc::new(Barrier::new(WORKERS));
    let handles = stores
        .into_iter()
        .enumerate()
        .map(|(worker, store)| {
            let thread_id = thread.thread_id.clone();
            let shared_turn_id = shared_turn.turn_id.clone();
            let turn_barrier = Arc::clone(&turn_barrier);
            let item_barrier = Arc::clone(&item_barrier);
            std::thread::spawn(move || {
                turn_barrier.wait();
                let turn = store.create_turn(&thread_id, "running");
                item_barrier.wait();
                let item = store.append_item(
                    &shared_turn_id,
                    ItemKind::Reasoning,
                    serde_json::json!({"worker": worker}),
                );
                (turn, item)
            })
        })
        .collect::<Vec<_>>();

    let mut admitted_turns = 0;
    for handle in handles {
        let (turn, item) = handle.join().expect("worker joins");
        match turn {
            Ok(_) => admitted_turns += 1,
            Err(StoreError::WorkspaceHasNonterminalTurn { .. }) => {}
            Err(error) => panic!("unexpected concurrent turn error: {error}"),
        }
        item.expect("concurrent item allocation");
    }
    assert_eq!(admitted_turns, 1);

    let connection = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let turn_sequences = connection
        .prepare("select turn_sequence from turns where thread_id = ?1 order by turn_sequence")
        .expect("prepare turns")
        .query_map([&thread.thread_id], |row| row.get::<_, u64>(0))
        .expect("query turns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect turn sequences");
    assert_eq!(turn_sequences, vec![1, 2]);

    let item_sequences = connection
        .prepare("select item_sequence from items where turn_id = ?1 order by item_sequence")
        .expect("prepare items")
        .query_map([&shared_turn.turn_id], |row| row.get::<_, u64>(0))
        .expect("query items")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect item sequences");
    assert_eq!(
        item_sequences,
        (1..=u64::try_from(WORKERS).expect("worker count")).collect::<Vec<_>>()
    );
}
// 验证共享 workspace 的执行 guard 串行化并在 owner 丢失后释放。
#[test]
fn workspace_execution_guard_serializes_shared_workspace_and_releases_after_owner_loss() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let workspace = dir.path().join("workspace");
    let other_workspace = dir.path().join("other-workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::create_dir(&other_workspace).expect("other workspace");
    let owner = SessionStore::open(&db_path).expect("owner store");
    let observer = SessionStore::open(&db_path).expect("observer store");
    let thread = owner
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("thread");
    let same_workspace_thread = owner
        .create_thread(None, Some(&workspace.to_string_lossy()))
        .expect("same workspace thread");
    let other_workspace_thread = owner
        .create_thread(None, Some(&other_workspace.to_string_lossy()))
        .expect("other workspace thread");

    let guard = owner
        .try_begin_workspace_execution(&thread.thread_id)
        .expect("acquire owner guard")
        .expect("owner guard available");
    let running = owner
        .create_turn(&thread.thread_id, "running")
        .expect("start owned turn");
    assert!(
        observer
            .try_begin_workspace_execution(&same_workspace_thread.thread_id)
            .expect("contended guard check")
            .is_none()
    );
    assert!(matches!(
        observer.create_turn(&same_workspace_thread.thread_id, "running"),
        Err(StoreError::WorkspaceHasNonterminalTurn { .. })
    ));
    let other_guard = observer
        .try_begin_workspace_execution(&other_workspace_thread.thread_id)
        .expect("other workspace guard")
        .expect("other workspace remains independent");
    drop(other_guard);

    drop(guard);
    let recovered_guard = observer
        .try_begin_workspace_execution(&same_workspace_thread.thread_id)
        .expect("recover released owner")
        .expect("released guard can be reacquired");
    let recovered = observer.get_turn(&running.turn_id).expect("recovered turn");
    assert_eq!(recovered.status, TurnStatus::Interrupted);
    assert_eq!(recovered.agent_loop_status, "interrupted");
    observer
        .create_turn(&same_workspace_thread.thread_id, "running")
        .expect("new workspace turn after recovery");
    drop(recovered_guard);
}

// 验证 atomic turn start 返回同一边界之前的历史。
#[test]
fn started_turn_returns_prior_history_from_the_same_atomic_start_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");
    let thread = store.create_thread(None, None).expect("thread");
    let prior_turn =
        append_completed_conversation(&store, &thread.thread_id, "prior user", "prior assistant");

    let started = store
        .create_turn_with_input_and_history(
            &thread.thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": "current user"}]),
            10,
        )
        .expect("start turn");

    assert_eq!(started.history.messages.len(), 2);
    assert!(
        started
            .history
            .messages
            .iter()
            .all(|message| message.turn_id == prior_turn)
    );
    assert_eq!(started.item.turn_id, started.turn.turn_id);
}

// 追加一条 completed conversation，供 history 测试构造数据。
fn append_completed_conversation(
    store: &SessionStore,
    thread_id: &str,
    user: &str,
    assistant: &str,
) -> String {
    let (turn, _) = store
        .create_turn_with_input(
            thread_id,
            "running",
            serde_json::json!([{"type": "text", "text": user}]),
        )
        .expect("start turn");
    store
        .append_item(
            &turn.turn_id,
            ItemKind::AgentMessage,
            serde_json::json!({"delta": assistant}),
        )
        .expect("assistant item");
    store
        .update_turn_status(&turn.turn_id, TurnStatus::Completed)
        .expect("complete turn");
    turn.turn_id
}


fn sqlite_snapshot(path: &std::path::Path) -> String {
    let connection = rusqlite::Connection::open(path).expect("open sqlite snapshot");
    let schema = connection
        .prepare(
            "select type, name, coalesce(sql, '')
             from sqlite_master where name not like 'sqlite_%'
             order by type, name",
        )
        .expect("prepare sqlite schema snapshot")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("query sqlite schema snapshot")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect sqlite schema snapshot");
    let mut snapshot = String::new();
    for (kind, name, sql) in &schema {
        snapshot.push_str(&format!("{kind}:{name}:{sql}\n"));
    }
    for (kind, name, _) in schema {
        if kind != "table" {
            continue;
        }
        let query = format!(
            "select * from \"{}\" order by rowid",
            name.replace('"', "\"\"")
        );
        let mut statement = connection
            .prepare(&query)
            .expect("prepare sqlite row snapshot");
        let column_count = statement.column_count();
        let mut rows = statement.query([]).expect("query sqlite row snapshot");
        while let Some(row) = rows.next().expect("read sqlite row snapshot") {
            snapshot.push_str(&format!("row:{name}:"));
            for column in 0..column_count {
                snapshot.push_str(&format!(
                    "{:?};",
                    row.get_ref(column).expect("snapshot value")
                ));
            }
            snapshot.push('\n');
        }
    }
    snapshot
}

// 一次性探针（验证后删除）
#[test]
fn probe_resume_commit_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            "paused",
            serde_json::json!([{"type": "text", "text": "resume me"}]),
        )
        .expect("turn");
    store
        .update_turn_state(&turn.turn_id, TurnStatus::Paused, "paused")
        .expect("paused turn");
    let item_id = SessionStore::allocate_assistant_item_id();
    let result = store.commit_turn_outcome_with_authority(
        &turn.turn_id,
        CommitTurnOutcomeParams {
            status: TurnStatus::Completed,
            agent_loop_status: "completed",
            assistant_item_id: Some(&item_id),
            assistant_delta: Some("resumed answer"),
        },
        TurnOutcomeAuthority::AgentLoop,
    );
    match result {
        Ok(committed) => println!("PROBE commit ok: {}", committed.turn.status.as_storage_text()),
        Err(e) => println!("PROBE commit err: {e:?}"),
    }
}

// 验证 paused/suspended 无 owner turn 在 workspace 恢复时保持可恢复（turn/resume 依赖）。
#[test]
fn workspace_recovery_preserves_paused_and_suspended_turns() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("sessions.sqlite3");
    let paused_workspace = dir.path().join("paused-workspace");
    let suspended_workspace = dir.path().join("suspended-workspace");
    std::fs::create_dir(&paused_workspace).expect("paused workspace");
    std::fs::create_dir(&suspended_workspace).expect("suspended workspace");
    let store = SessionStore::open(&db_path).expect("store");
    let paused_thread = store
        .create_thread(None, Some(&paused_workspace.to_string_lossy()))
        .expect("paused thread");
    let paused = store
        .create_turn(&paused_thread.thread_id, "running")
        .expect("paused turn");
    store
        .update_turn_state(&paused.turn_id, TurnStatus::Paused, "paused")
        .expect("pause");
    let suspended_thread = store
        .create_thread(None, Some(&suspended_workspace.to_string_lossy()))
        .expect("suspended thread");
    let suspended = store
        .create_turn(&suspended_thread.thread_id, "running")
        .expect("suspended turn");
    store
        .update_turn_state(&suspended.turn_id, TurnStatus::Suspended, "suspended")
        .expect("suspend");

    store
        .recover_unowned_workspace_executions()
        .expect("recover");

    assert_eq!(
        store.get_turn(&paused.turn_id).expect("paused").status,
        TurnStatus::Paused
    );
    assert_eq!(
        store
            .get_turn(&suspended.turn_id)
            .expect("suspended")
            .status,
        TurnStatus::Suspended
    );
}
