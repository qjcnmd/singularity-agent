//! Session index integration tests: schema, WAL, validation, CRUD.

use serde_json::json;
use singularity_store::{
    SessionMetadataUpdate, SessionRecord, SessionStatus, SessionStore, StoreError,
};

fn record(id: &str, path: &std::path::Path) -> SessionRecord {
    SessionRecord {
        session_id: id.to_string(),
        rollout_path: path.to_string_lossy().to_string(),
        cwd: path.parent().expect("parent").to_string_lossy().to_string(),
        title: None,
        model: None,
        status: Some(SessionStatus::Active),
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
fn file_backed_store_sets_schema_version_and_uses_wal_journal() {
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
fn quarantine_corrupted_store_files_isolates_db_and_sidecars() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("index.sqlite3");
    let wal = dir.path().join("index.sqlite3-wal");
    let shm = dir.path().join("index.sqlite3-shm");

    std::fs::write(&db, b"corrupted-main-db").expect("write db");
    std::fs::write(&wal, b"corrupted-wal").expect("write wal");
    std::fs::write(&shm, b"corrupted-shm").expect("write shm");

    let backup_path =
        singularity_store::quarantine_corrupted_store_files(&db).expect("quarantine files");
    assert!(!db.exists(), "original db must be moved");
    assert!(!wal.exists(), "original wal must be moved");
    assert!(!shm.exists(), "original shm must be moved");

    assert!(backup_path.exists(), "backup db must exist");
    let mut backup_wal = backup_path.as_os_str().to_os_string();
    backup_wal.push("-wal");
    assert!(
        std::path::Path::new(&backup_wal).exists(),
        "backup wal must exist"
    );
    let mut backup_shm = backup_path.as_os_str().to_os_string();
    backup_shm.push("-shm");
    assert!(
        std::path::Path::new(&backup_shm).exists(),
        "backup shm must exist"
    );
}

#[test]
fn sqlite_store_quarantines_not_a_database_and_reinitializes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("index.sqlite3");
    std::fs::write(&db, b"this is not an sqlite database header").expect("write garbage");

    let store = SessionStore::open(&db).expect("open should recover and reinitialize");
    assert_eq!(store.descriptor().schema_version, 1);

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    assert!(
        files
            .iter()
            .any(|f| f.starts_with("index.sqlite3.corrupt.")),
        "quarantine backup must exist: {files:?}"
    );
}

#[test]
fn initialization_callback_failure_preserves_quarantine_and_partial_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("index.sqlite3");
    std::fs::write(&db, b"corrupted-main-db").expect("write corrupt db");

    let error = match SessionStore::open_with_initialization(&db, |_| {
        Err(StoreError::InvalidState(
            "session index rebuild failed at jsonl scan".to_string(),
        ))
    }) {
        Ok(_) => panic!("rebuild failure must fail startup"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("session index rebuild failed"),
        "stage-specific callback error: {error}"
    );
    assert!(db.is_file(), "fresh partial database remains diagnosable");
    assert!(
        std::fs::read_dir(dir.path())
            .expect("read directory")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.sqlite3.corrupt.")),
        "corrupt backup must remain after rebuild failure"
    );

    // The callback runs after quarantine and current-schema creation, so a later
    // startup reopens the partial DB normally and classifies it as current.
    let reopened = SessionStore::open(&db).expect("reopen after failed rebuild");
    assert_eq!(reopened.descriptor().schema_version, 1);
}

#[test]
fn sqlite_store_quarantines_unsupported_schema() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = dir.path().join("legacy.sqlite3");
    let connection = rusqlite::Connection::open(&db).expect("legacy connection");
    connection
        .pragma_update(None, "user_version", 13)
        .expect("legacy version");
    drop(connection);

    let store = SessionStore::open(&db).expect("open should recover legacy schema");
    assert_eq!(store.descriptor().schema_version, 1);

    let files: Vec<_> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    assert!(
        files
            .iter()
            .any(|f| f.starts_with("legacy.sqlite3.corrupt.")),
        "quarantine backup must exist: {files:?}"
    );
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
