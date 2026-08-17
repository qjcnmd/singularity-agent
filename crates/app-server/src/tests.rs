use std::sync::{Arc, Mutex};

use super::*;
use singularity_agent::session::SessionManager;
use singularity_model::{
    ModelError, ModelErrorKind, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, Provider, ProviderError, ProviderProtocolContract,
};

fn app_server(store: SessionStore, sessions_dir: &Path) -> AppServer {
    AppServer::new(
        store,
        ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            None,
        ),
    )
    .with_sessions_dir(sessions_dir)
}

fn initialize(server: &mut AppServer) {
    server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
        )
        .expect("initialize");
    server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#)
        .expect("initialized");
}

fn insert_session(server: &AppServer, sessions_dir: &Path, session_id: &str, cwd: &Path) -> String {
    let session =
        SessionManager::create_with_id(cwd, sessions_dir, session_id).expect("create session file");
    let created_at = now_iso();
    server
        .store()
        .insert_session(&SessionRecord {
            session_id: session_id.to_string(),
            rollout_path: session.path().to_string_lossy().to_string(),
            cwd: cwd.to_string_lossy().to_string(),
            title: None,
            model: Some("gpt-test".to_string()),
            status: None,
            created_at,
            updated_at: now_iso(),
            token_usage: json!({}),
        })
        .expect("insert session");
    let _ = created_at;
    session_id.to_string()
}

#[derive(Clone)]
struct StaticProvider {
    responses: Vec<ModelTurnResponse>,
    seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
}

impl Provider for StaticProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        let mut seen_requests = self.seen_requests.lock().expect("seen requests lock");
        let response_index = seen_requests.len();
        seen_requests.push(request.clone());
        let mut response = self
            .responses
            .get(response_index)
            .unwrap_or_else(|| self.responses.last().expect("static provider response"))
            .clone();
        response.request_id = request.request_id.clone();
        Ok(response)
    }
}

fn tool_using_provider(seen: Arc<Mutex<Vec<ModelTurnRequest>>>) -> StaticProvider {
    let mut first = ModelTurnResponse::completed("request_1", "response_1", "");
    first.tool_calls.push(ModelToolCall {
        tool_call_id: "call_1".to_string(),
        tool_name: "write".to_string(),
        arguments: json!({"path": "hello.txt", "content": "hello"}),
        raw_arguments: json!({"path": "hello.txt", "content": "hello"}).to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    StaticProvider {
        responses: vec![
            first,
            ModelTurnResponse::completed("request_2", "response_2", "done"),
        ],
        seen_requests: seen,
    }
}

#[test]
fn turn_start_runs_tools_in_user_session_and_updates_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "9b63cd69-94af-4e42-a53d-dac832be76f7";
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(
        tool_using_provider(Arc::new(Mutex::new(Vec::new()))),
    ));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"write hello.txt"}}]}}}}"#
        ))
        .expect("turn start");
    let result = responses
        .iter()
        .find(|message| message["id"] == 2)
        .expect("turn response");
    assert_eq!(result["result"]["turn"]["status"], "completed");

    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).expect("hello.txt"),
        "hello"
    );
    let rollout = sessions_dir.join(format!("{session_id}.jsonl"));
    assert!(rollout.is_file());
    let record = server.store().get_session(session_id).expect("indexed");
    assert_eq!(record.status, Some(SessionStatus::Completed));
    assert_eq!(record.title.as_deref(), Some("write hello.txt"));
    let session = SessionManager::open_existing(&rollout).expect("session");
    assert_eq!(session.session_id(), session_id);
}

fn failed_response() -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed("failed_request", "failed_response", "unused");
    response.status = ModelTurnStatus::Failed;
    response.assistant_message = None;
    response.error = Some(ModelError::new(
        ModelErrorKind::UnknownProviderError,
        "synthetic failure",
    ));
    response
}

fn completed_response(id: &str) -> ModelTurnResponse {
    ModelTurnResponse::completed(id, id, "done")
}

#[test]
fn session_status_sequence_tracks_turn_and_continue_ignores_terminal_status() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let provider = StaticProvider {
        responses: vec![
            completed_response("first"),
            failed_response(),
            completed_response("third"),
        ],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);

    let started = server
        .handle_json(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "thread/start",
                "id": 2,
                "params": {"cwd": workspace},
            })
            .to_string(),
        )
        .expect("thread start");
    let session_id = started[1]["result"]["thread"]["thread_id"]
        .as_str()
        .expect("session id")
        .to_string();
    // 尚无 turn：lastTurnStatus 为 null，索引行 status 也是 null，不伪装成 active。
    assert_eq!(
        started[1]["result"]["thread"]["lastTurnStatus"],
        serde_json::Value::Null
    );
    assert_eq!(
        server
            .store()
            .get_session(&session_id)
            .expect("record")
            .status,
        None
    );

    // completed → continue 必须保持 completed，不提前置 active。
    let first = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .expect("first turn");
    assert_eq!(
        first.iter().find(|m| m["id"] == 3).expect("response")["result"]["turn"]["status"],
        "completed"
    );
    assert_eq!(
        server
            .store()
            .get_session(&session_id)
            .expect("record")
            .status,
        Some(SessionStatus::Completed)
    );
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":4,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume completed");
    assert_eq!(
        resumed[0]["result"]["thread"]["lastTurnStatus"],
        "completed"
    );
    assert_eq!(
        server
            .store()
            .get_session(&session_id)
            .expect("record")
            .status,
        Some(SessionStatus::Completed)
    );

    // 失败 turn 把展示状态变为 failed；随后仍可 continue。
    let failed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":5,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"fail"}}]}}}}"#
        ))
        .expect_err("failed turn");
    assert!(matches!(
        &failed,
        AppServerError::TurnExecution { original: Some(text), .. } if text.contains("synthetic failure")
    ));
    assert_eq!(
        server
            .store()
            .get_session(&session_id)
            .expect("record")
            .status,
        Some(SessionStatus::Failed)
    );
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":6,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume failed");
    assert_eq!(resumed[0]["result"]["thread"]["lastTurnStatus"], "failed");

    let third = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":7,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"third"}}]}}}}"#
        ))
        .expect("third turn");
    assert_eq!(
        third.iter().find(|m| m["id"] == 7).expect("response")["result"]["turn"]["status"],
        "completed"
    );

    // interrupted 也是纯展示状态，continue 不受限制。
    server
        .store()
        .update_session(
            &session_id,
            SessionMetadataUpdate {
                status: Some(SessionStatus::Interrupted),
                ..SessionMetadataUpdate::default()
            },
        )
        .expect("set interrupted");
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":8,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume interrupted");
    assert_eq!(
        resumed[0]["result"]["thread"]["lastTurnStatus"],
        "interrupted"
    );
}

#[test]
fn last_turn_status_reports_running_only_with_live_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);

    let started = server
        .handle_json(
            &json!({
                "jsonrpc": "2.0",
                "method": "thread/start",
                "id": 2,
                "params": {"cwd": workspace},
            })
            .to_string(),
        )
        .expect("thread start");
    let session_id = started[1]["result"]["thread"]["thread_id"]
        .as_str()
        .expect("session id")
        .to_string();

    fn listed_status(server: &mut AppServer, id: i64, session_id: &str) -> serde_json::Value {
        server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"thread/list","id":{id}}}"#
            ))
            .expect("thread list")[0]["result"]["threads"]
            .as_array()
            .expect("threads")
            .iter()
            .find(|thread| thread["thread_id"] == session_id)
            .expect("session row")["lastTurnStatus"]
            .clone()
    }

    fn read_status(server: &mut AppServer, id: i64, session_id: &str) -> serde_json::Value {
        server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"session/read","id":{id},"params":{{"sessionId":"{session_id}","recentLimit":5}}}}"#
            ))
            .expect("session read")[0]["result"]["status"]
            .clone()
    }

    // 尚无 turn：三个读取接口一致投影为 null。
    assert_eq!(
        listed_status(&mut server, 3, &session_id),
        serde_json::Value::Null
    );
    assert_eq!(
        read_status(&mut server, 4, &session_id),
        serde_json::Value::Null
    );

    // 模拟崩溃遗留：索引行停在 active，但本进程没有该会话的存活 turn。
    server
        .store()
        .update_session(
            &session_id,
            SessionMetadataUpdate {
                status: Some(SessionStatus::Active),
                ..SessionMetadataUpdate::default()
            },
        )
        .expect("force active row");
    assert_eq!(
        listed_status(&mut server, 5, &session_id),
        json!("interrupted"),
        "crash-leftover active must not masquerade as running"
    );
    assert_eq!(
        read_status(&mut server, 6, &session_id),
        json!("interrupted")
    );
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":7,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume");
    assert_eq!(
        resumed[0]["result"]["thread"]["lastTurnStatus"],
        "interrupted"
    );
    assert_eq!(
        server
            .store()
            .get_session(&session_id)
            .expect("record")
            .status,
        Some(SessionStatus::Active),
        "resume must not rewrite the stored status"
    );

    // 真正运行中的 turn：存活 guard 期间投影为 active，结束后回到崩溃遗留投影。
    let (_cancellation, guard) = server
        .activate_turn("turn_live_1", &session_id)
        .expect("activate turn");
    assert_eq!(listed_status(&mut server, 8, &session_id), json!("active"));
    assert_eq!(read_status(&mut server, 9, &session_id), json!("active"));
    drop(guard);
    assert_eq!(
        listed_status(&mut server, 10, &session_id),
        json!("interrupted")
    );
    assert_eq!(
        read_status(&mut server, 11, &session_id),
        json!("interrupted")
    );
}

fn legacy_project(temp: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    let workspace = temp.join("workspace");
    let legacy = workspace.join(".singularity").join("agent-sessions");
    std::fs::create_dir_all(&legacy).expect("legacy dir");
    let source = legacy.join("thread_old.jsonl");
    let header = json!({
        "type": "session",
        "version": 3,
        "id": session_id,
        "timestamp": "2026-08-15T00:00:00Z",
        "cwd": workspace
    });
    std::fs::write(&source, format!("{header}\n")).expect("legacy jsonl");
    workspace
}

#[test]
fn migration_refuses_non_empty_legacy_sqlite() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "8512c2a8-0e0c-49cd-a799-4d8a2096349a";
    let workspace = legacy_project(temp.path(), session_id);
    let sqlite = workspace
        .join(".singularity")
        .join("rust-app-server.sqlite3");
    let connection = rusqlite::Connection::open(&sqlite).expect("legacy sqlite");
    connection
        .execute_batch(
            "create table threads(thread_id text primary key);
             insert into threads values ('legacy-thread');",
        )
        .expect("legacy data");
    drop(connection);

    // AppPaths home 与 workspace 分离，避免共享同一临时根目录。
    let home = temp.path().join("home");
    let paths = crate::paths::AppPaths {
        home_dir: home.clone(),
        index_path: home.join("index.sqlite3"),
        sessions_dir: home.join("sessions"),
        backups_dir: home.join("backups"),
    };
    paths.prepare().expect("paths");
    let store = SessionStore::open(&paths.index_path).expect("store");
    let error = crate::paths::migrate_legacy_project_sessions(&paths, &store, &workspace)
        .expect_err("non-empty legacy sqlite must stop migration");
    assert!(error.contains("user data row"), "{error}");
    assert!(
        workspace
            .join(".singularity")
            .join("agent-sessions")
            .join("thread_old.jsonl")
            .is_file()
    );
    assert!(sqlite.is_file());
}

#[test]
fn migration_is_idempotent_when_destination_or_index_already_exists() {
    let temp = tempfile::tempdir().expect("temp dir");
    let session_id = "2e22c5fa-92ba-483c-b245-20a3e6d19e05";
    let workspace = legacy_project(temp.path(), session_id);
    let source = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join("thread_old.jsonl");
    let legacy_sqlite = workspace
        .join(".singularity")
        .join("rust-app-server.sqlite3");
    let connection = rusqlite::Connection::open(&legacy_sqlite).expect("empty legacy sqlite");
    connection
        .execute_batch("create table schema_meta(schema_version integer);")
        .expect("empty schema");
    drop(connection);

    // AppPaths home 与 workspace 分离，避免共享同一临时根目录。
    let home = temp.path().join("home");
    let paths = crate::paths::AppPaths {
        home_dir: home.clone(),
        index_path: home.join("index.sqlite3"),
        sessions_dir: home.join("sessions"),
        backups_dir: home.join("backups"),
    };
    paths.prepare().expect("paths");
    let store = SessionStore::open(&paths.index_path).expect("store");

    // 第一次完整迁移，备份含旧 SQLite 与 manifest。
    assert_eq!(
        crate::paths::migrate_legacy_project_sessions(&paths, &store, &workspace).expect("migrate"),
        1
    );
    assert!(!source.exists());
    assert!(!legacy_sqlite.exists());
    let backup = std::fs::read_dir(&paths.backups_dir)
        .expect("backups")
        .filter_map(Result::ok)
        .find(|entry| entry.path().is_dir())
        .expect("backup dir")
        .path();
    assert!(backup.join("rust-app-server.sqlite3").is_file());
    assert!(backup.join("manifest.json").is_file());

    // 完整迁移后再次运行：无旧对象，幂等返回 0。
    assert_eq!(
        crate::paths::migrate_legacy_project_sessions(&paths, &store, &workspace).expect("rerun"),
        0
    );

    // 部分失败续跑：destination 已复制但索引不存在（模拟上次插入前失败）。
    let workspace2 = legacy_project(&temp.path().join("partial"), session_id);
    let paths2 = crate::paths::AppPaths {
        home_dir: temp.path().join("partial-home"),
        index_path: temp.path().join("partial-home").join("index.sqlite3"),
        sessions_dir: temp.path().join("partial-home").join("sessions"),
        backups_dir: temp.path().join("partial-home").join("backups"),
    };
    paths2.prepare().expect("paths2");
    let store2 = SessionStore::open(&paths2.index_path).expect("store2");
    let destination = paths2.sessions_dir.join(format!("{session_id}.jsonl"));
    std::fs::create_dir_all(&paths2.sessions_dir).expect("sessions");
    std::fs::copy(
        workspace2
            .join(".singularity")
            .join("agent-sessions")
            .join("thread_old.jsonl"),
        &destination,
    )
    .expect("simulate partial copy");
    assert_eq!(
        crate::paths::migrate_legacy_project_sessions(&paths2, &store2, &workspace2)
            .expect("resume partial"),
        1
    );
    assert!(store2.get_session(session_id).is_ok());
    assert!(destination.is_file());
    assert!(
        !workspace2
            .join(".singularity")
            .join("agent-sessions")
            .join("thread_old.jsonl")
            .exists()
    );
}

#[test]
fn session_delete_faults_roll_back_or_leave_recognizable_tombstone() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let server = app_server(store, &sessions_dir);
    let session_id = "cff25e0f-60b8-44dd-97eb-5f2bb4fda847";
    insert_session(&server, &sessions_dir, session_id, &workspace);
    let rollout = sessions_dir.join(format!("{session_id}.jsonl"));
    let record = server.store().get_session(session_id).expect("record");

    // rename 失败：原文件与索引都不动。
    let error = crate::delete::delete_session_with_faults(
        &record,
        server.store(),
        crate::delete::DeleteFaults {
            fail_rename: true,
            ..crate::delete::DeleteFaults::default()
        },
    )
    .expect_err("rename failure");
    assert!(error.to_string().contains("rename failure"));
    assert!(rollout.is_file());
    assert!(server.store().get_session(session_id).is_ok());

    // 索引删除失败：rollout 必须从 tombstone 恢复。
    let error = crate::delete::delete_session_with_faults(
        &record,
        server.store(),
        crate::delete::DeleteFaults {
            fail_index_delete: true,
            ..crate::delete::DeleteFaults::default()
        },
    )
    .expect_err("index delete failure");
    assert!(error.to_string().contains("index delete failure"));
    assert!(rollout.is_file());
    assert!(server.store().get_session(session_id).is_ok());

    // 最终清理失败：索引与可见 rollout 已删，只留下可识别 tombstone。
    let tombstone = crate::delete::delete_session_with_faults(
        &record,
        server.store(),
        crate::delete::DeleteFaults {
            leave_tombstone: true,
            ..crate::delete::DeleteFaults::default()
        },
    )
    .expect("logical delete")
    .expect("tombstone path");
    assert!(!rollout.exists());
    assert!(matches!(
        server.store().get_session(session_id),
        Err(StoreError::NotFound(_))
    ));
    assert!(tombstone.is_file());
    assert!(
        tombstone
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".tombstone")
    );
}

#[test]
fn turn_steer_and_follow_up_inject_into_active_turn_queues() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "b928f6f2-ddb4-4a0b-a237-6936c7e8c268";
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    let (_, _guard) = server
        .activate_turn("turn_live", session_id)
        .expect("activate turn");
    let steer = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let follow_up = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    server
        .steer_handles
        .lock()
        .expect("steer handles")
        .insert("turn_live".to_string(), Arc::clone(&steer));
    server
        .follow_up_handles
        .lock()
        .expect("follow up handles")
        .insert("turn_live".to_string(), Arc::clone(&follow_up));

    server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/steer","id":3,"params":{"turnId":"turn_live","input":[{"type":"text","text":"change direction"}]}}"#,
        )
        .expect("turn steer");
    server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/followUp","id":4,"params":{"turnId":"turn_live","input":[{"type":"text","text":"keep going"}]}}"#,
        )
        .expect("turn followUp");

    let steer = steer.lock().expect("steer queue");
    let follow_up = follow_up.lock().expect("follow up queue");
    assert_eq!(steer.len(), 1);
    assert_eq!(steer[0], "change direction");
    assert_eq!(follow_up.len(), 1);
    assert_eq!(follow_up[0], "keep going");
}

#[test]
fn request_methods_as_notifications_are_rejected_without_side_effects() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "0b0c1d2e-3f40-4152-8263-9474a5b6c7d8";
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    // 每个 Request 方法以 notification（无 id）提交 → -32600 且不执行任何副作用。
    for (method, params) in [
        (
            "initialize",
            r#"{"clientInfo":{"name":"t","title":"T","version":"0"}}"#,
        ),
        ("thread/start", r#"{"cwd":"/tmp"}"#),
        ("thread/resume", r#"{"threadId":"<session>"}"#),
        ("session/read", r#"{"sessionId":"<session>"}"#),
        ("session/delete", r#"{"sessionId":"<session>"}"#),
        (
            "turn/start",
            r#"{"threadId":"<session>","input":[{"type":"text","text":"x"}]}"#,
        ),
        (
            "turn/steer",
            r#"{"turnId":"t","input":[{"type":"text","text":"x"}]}"#,
        ),
        (
            "turn/followUp",
            r#"{"turnId":"t","input":[{"type":"text","text":"x"}]}"#,
        ),
        ("agent/capability", r#"{}"#),
        ("turn/interrupt", r#"{"turnId":"t"}"#),
        ("server/shutdown", r#"{}"#),
    ] {
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"{method}","params":{}}}"#,
            params.replace("<session>", session_id)
        );
        let responses = server.handle_json(&body).expect("notification handled");
        assert_eq!(responses.len(), 1, "{method}");
        assert_eq!(responses[0]["error"]["code"], -32600, "{method}");
        assert!(responses[0]["error"]["message"].is_string(), "{method}");
    }

    // 副作用检查：会话仍存在、thread/start 未创建任何 thread、服务器未关闭。
    assert!(server.store().get_session(session_id).is_ok());
    assert!(!server.shutdown_requested);
    let responses = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"thread/list","id":10,"params":{}}"#)
        .expect("thread list");
    let threads = responses
        .iter()
        .find(|message| message["id"] == 10)
        .expect("thread list response");
    assert_eq!(
        threads["result"]["threads"].as_array().map(Vec::len),
        Some(1)
    );
}

#[test]
fn session_delete_rejects_active_turn_and_succeeds_after_turn_ends() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "c14e4e8b-9b4a-4c1d-8f0a-2d5e6f7a8b9c";
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    let (_, guard) = server
        .activate_turn("turn_live", session_id)
        .expect("activate turn");

    // 活跃 turn 期间删除 → invalid state 拒绝，索引与 rollout 都不动。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"session/delete","id":2,"params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .expect("delete rejected");
    assert_eq!(responses[0]["error"]["code"], -32005);
    assert!(server.store().get_session(session_id).is_ok());
    assert!(sessions_dir.join(format!("{session_id}.jsonl")).is_file());

    // turn 结束后删除成功。
    drop(guard);
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"session/delete","id":3,"params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .expect("delete after turn");
    assert_eq!(responses[0]["result"]["deleted"], true);
    assert!(matches!(
        server.store().get_session(session_id),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn steer_and_follow_up_after_turn_completion_queue_for_next_turn_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "7d1e2f3a-4b5c-4d6e-8f90-1a2b3c4d5e6f";
    let requests: Arc<Mutex<Vec<ModelTurnRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![
            completed_response("turn_one"),
            completed_response("turn_two"),
        ],
        seen_requests: Arc::clone(&requests),
    };
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    // turn 1 正常完成。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .expect("turn one");
    let turn_one = responses
        .iter()
        .find(|message| message["id"] == 2)
        .expect("turn one response");
    let turn_id = turn_one["result"]["turn"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    assert_eq!(turn_one["result"]["turn"]["status"], "completed");

    // turn 已终态：steer / followUp 不再 not found，入 thread 级待办队列。
    for (method, text) in [
        ("turn/steer", "change direction"),
        ("turn/followUp", "keep going"),
    ] {
        let responses = server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"{method}","id":3,"params":{{"turnId":"{turn_id}","input":[{{"type":"text","text":"{text}"}}]}}}}"#
            ))
            .expect("post-terminal inject");
        assert!(
            responses[0]["result"].is_object(),
            "{method} after completion must be queued, got: {responses:?}"
        );
    }
    assert_eq!(
        server
            .thread_steer_pending
            .lock()
            .expect("steer pending")
            .get(session_id)
            .map(VecDeque::len),
        Some(1)
    );
    assert_eq!(
        server
            .thread_follow_up_pending
            .lock()
            .expect("follow up pending")
            .get(session_id)
            .map(VecDeque::len),
        Some(1)
    );

    // turn 2 在同一 thread 开始：取走待办并注入，请求消息包含纸条文本。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":4,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"second"}}]}}}}"#
        ))
        .expect("turn two");
    assert_eq!(
        responses.iter().find(|m| m["id"] == 4).expect("turn two")["result"]["turn"]["status"],
        "completed"
    );
    let seen = requests.lock().expect("seen requests");
    let last = seen.last().expect("second provider call");
    assert!(
        seen.iter().any(|request| request
            .messages
            .iter()
            .any(|m| m.content == "change direction")),
        "second request must carry the queued steer text"
    );
    assert!(
        seen.iter()
            .any(|request| request.messages.iter().any(|m| m.content == "keep going")),
        "second request must carry the queued followUp text"
    );
    let _ = last;
}

#[test]
fn project_instructions_load_from_workspace_root_to_cwd() {
    // H2 回归：root→cwd 逐层加载（此前 &cwd,&cwd 只加载 cwd 层）。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let nested = workspace.join("src").join("nested");
    std::fs::create_dir_all(nested.join("..")).expect("nested parent");
    std::fs::create_dir_all(&nested).expect("nested cwd");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    std::fs::write(workspace.join("AGENTS.md"), "ROOT INSTRUCTION").expect("root agents");
    std::fs::write(nested.join("AGENTS.md"), "NESTED INSTRUCTION").expect("nested agents");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let requests: Arc<Mutex<Vec<ModelTurnRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![completed_response("instructions_turn")],
        seen_requests: Arc::clone(&requests),
    };
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(
        &server,
        &sessions_dir,
        "9f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d",
        &nested,
    );

    server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{"threadId":"9f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d","input":[{"type":"text","text":"do it"}]}}"#,
        )
        .expect("turn start");
    let seen = requests.lock().expect("seen requests");
    let request = seen.last().expect("provider request");
    let joined: String = request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("ROOT INSTRUCTION"),
        "root AGENTS.md must be loaded from workspace root: {joined}"
    );
    assert!(
        joined.contains("NESTED INSTRUCTION"),
        "nested AGENTS.md must be loaded: {joined}"
    );
}
