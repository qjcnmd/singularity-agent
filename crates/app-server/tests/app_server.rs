//! stdio app-server 协议与用户级会话布局集成测试。

mod support;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
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
    let thread_id = started["result"]["thread"]["threadId"]
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

    process.send_request(4, "thread/list", json!({}));
    let listed = process.output.recv_id(4, Duration::from_secs(5));
    let threads = listed["result"]["threads"].as_array().expect("threads");
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0]["threadId"], thread_id);
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
fn thread_read_returns_turn_page_then_delete_removes_both() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let home_dir = isolated_home();
    let home = home_dir.path().to_path_buf();
    let mut process = spawn(&workspace, &home);
    process.initialize();

    process.send_request(3, "thread/start", json!({"cwd": workspace}));
    let started = process.output.recv_id(3, Duration::from_secs(5));
    let session_id = started["result"]["thread"]["threadId"]
        .as_str()
        .expect("session id")
        .to_string();
    let rollout = home.join("sessions").join(format!("{session_id}.jsonl"));
    // 追加两条无轮次标记的 message 条目，验证 read 把它们归入前导组返回。
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

    process.send_request(4, "thread/read", json!({"sessionId": session_id}));
    let read = process.output.recv_id(4, Duration::from_secs(5));
    assert_eq!(read["result"]["sessionId"], session_id);
    assert_eq!(read["result"]["totalTurns"], 0);
    let turns = read["result"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 1, "prelude entries form one leading group");
    assert!(turns[0]["turnId"].is_null());
    assert!(turns[0]["status"].is_null());
    assert_eq!(turns[0]["items"].as_array().expect("items").len(), 2);
    assert!(read["result"]["summary"].is_null());

    process.send_request(6, "session/delete", json!({"sessionId": session_id}));
    let deleted = process.output.recv_id(6, Duration::from_secs(5));
    assert_eq!(
        deleted["result"]["deleted"], true,
        "session/delete response: {deleted}"
    );
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
