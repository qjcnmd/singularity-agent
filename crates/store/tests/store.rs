//! Session index integration tests: schema, WAL, validation, CRUD.

use serde_json::json;
use singularity_store::{SessionMetadataUpdate, SessionRecord, SessionStatus, SessionStore, StoreError};

fn record(id: &str, path: &std::path::Path) -> SessionRecord {
    SessionRecord {
        session_id: id.to_string(),
        rollout_path: path.to_string_lossy().to_string(),
        cwd: path.parent().expect("parent").to_string_lossy().to_string(),
        title: None,
        model: None,
        status: SessionStatus::Active,
        created_at: "2026-08-15T00:00:00Z".to_string(),
        updated_at: "2026-08-15T00:00:00Z".to_string(),
        token_usage: json!({}),
    }
}

#[test]
fn sqlite_store_persists_and_lists_session_metadata() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("index.sqlite3");
    let store = SessionStore::open(&db).expect("open store");
    store
        .insert_session(&record("one", &dir.path().join("one.jsonl")))
        .expect("insert one");
    store
        .insert_session(&record("two", &dir.path().join("two.jsonl")))
        .expect("insert two");

    let reopened = SessionStore::open(&db).expect("reopen store");
    let ids = reopened
        .list_sessions()
        .expect("list")
        .into_iter()
        .map(|record| record.session_id)
        .collect::<Vec<_>>();
    assert!(ids.contains(&"one".to_string()));
    assert!(ids.contains(&"two".to_string()));
}

#[test]
fn sqlite_store_writes_schema_meta_and_uses_wal_journal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("index.sqlite3");
    let store = SessionStore::open(&db).expect("open store");
    assert_eq!(store.descriptor().schema_version, 1);
    store
        .insert_session(&record("wal-probe", &dir.path().join("wal.jsonl")))
        .expect("write session");
    assert!(
        dir.path().join("index.sqlite3-wal").exists()
            || dir.path().join("index.sqlite3-shm").exists(),
        "WAL sidecar files are created for the file-backed index"
    );
    drop(store);
}

#[test]
fn sqlite_store_rejects_legacy_turn_schema() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("legacy.sqlite3");
    let connection = rusqlite::Connection::open(&db).expect("legacy connection");
    connection
        .execute_batch(
            "create table schema_meta(schema_version integer not null check(schema_version = 13));
             create table schema_migrations(migration_id text primary key, applied_at text not null default current_timestamp);
             create table threads(thread_id text primary key, model text, cwd text, status text not null default 'active');
             create table turns(turn_id text primary key, thread_id text not null, turn_sequence integer not null, status text not null, agent_loop_status text not null, pause_requested integer not null default 0);
             create table items(item_id text primary key, turn_id text not null, item_sequence integer not null, kind text not null, payload text not null, status text not null, redacted integer not null);
             insert into schema_meta(schema_version) values (13);",
        )
        .expect("legacy schema");
    drop(connection);

    let error = match SessionStore::open(&db) {
        Err(error) => error,
        Ok(_) => panic!("legacy turn schema must be rejected"),
    };
    assert!(matches!(
        error,
        StoreError::UnsupportedSchema { found: 13, supported: 1 }
    ));
}

#[test]
fn session_delete_and_update_fail_closed_for_missing_rows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("index.sqlite3")).expect("open store");
    assert!(matches!(
        store.delete_session("missing"),
        Err(StoreError::NotFound(_))
    ));
    assert!(matches!(
        store.update_session(
            "missing",
            SessionMetadataUpdate {
                status: Some(SessionStatus::Failed),
                ..SessionMetadataUpdate::default()
            }
        ),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn store_open_rejects_parent_symlinks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("target");
    std::fs::create_dir(&target).expect("target");
    let link = dir.path().join("link");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&target, &link).expect("symlink");

    let error = match SessionStore::open(link.join("index.sqlite3")) {
        Err(error) => error,
        Ok(_) => panic!("symlink parent must be rejected"),
    };
    assert!(matches!(error, StoreError::InvalidState(_)));
}
