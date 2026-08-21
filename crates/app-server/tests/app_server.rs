//! stdio app-server 协议与用户级会话布局集成测试。

mod support;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use singularity_agent::session::SessionManager;
use support::{AppServerProcess, isolated_home, send_json};

fn spawn(workspace: &Path, home: &Path) -> AppServerProcess {
    AppServerProcess::spawn(workspace, home, "http://127.0.0.1:1/v1/responses")
}

#[test]
fn stdio_handshake_thread_start_lists_user_level_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let mut process = spawn(&workspace, &home);
    process.initialize();

    process.send_request(3, "thread/start", json!({"cwd": workspace}));
    let started = process.output.recv_id(3, Duration::from_secs(5));
    let thread_id = started["result"]["thread"]["thread_id"]
        .as_str()
        .expect("session id")
        .to_string();
    assert!(!thread_id.is_empty());
    assert_eq!(
        started["result"]["thread"]["cwd"].as_str(),
        Some(
            workspace
                .canonicalize()
                .expect("canonical workspace")
                .to_str()
                .expect("workspace utf8")
        )
    );
    let rollout = home.join("sessions").join(format!("{thread_id}.jsonl"));
    assert!(
        rollout.is_file(),
        "rollout file missing: {}",
        rollout.display()
    );
    let header: Value =
        serde_json::from_str(&std::fs::read_to_string(&rollout).expect("rollout")).expect("header");
    assert_eq!(header["id"], thread_id);
    assert!(home.join("index.sqlite3").is_file());

    process.send_request(4, "thread/list", json!({}));
    let listed = process.output.recv_id(4, Duration::from_secs(5));
    let threads = listed["result"]["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["thread_id"], thread_id);
    assert_eq!(
        threads[0]["cwd"].as_str(),
        Some(
            workspace
                .canonicalize()
                .expect("canonical")
                .to_str()
                .expect("workspace utf8")
        )
    );

    process.shutdown();
}

#[test]
fn stdio_startup_recovers_corrupt_index_and_rebuilds_jsonl_projection() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let sessions_dir = home.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_id = "01914f6b-0000-7000-8000-000000000006";
    let session = SessionManager::create_with_id(&workspace, &sessions_dir, session_id)
        .expect("session rollout");
    drop(session);
    std::fs::write(home.join("index.sqlite3"), b"not an sqlite database").expect("corrupt index");

    let mut process = spawn(&workspace, &home);
    process.initialize();
    process.send_request(3, "thread/list", json!({}));
    let listed = process.output.recv_id(3, Duration::from_secs(5));
    let threads = listed["result"]["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 1, "JSONL must rebuild the index row");
    assert_eq!(threads[0]["thread_id"], session_id);
    assert!(
        std::fs::read_dir(&home)
            .expect("home entries")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.sqlite3.corrupt.")),
        "corrupt index evidence must remain quarantined"
    );
    process.shutdown();
}

#[test]
fn explicit_db_startup_uses_the_same_recovery_and_rebuild_contract() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let sessions_dir = home.join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let session_id = "01914f6b-0000-7000-8000-000000000007";
    let session = SessionManager::create_with_id(&workspace, &sessions_dir, session_id)
        .expect("session rollout");
    drop(session);
    let explicit_db = dir.path().join("explicit-index.sqlite3");
    std::fs::write(&explicit_db, b"not an sqlite database").expect("corrupt explicit index");

    let mut process = AppServerProcess::spawn_with_db(
        &workspace,
        &home,
        "http://127.0.0.1:1/v1/responses",
        Some(&explicit_db),
    );
    process.initialize();
    process.send_request(3, "thread/list", json!({}));
    let listed = process.output.recv_id(3, Duration::from_secs(5));
    let threads = listed["result"]["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["thread_id"], session_id);
    assert!(
        std::fs::read_dir(dir.path())
            .expect("explicit db directory")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("explicit-index.sqlite3.corrupt.")),
        "explicit DB quarantine evidence must remain"
    );
    process.shutdown();
}

#[test]
fn removed_thread_and_turn_methods_return_stable_errors() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let mut process = spawn(&workspace, &home);
    process.initialize();
    for (id, method, params) in [
        (3, "thread/delete", json!({"threadId":"x"})),
        (4, "turn/status", json!({"turnId":"x"})),
        (5, "event/subscribe", json!({"eventTypes":[]})),
    ] {
        process.send_request(id, method, params);
        let response = process.output.recv_id(id, Duration::from_secs(5));
        assert_eq!(
            response["error"]["code"], -32601,
            "{method} must be removed from the registry: {response}"
        );
    }
    process.shutdown();
}

#[test]
fn session_read_returns_summary_and_recent_entries_then_delete_removes_both() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let mut process = spawn(&workspace, &home);
    process.initialize();

    process.send_request(3, "thread/start", json!({"cwd": workspace}));
    let started = process.output.recv_id(3, Duration::from_secs(5));
    let session_id = started["result"]["thread"]["thread_id"]
        .as_str()
        .expect("session id")
        .to_string();
    let rollout = home.join("sessions").join(format!("{session_id}.jsonl"));
    // 追加两条 message 条目，验证 read 返回“最近片段”而非空/全文。
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&rollout)
        .expect("open rollout");
    writeln!(
        file,
        "{{\"id\":\"e1\",\"timestamp\":\"2026-08-20T00:00:01.000Z\",\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"one\"}}]}}}}"
    )
    .expect("append entry");
    writeln!(
        file,
        "{{\"id\":\"e2\",\"timestamp\":\"2026-08-20T00:00:02.000Z\",\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"two\"}}]}}}}"
    )
    .expect("append entry");
    drop(file);

    process.send_request(
        4,
        "session/read",
        json!({"sessionId": session_id, "recentLimit": 1}),
    );
    let read = process.output.recv_id(4, Duration::from_secs(5));
    assert_eq!(read["result"]["sessionId"], session_id);
    assert_eq!(read["result"]["totalEntries"], 2);
    assert_eq!(
        read["result"]["recentEntries"]
            .as_array()
            .expect("entries")
            .len(),
        1
    );
    assert!(read["result"]["summary"].is_null());

    process.send_request(5, "session/delete", json!({"sessionId": session_id}));
    let deleted = process.output.recv_id(5, Duration::from_secs(5));
    assert_eq!(deleted["result"]["deleted"], true);
    assert!(
        !rollout.exists(),
        "session/delete must remove JSONL rollout"
    );
    process.shutdown();

    // 重启后索引行也已被删除。
    let mut process = spawn(&workspace, &home);
    process.initialize();
    process.send_request(3, "thread/list", json!({}));
    let listed = process.output.recv_id(3, Duration::from_secs(5));
    assert_eq!(
        listed["result"]["threads"]
            .as_array()
            .expect("threads")
            .len(),
        0
    );
    process.shutdown();
}

#[test]
fn stdio_rejects_json_rpc_batch_frames() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let mut process = spawn(&workspace, &home);
    process.initialize();
    process.send_request(3, "thread/list", json!({}));
    assert!(process.output.recv_id(3, Duration::from_secs(5))["result"].is_object());

    let batch = json!([
        {"jsonrpc":"2.0","method":"thread/list","id":4,"params":{}},
        {"jsonrpc":"2.0","method":"thread/list","id":5,"params":{}}
    ]);
    send_json(&mut process.input, batch);
    let response = process
        .output
        .recv_where(Duration::from_secs(5), |message| {
            message["error"]["code"] == -32600
        });
    assert_eq!(response["id"], Value::Null);
    process.shutdown();
}
