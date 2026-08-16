#[cfg(windows)]
use super::file_identity::{checked_store_file_identity, open_store_file};
use super::*;

// 验证新连接启用外键、WAL、busy timeout 与 secure delete pragma。
#[test]
fn open_configures_sqlite_connection_pragmas() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("open store");

    let foreign_keys: u32 = store
        .connection
        .query_row("pragma foreign_keys", [], |row| row.get(0))
        .expect("foreign keys pragma");
    let journal_mode: String = store
        .connection
        .query_row("pragma journal_mode", [], |row| row.get(0))
        .expect("journal mode pragma");
    let busy_timeout_ms: u64 = store
        .connection
        .query_row("pragma busy_timeout", [], |row| row.get(0))
        .expect("busy timeout pragma");
    let secure_delete: u32 = store
        .connection
        .query_row("pragma secure_delete", [], |row| row.get(0))
        .expect("secure delete pragma");

    assert_eq!(foreign_keys, 1);
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(busy_timeout_ms, SQLITE_BUSY_TIMEOUT_MS);
    assert_eq!(secure_delete, 1);
}

#[test]
fn session_index_crud_round_trips_metadata() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("index.sqlite3")).expect("open store");
    let record = SessionRecord {
        session_id: "a7a1f0a1-65b0-4b78-b0e3-9c78d334aa11".to_string(),
        rollout_path: dir
            .path()
            .join("sessions")
            .join("a7a1f0a1-65b0-4b78-b0e3-9c78d334aa11.jsonl")
            .to_string_lossy()
            .to_string(),
        cwd: dir.path().to_string_lossy().to_string(),
        title: Some("first task".to_string()),
        model: Some("provider/model".to_string()),
        status: SessionStatus::Completed,
        created_at: "2026-08-15T00:00:00Z".to_string(),
        updated_at: "2026-08-15T00:01:00Z".to_string(),
        token_usage: serde_json::json!({"input_tokens": 12, "output_tokens": 3}),
    };
    store.insert_session(&record).expect("insert session");

    assert_eq!(store.get_session(&record.session_id).expect("read"), record);
    assert_eq!(store.list_sessions().expect("list").len(), 1);

    let updated = store
        .update_session(
            &record.session_id,
            SessionMetadataUpdate {
                status: Some(SessionStatus::Failed),
                title: Some(Some("renamed")),
                ..SessionMetadataUpdate::default()
            },
        )
        .expect("update session");
    assert_eq!(updated.status, SessionStatus::Failed);
    assert_eq!(updated.title.as_deref(), Some("renamed"));

    store
        .delete_session(&record.session_id)
        .expect("delete session");
    assert!(matches!(
        store.get_session(&record.session_id),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn trusted_reopen_keeps_the_same_file_identity() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("index.sqlite3")).expect("open store");
    let reopened = store.trusted_reopen().expect("reopen store");
    assert_eq!(
        reopened.descriptor().schema_version,
        store.descriptor().schema_version
    );
}

#[cfg(windows)]
#[test]
fn windows_file_identity_distinguishes_files_with_equal_attributes() {
    use std::os::windows::fs::MetadataExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let first_path = dir.path().join("first.sqlite3");
    let second_path = dir.path().join("second.sqlite3");
    std::fs::write(&first_path, b"first").expect("first file");
    std::fs::write(&second_path, b"second").expect("second file");
    assert_eq!(
        std::fs::metadata(&first_path)
            .expect("first metadata")
            .file_attributes(),
        std::fs::metadata(&second_path)
            .expect("second metadata")
            .file_attributes()
    );

    let first = open_store_file(&first_path, false).expect("open first");
    let second = open_store_file(&second_path, false).expect("open second");
    assert_ne!(
        checked_store_file_identity(&first).expect("first identity"),
        checked_store_file_identity(&second).expect("second identity")
    );
}

#[cfg(windows)]
#[test]
fn windows_file_identity_rejects_hard_links() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("sessions.sqlite3");
    let alias = dir.path().join("sessions-alias.sqlite3");
    std::fs::write(&path, b"store").expect("store file");
    std::fs::hard_link(&path, &alias).expect("hard link");
    let file = open_store_file(&path, false).expect("open hard-linked store");
    let error = checked_store_file_identity(&file).expect_err("hard link rejected");
    assert!(matches!(
        error,
        StoreError::InvalidState(message) if message.contains("hard links")
    ));
}

#[test]
fn v1_session_index_upgrades_in_place_and_accepts_idle() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("index.sqlite3");
    {
        let legacy = rusqlite::Connection::open(&path).expect("create v1 store");
        legacy
            .execute_batch(
                r#"
                create table schema_meta(schema_version integer not null check(schema_version = 1));
                create table schema_migrations(migration_id text primary key, applied_at text not null default current_timestamp);
                create table session_index(
                    session_id text primary key,
                    rollout_path text not null,
                    cwd text not null,
                    title text,
                    model text,
                    status text not null check(status in ('active', 'completed', 'failed', 'interrupted')),
                    created_at text not null,
                    updated_at text not null,
                    token_usage text not null
                );
                create index session_index_updated_at on session_index(updated_at);
                create index session_index_cwd on session_index(cwd);
                insert into schema_meta(schema_version) values(1);
                insert into schema_migrations(migration_id) values('0001_session_index');
                insert into session_index values(
                    'legacy-session', 'rollout.jsonl', 'cwd', 'legacy title', null,
                    'completed', '2026-08-15T00:00:00Z', '2026-08-15T00:01:00Z', '{}');
                "#,
            )
            .expect("seed v1 schema");
    }
    let store = SessionStore::open(&path).expect("open and upgrade v1 store");

    let legacy = store
        .get_session("legacy-session")
        .expect("legacy row survives upgrade");
    assert_eq!(legacy.status, SessionStatus::Completed);
    assert_eq!(legacy.title.as_deref(), Some("legacy title"));

    let idle = SessionRecord {
        session_id: "idle-session".to_string(),
        rollout_path: "rollout-idle.jsonl".to_string(),
        cwd: dir.path().to_string_lossy().to_string(),
        title: None,
        model: None,
        status: SessionStatus::Idle,
        created_at: now_iso(),
        updated_at: now_iso(),
        token_usage: serde_json::json!({}),
    };
    store
        .insert_session(&idle)
        .expect("idle inserts after upgrade");
    assert_eq!(
        store.get_session("idle-session").expect("read idle").status,
        SessionStatus::Idle
    );
    // 升级幂等：重开已升级库不再触发迁移路径。
    drop(store);
    SessionStore::open(&path).expect("reopen upgraded store");
}
