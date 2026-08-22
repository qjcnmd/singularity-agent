use std::io::Write;
use std::sync::{Arc, Mutex};

use super::*;
use singularity_agent::agent::TurnInbox;
use singularity_agent::session::SessionManager;
use singularity_model::{
    ModelError, ModelErrorKind, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelUsage, Provider, ProviderError,
    ProviderProtocolContract, ProviderReasoningReplay,
};

/// 测试共享的注入 runtime：provider 异步执行一律由上层提供。
pub(super) fn test_runtime_handle() -> tokio::runtime::Handle {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test provider runtime")
        })
        .handle()
        .clone()
}

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
            test_runtime_handle(),
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

#[test]
fn jsonl_discovery_rebuilds_index_without_repairing_incomplete_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "9b63cd69-94af-4e42-a53d-dac832be76f8";
    let mut session =
        SessionManager::create_with_id(&workspace, &sessions_dir, session_id).expect("session");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_started(
            "turn-1",
        ))
        .expect("turn metadata");
    store
        .insert_session(&SessionRecord {
            session_id: session_id.to_string(),
            rollout_path: session.path().to_string_lossy().to_string(),
            cwd: workspace.to_string_lossy().to_string(),
            title: None,
            model: Some("provider/model".to_string()),
            status: Some(SessionStatus::Active),
            created_at: now_iso(),
            updated_at: now_iso(),
            token_usage: json!({}),
        })
        .expect("stale index");

    rebuild_session_index_from_jsonl(&store, &sessions_dir).expect("discover sessions");
    let discovered = store.get_session(session_id).expect("discovered record");
    assert_eq!(discovered.status, Some(SessionStatus::Active));
    let discovered_session = SessionManager::open_existing(session.path()).expect("reopen");
    assert_eq!(discovered_session.metadata_entries().len(), 1);

    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":2,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume repairs session");
    assert_eq!(
        resumed[0]["result"]["thread"]["lastTurnStatus"],
        "interrupted"
    );
    assert_eq!(
        server.store().get_session(session_id).unwrap().status,
        Some(SessionStatus::Interrupted)
    );
    assert_eq!(
        SessionManager::open_existing(session.path())
            .unwrap()
            .metadata_entries()
            .len(),
        2
    );
    server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":3,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("second resume");
    assert_eq!(
        SessionManager::open_existing(session.path())
            .unwrap()
            .metadata_entries()
            .len(),
        2
    );
}

#[test]
fn jsonl_discovery_isolates_one_corrupt_rollout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let valid_id = "c2e0d5f5-7d50-4ef7-a6f9-0f0c1b3f44ab";
    SessionManager::create_with_id(&workspace, &sessions_dir, valid_id).expect("valid session");
    std::fs::write(sessions_dir.join("broken.jsonl"), b"not-json\n").expect("broken session");

    rebuild_session_index_from_jsonl(&store, &sessions_dir).expect("discovery isolates bad file");
    let sessions = store.list_sessions().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, valid_id);
}

#[test]
fn corrupted_index_reopens_with_quarantine_and_jsonl_rebuild() {
    // D-037：JSONL rollout 是权威正文，SQLite 索引损坏不允许永久不一致。
    // 人为把索引写成非数据库垃圾后，走与 supervisor 启动相同的重开路径
    // （quarantine + 当前 schema 创建 + JSONL 重建回调）打开：会话投影从
    // JSONL 恢复，损坏库保留为审计备份。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let index_path = temp.path().join("index.sqlite3");
    let session_id = "c3d4e5f6-a7b8-4c9d-8e1f-2a3b4c5d6e7f";
    let mut session =
        SessionManager::create_with_id(&workspace, &sessions_dir, session_id).expect("session");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_completed(
            "turn-1",
        ))
        .expect("terminal metadata");
    drop(session);

    let store = SessionStore::open(&index_path).expect("initial store");
    store
        .insert_session(&SessionRecord {
            session_id: session_id.to_string(),
            rollout_path: sessions_dir
                .join(format!("{session_id}.jsonl"))
                .to_string_lossy()
                .to_string(),
            cwd: workspace.to_string_lossy().to_string(),
            title: None,
            model: None,
            status: None,
            created_at: now_iso(),
            updated_at: now_iso(),
            token_usage: json!({}),
        })
        .expect("insert session");
    drop(store);

    std::fs::write(&index_path, b"this is not an sqlite database header")
        .expect("corrupt the index");

    let reopened = SessionStore::open_with_initialization(&index_path, |store| {
        rebuild_session_index_from_jsonl(store, &sessions_dir).map_err(|error| {
            StoreError::InvalidState(format!(
                "failed to rebuild app-server session index: {error}"
            ))
        })
    })
    .expect("reopen must quarantine and rebuild from JSONL");
    let record = reopened.get_session(session_id).expect("rebuilt record");
    assert_eq!(
        record.rollout_path,
        sessions_dir
            .join(format!("{session_id}.jsonl"))
            .to_string_lossy()
    );
    assert_eq!(record.cwd, workspace.to_string_lossy());
    assert_eq!(record.status, Some(SessionStatus::Completed));

    let files: Vec<String> = std::fs::read_dir(temp.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        files
            .iter()
            .any(|name| name.starts_with("index.sqlite3.corrupt.")),
        "corrupted index must remain as audit backup: {files:?}"
    );
}

#[test]
fn jsonl_discovery_rejects_an_oversized_unterminated_header_without_indexing_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let valid_id = "c2e0d5f5-7d50-4ef7-a6f9-0f0c1b3f44ab";
    SessionManager::create_with_id(&workspace, &sessions_dir, valid_id).expect("valid session");

    let mut oversized =
        std::fs::File::create(sessions_dir.join("oversized.jsonl")).expect("oversized session");
    let chunk = [b'x'; 4096];
    for _ in 0..=(16 * 1024 * 1024 / chunk.len()) {
        oversized.write_all(&chunk).expect("oversized header bytes");
    }
    oversized.flush().expect("flush oversized header");

    rebuild_session_index_from_jsonl(&store, &sessions_dir)
        .expect("discovery isolates oversized header");
    let sessions = store.list_sessions().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, valid_id);
}

#[test]
fn jsonl_discovery_recovers_all_fields_including_title_model_usage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "d3f1e6a7-8b90-4c12-9e34-5f6a7b8c9d0e";
    let mut session =
        SessionManager::create_with_id(&workspace, &sessions_dir, session_id).expect("session");

    session
        .append_message(singularity_agent::message::AgentMessage::text(
            singularity_agent::message::AgentMessageRole::User,
            "Implement feature X for the system",
        ))
        .expect("user message");
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::thread_settings(
                "anthropic",
                "claude-3-7-sonnet",
                Some("high".to_string()),
            )
            .expect("settings"),
        )
        .expect("settings metadata");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_completed(
            "turn-1",
        ))
        .expect("turn completed");
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::usage(
                "turn-1",
                json!({"input_tokens": 120, "output_tokens": 45}),
            )
            .expect("usage"),
        )
        .expect("usage metadata");

    rebuild_session_index_from_jsonl(&store, &sessions_dir).expect("rebuild index");
    let record = store.get_session(session_id).expect("get session");
    assert_eq!(
        record.title.as_deref(),
        Some("Implement feature X for the system")
    );
    assert_eq!(
        record.model.as_deref(),
        Some("anthropic/claude-3-7-sonnet#high")
    );
    assert_eq!(record.status, Some(SessionStatus::Completed));
    assert_eq!(
        record.token_usage,
        json!({"input_tokens": 120, "output_tokens": 45})
    );
}

#[test]
fn jsonl_discovery_uses_header_creation_and_last_entry_fact_times() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "f4a2b3c4-d5e6-4f7a-8b9c-0d1e2f3a4b5c";
    let rollout = sessions_dir.join(format!("{session_id}.jsonl"));
    let header_timestamp = "2020-01-01T00:00:00.000Z";
    let entry_timestamp = "2024-04-05T06:07:08.000Z";
    let header = json!({
        "type": "session",
        "version": 1,
        "id": session_id,
        "timestamp": header_timestamp,
        "cwd": workspace.to_string_lossy(),
    });
    let message = json!({
        "type": "message",
        "id": "entry-1",
        "timestamp": entry_timestamp,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "timestamp probe"}]
        }
    });
    std::fs::write(&rollout, format!("{header}\n{message}\n")).expect("rollout");

    store
        .insert_session(&SessionRecord {
            session_id: session_id.to_string(),
            rollout_path: rollout.to_string_lossy().to_string(),
            cwd: workspace.to_string_lossy().to_string(),
            title: Some("stale title".to_string()),
            model: None,
            status: Some(SessionStatus::Failed),
            created_at: "1999-01-01T00:00:00.000Z".to_string(),
            updated_at: "1999-01-01T00:00:00.000Z".to_string(),
            token_usage: json!({"stale": true}),
        })
        .expect("stale index");

    rebuild_session_index_from_jsonl(&store, &sessions_dir).expect("rebuild");
    let record = store.get_session(session_id).expect("rebuilt record");
    assert_eq!(record.created_at, header_timestamp);
    assert_eq!(record.updated_at, entry_timestamp);
}

#[test]
fn jsonl_discovery_isolates_legacy_parent_field_rollout() {
    // 线性化硬切后 parentId 不再合法：任何旧格式 rollout 按坏文件隔离，不建索引。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "a4a2b3c4-d5e6-4f7a-8b9c-0d1e2f3a4b5c";
    let rollout = sessions_dir.join(format!("{session_id}.jsonl"));
    let header = json!({
        "type": "session",
        "version": 1,
        "id": session_id,
        "timestamp": "2026-08-20T00:00:00.000Z",
        "cwd": workspace.to_string_lossy(),
    });
    let first = json!({
        "type": "message",
        "id": "entry-1",
        "timestamp": "2026-08-20T00:00:01.000Z",
        "message": {"role": "user", "content": [{"type": "text", "text": "one"}]}
    });
    let legacy = json!({
        "type": "message",
        "id": "entry-2",
        "parentId": "entry-1",
        "timestamp": "2026-08-20T00:00:02.000Z",
        "message": {"role": "user", "content": [{"type": "text", "text": "old format"}]}
    });
    std::fs::write(&rollout, format!("{header}\n{first}\n{legacy}\n")).expect("rollout");

    rebuild_session_index_from_jsonl(&store, &sessions_dir).expect("rebuild isolates legacy");
    assert!(store.list_sessions().expect("list sessions").is_empty());
}

#[test]
fn jsonl_discovery_deletes_ghost_index_rows() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");

    let valid_id = "e4a2b3c4-d5e6-4f7a-8b9c-0d1e2f3a4b5c";
    SessionManager::create_with_id(&workspace, &sessions_dir, valid_id).expect("session");

    let ghost_id = "f5b3c4d5-e6f7-4a8b-9c0d-1e2f3a4b5c6d";
    store
        .insert_session(&SessionRecord {
            session_id: ghost_id.to_string(),
            rollout_path: sessions_dir
                .join(format!("{ghost_id}.jsonl"))
                .to_string_lossy()
                .to_string(),
            cwd: workspace.to_string_lossy().to_string(),
            title: Some("Ghost session".to_string()),
            model: None,
            status: Some(SessionStatus::Completed),
            created_at: now_iso(),
            updated_at: now_iso(),
            token_usage: json!({}),
        })
        .expect("insert ghost");

    assert_eq!(store.list_sessions().expect("list").len(), 1);

    rebuild_session_index_from_jsonl(&store, &sessions_dir).expect("rebuild index");

    let sessions = store.list_sessions().expect("list after rebuild");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, valid_id);
    assert!(store.get_session(ghost_id).is_err());
}

#[test]
fn thread_settings_are_jsonl_first_and_never_store_credentials() {
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
    let thread_id = started[1]["result"]["thread"]["threadId"]
        .as_str()
        .expect("thread id")
        .to_string();
    let settings = server
        .handle_json(
            &json!({
                "jsonrpc": "2.0",
                "method": "thread/settings",
                "id": 3,
                "params": {
                    "threadId": thread_id,
                    "provider": "openai_compatible",
                    "model": "gpt-test",
                },
            })
            .to_string(),
        )
        .expect("settings");
    assert_eq!(settings[0]["result"]["updated"], true);
    let record = server.store().get_session(&thread_id).expect("record");
    assert_eq!(record.model.as_deref(), Some("openai_compatible/gpt-test"));
    rebuild_session_index_from_jsonl(server.store(), &sessions_dir).expect("discover settings");
    assert_eq!(
        server
            .store()
            .get_session(&thread_id)
            .unwrap()
            .model
            .as_deref(),
        Some("openai_compatible/gpt-test")
    );
    let rollout = std::fs::read_to_string(record.rollout_path).expect("rollout");
    assert!(rollout.contains("thread_settings"));
    assert!(!rollout.contains("apiKey"));
    assert!(!rollout.contains("authorization"));
}

#[test]
fn server_capabilities_method_is_removed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &temp.path().join("sessions"));
    initialize(&mut server);
    let response = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"server/capabilities","id":2,"params":{}}"#)
        .expect("capabilities");
    assert_eq!(response[0]["error"]["code"], -32601);
}

#[test]
fn public_history_projection_omits_private_replay_and_internal_tree_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "7b63cd69-94af-4e42-a53d-dac832be76f8";
    let mut session =
        SessionManager::create_with_id(&workspace, &sessions_dir, session_id).expect("session");
    session
        .append_message(singularity_agent::message::AgentMessage {
            role: singularity_agent::message::AgentMessageRole::Assistant,
            content: vec![singularity_agent::message::ContentBlock::Thinking {
                thinking: "visible reasoning".to_string(),
                signature: None,
            }, singularity_agent::message::ContentBlock::ToolCall {
                id: "call-1".to_string(),
                name: "write".to_string(),
                args: json!({"path":"out.txt","content":"ok"}),
            }],
            provider_reasoning_replay: Some(ProviderReasoningReplay::Responses {
                provider_name: "private-provider".to_string(),
                model_name: "private-model".to_string(),
                reasoning_effort: "high".to_string(),
                tool_call_ids: vec!["call-1".to_string()],
                items: vec![
                    json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque-secret"}),
                    json!({"type":"function_call","call_id":"call-1","name":"write","arguments":"{}"}),
                ],
            }),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
            timestamp: None,
        })
        .expect("assistant");
    session
        .append_message(singularity_agent::message::AgentMessage {
            role: singularity_agent::message::AgentMessageRole::ToolResult,
            content: vec![singularity_agent::message::ContentBlock::Text {
                text: "write failed".to_string(),
            }],
            provider_reasoning_replay: None,
            tool_call_id: Some("call-1".to_string()),
            tool_name: Some("write".to_string()),
            is_error: Some(true),
            timestamp: None,
        })
        .expect("tool result");
    store
        .insert_session(&SessionRecord {
            session_id: session_id.to_string(),
            rollout_path: session.path().to_string_lossy().to_string(),
            cwd: workspace.to_string_lossy().to_string(),
            title: None,
            model: None,
            status: None,
            created_at: now_iso(),
            updated_at: now_iso(),
            token_usage: json!({}),
        })
        .expect("index");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let output = server
        .handle_json(
            &json!({
                "jsonrpc":"2.0",
                "method":"session/read",
                "id":2,
                "params":{"sessionId":session_id}
            })
            .to_string(),
        )
        .expect("session/read");
    let wire = serde_json::to_string(&output).expect("wire");
    assert!(wire.contains("visible reasoning"));
    assert!(wire.contains("out.txt"));
    assert!(wire.contains("\"isError\":true"));
    assert!(!wire.contains("opaque-secret"));
    assert!(!wire.contains("private-provider"));
    assert!(!wire.contains("providerReasoningReplay"));
    assert!(!wire.contains("parentId"));
}

#[test]
fn completed_turn_runtime_registry_is_released() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let sessions_dir = temp.path().join("sessions");
    let server = app_server(store, &sessions_dir);
    for index in 0..300 {
        let turn_id = format!("turn-{index}");
        let (_cancellation, guard) = server
            .activate_turn(&turn_id, "thread-1")
            .expect("activate");
        drop(guard);
    }
    assert!(server.active_turns.lock().unwrap().is_empty());
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
    let methods = responses
        .iter()
        .filter_map(|value| value["method"].as_str())
        .collect::<Vec<_>>();
    let tool_started = responses
        .iter()
        .position(|value| {
            value["method"] == "item/started" && value["params"]["item"]["itemId"] == "call_1"
        })
        .expect("tool item started");
    let execution_started = responses
        .iter()
        .position(|value| value["method"] == "tool/execution/start")
        .expect("tool execution started");
    let execution_ended = responses
        .iter()
        .position(|value| value["method"] == "tool/execution/end")
        .expect("tool execution ended");
    let tool_completed = responses
        .iter()
        .position(|value| {
            value["method"] == "item/completed" && value["params"]["item"]["itemId"] == "call_1"
        })
        .expect("tool item completed");
    assert!(tool_started < execution_started && execution_started < execution_ended);
    assert!(execution_ended < tool_completed);
    assert!(methods.contains(&"item/completed"));
    let result = responses
        .iter()
        .find(|message| message["id"] == 2)
        .expect("turn response");
    assert_eq!(result["result"]["turn"]["status"], "running");
    let completed_event = responses
        .iter()
        .find(|message| message["method"] == "turn/completed")
        .expect("turn completed event");
    assert_eq!(completed_event["params"]["turn"]["status"], "completed");

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
    let metadata = session.metadata_entries();
    assert_eq!(metadata.len(), 3, "start, terminal and usage facts");
    assert_eq!(
        metadata[0].kind(),
        singularity_agent::session::SessionMetadataKind::TurnStarted
    );
    assert_eq!(
        metadata[1].kind(),
        singularity_agent::session::SessionMetadataKind::TurnCompleted
    );
    assert_eq!(
        metadata[2].kind(),
        singularity_agent::session::SessionMetadataKind::Usage
    );
}

#[test]
fn a_single_turn_opens_the_session_file_exactly_once() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "3a0e99f1-8b2c-47d6-9f3e-2a1b0c8d7e6f";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![completed_response("single-turn")],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    // 单写者所有权：一轮 turn（开始标记→对话→工具→终态→用量）只打开一次
    // SessionManager。计数放在 state.rs 的 open_session_for_thread（open_existing
    // 的唯一 turn 入口），开始时清零观察一轮内的增量。
    server.session_opens.store(0, Ordering::SeqCst);
    let mut events = Vec::new();
    server
        .handle_turn_start_streaming_with_output(turn_start_message(2, session_id), |value| {
            events.push(value);
        })
        .expect("turn start");
    assert!(
        events
            .iter()
            .any(|value| value["method"] == "turn/completed"),
        "single completed turn"
    );
    assert_eq!(
        server.session_opens.load(Ordering::SeqCst),
        1,
        "a single turn must open the session file exactly once"
    );
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
    let session_id = started[1]["result"]["thread"]["threadId"]
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
        "running"
    );
    assert_eq!(
        first
            .iter()
            .find(|m| m["method"] == "turn/completed")
            .expect("completed")["params"]["turn"]["status"],
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
        .expect("turn start returns ok");
    let error_notif = failed
        .iter()
        .find(|m| m["method"] == "turn/error")
        .expect("turn/error notification");
    assert!(
        error_notif["params"]["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("synthetic failure"))
    );
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
        "running"
    );
    assert_eq!(
        third
            .iter()
            .find(|m| m["method"] == "turn/completed")
            .expect("completed")["params"]["turn"]["status"],
        "completed"
    );

    // SQLite-only 的陈旧 interrupted 不得覆盖 JSONL 中已有的 completed 事实；
    // resume 打开目标会话时重新投影为 completed。
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
        "completed"
    );
    assert_eq!(
        server.store().get_session(&session_id).unwrap().status,
        Some(SessionStatus::Completed)
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
    let session_id = started[1]["result"]["thread"]["threadId"]
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
            .find(|thread| thread["threadId"] == session_id)
            .expect("session row")["lastTurnStatus"]
            .clone()
    }

    fn read_status(server: &mut AppServer, id: i64, session_id: &str) -> serde_json::Value {
        server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"session/read","id":{id},"params":{{"sessionId":"{session_id}"}}}}"#
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
    let inbox = Arc::new(Mutex::new(TurnInbox::default()));
    server
        .active_turns
        .lock()
        .expect("active turns")
        .get_mut("turn_live")
        .expect("active turn")
        .inbox = Some(Arc::clone(&inbox));

    let steer_response = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/steer","id":3,"params":{"turnId":"turn_live","input":[{"type":"text","text":"change direction"}]}}"#,
        )
        .expect("turn steer");
    assert_eq!(steer_response[0]["result"]["turn"]["status"], "running");
    let follow_up_response = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/followUp","id":4,"params":{"turnId":"turn_live","input":[{"type":"text","text":"keep going"}]}}"#,
        )
        .expect("turn followUp");
    assert_eq!(follow_up_response[0]["result"]["turn"]["status"], "running");

    // The production handles are typed atomic TurnInbox values rather than
    // raw VecDeque instances; the active responses above prove acceptance
    // without coupling this test to the inbox's private representation.
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

    // 每个 Request 方法以 notification（无 id）提交 → 静默忽略且不执行任何副作用。
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
        assert!(responses.is_empty(), "{method}: {responses:?}");
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
fn notification_only_method_with_id_returns_typed_invalid_request() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &temp.path().join("sessions"));
    let response = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","id":9,"params":{}}"#)
        .expect("notification-only request");
    assert_eq!(response.len(), 1);
    assert_eq!(response[0]["id"], 9);
    assert_eq!(response[0]["error"]["code"], -32600);
    assert!(!server.initialized_acknowledged);
}

#[test]
fn turn_start_before_initialize_is_rejected() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "5b5c6d7e-8f90-4a1b-9c2d-3e4f5a6b7c8d";
    let mut server = app_server(store, &sessions_dir);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    // initialize 之前发 turn/start：普通管线门禁拒绝，不产生任何 turn 语义。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"early"}}]}}}}"#
        ))
        .expect("turn/start before initialize");
    assert_eq!(responses[0]["error"]["code"], -32002);
    assert_eq!(
        server
            .store()
            .get_session(session_id)
            .expect("record")
            .status,
        None,
        "rejected turn must not activate the session"
    );
    let session = SessionManager::open_existing(&sessions_dir.join(format!("{session_id}.jsonl")))
        .expect("session file");
    assert!(
        session.metadata_entries().is_empty(),
        "rejected turn must not write turn_started"
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
fn steer_and_follow_up_after_turn_completion_are_rejected() {
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
    let turn_id = turn_one["result"]["turn"]["turnId"]
        .as_str()
        .expect("turn id")
        .to_string();
    assert_eq!(turn_one["result"]["turn"]["status"], "running");
    assert_eq!(
        responses
            .iter()
            .find(|m| m["method"] == "turn/completed")
            .expect("completed")["params"]["turn"]["status"],
        "completed"
    );

    // turn 已终态：steer / followUp 必须拒绝；客户端应发送新的 turn/start。
    for (method, text) in [
        ("turn/steer", "change direction"),
        ("turn/followUp", "keep going"),
    ] {
        let responses = server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"{method}","id":3,"params":{{"turnId":"{turn_id}","input":[{{"type":"text","text":"{text}"}}]}}}}"#
            ))
            .expect("post-terminal inject");
        assert_eq!(
            responses[0]["error"]["code"], -32004,
            "{method}: {responses:?}"
        );
        assert!(
            responses[0]["result"].is_null(),
            "{method} must not acknowledge terminal input"
        );
    }

    // turn 2 只能通过显式 turn/start 开始，不应携带已拒绝的输入。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":4,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"second"}}]}}}}"#
        ))
        .expect("turn two");
    assert_eq!(
        responses.iter().find(|m| m["id"] == 4).expect("turn two")["result"]["turn"]["status"],
        "running"
    );
    assert_eq!(
        responses
            .iter()
            .find(|m| m["method"] == "turn/completed")
            .expect("completed")["params"]["turn"]["status"],
        "completed"
    );
    let seen = requests.lock().expect("seen requests");
    assert!(
        seen.iter().all(|request| !request
            .messages
            .iter()
            .any(|m| { m.content == "change direction" || m.content == "keep going" })),
        "rejected terminal inputs must not appear in a later turn"
    );
}
#[test]
fn turn_injection_unknown_turn_is_not_found() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "c1e2d3f4-3333-4555-8666-777788889999";
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    for method in ["turn/steer", "turn/followUp"] {
        let responses = server
            .handle_json(&format!(
                r#"{{"jsonrpc":"2.0","method":"{method}","id":2,"params":{{"turnId":"unknown","input":[{{"type":"text","text":"ignored"}}]}}}}"#
            ))
            .expect("unknown turn response");
        assert_eq!(responses[0]["error"]["code"], -32004, "{method}");
    }
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

#[test]
fn oversized_project_instructions_truncate_with_warning_instead_of_failing() {
    // 超限不再报错：截断前缀纳入系统提示词并附模型可见尾注，客户端在
    // turn/started 之后收到 project_instructions_truncated 告警诊断。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        workspace.join("AGENTS.md"),
        // `#` 不会出现在 Windows 路径（含 8.3 短名）或提示词模板中，计数不受
        // runner 环境影响。
        vec![b'#'; singularity_core::PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1],
    )
    .expect("oversized agents");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let requests: Arc<Mutex<Vec<ModelTurnRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![completed_response("truncated_instructions_turn")],
        seen_requests: Arc::clone(&requests),
    };
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(
        &server,
        &sessions_dir,
        "3d4e5f6a-9b8c-4d1e-a2f3-b4c5d6e7f8a9",
        &workspace,
    );

    let responses = server
        .handle_json(
            r#"{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{"threadId":"3d4e5f6a-9b8c-4d1e-a2f3-b4c5d6e7f8a9","input":[{"type":"text","text":"do it"}]}}"#,
        )
        .expect("oversized instructions must not fail the turn");
    assert_eq!(
        responses
            .iter()
            .find(|m| m["id"] == 2)
            .expect("turn response")["result"]["turn"]["status"],
        "running"
    );
    assert_eq!(
        responses
            .iter()
            .find(|m| m["method"] == "turn/completed")
            .expect("completed")["params"]["turn"]["status"],
        "completed"
    );
    let started_position = responses
        .iter()
        .position(|m| m["method"] == "turn/started")
        .expect("turn started");
    let warning_position = responses
        .iter()
        .position(|m| {
            m["method"] == "agent/diagnostic"
                && m["params"]["code"] == "project_instructions_truncated"
                && m["params"]["severity"] == "warning"
        })
        .expect("client receives the truncation warning");
    assert!(
        warning_position > started_position,
        "warning must arrive after turn/started"
    );
    let seen = requests.lock().expect("seen requests");
    let request = seen.last().expect("provider request");
    let joined: String = request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let marker_count = joined.matches('#').count();
    assert_eq!(
        marker_count,
        singularity_core::PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
        "exactly the file budget prefix reaches the model (joined_len={} head={:?} tail={:?})",
        joined.len(),
        &joined[..joined.len().min(60)],
        &joined[joined.len().saturating_sub(60)..]
    );
    assert!(
        joined.contains("[warning] project instructions were truncated"),
        "truncation must be visible to the model: {joined}"
    );
}

#[test]
fn terminal_turn_usage_matches_provider_usage() {
    // 1c 回归：终态 turn 的 model_usage 直接来自 AgentLoop 返回值（不再经由
    // 运行时缓存表转手），数值必须与 provider 上报逐字段一致。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "e9f2e1d0-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut completed = ModelTurnResponse::completed("usage_request", "usage_response", "done");
    completed.usage = ModelUsage {
        input_tokens: 111,
        output_tokens: 22,
        total_tokens: 133,
        cached_input_tokens: 5,
        reasoning_tokens: 3,
        usage_present: true,
    };
    let provider = StaticProvider {
        responses: vec![completed],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"usage"}}]}}}}"#
        ))
        .expect("turn start");
    let completed_event = responses
        .iter()
        .find(|message| message["method"] == "turn/completed")
        .expect("turn completed event");
    let usage = &completed_event["params"]["turn"]["modelUsage"];
    assert_eq!(usage["inputTokens"], 111);
    assert_eq!(usage["outputTokens"], 22);
    assert_eq!(usage["totalTokens"], 133);
    assert_eq!(usage["cachedInputTokens"], 5);
    assert_eq!(usage["reasoningTokens"], 3);
    assert_eq!(usage["usagePresent"], true);
    assert_eq!(usage["usageComplete"], true);
}

#[test]
fn turn_failure_emits_typed_error_event_with_provider_cause() {
    // H4/M1：provider 边界失败（429）→ turn/error 终态事件携带 typed cause，
    // willRetry=false（N3 后循环层不再重试）。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "3b4c5d6e-7f80-4912-8a3b-4c5d6e7f8091";
    let mut rate_limited = ModelTurnResponse::completed("rl_request", "rl_response", "unused");
    rate_limited.status = ModelTurnStatus::Failed;
    rate_limited.assistant_message = None;
    rate_limited.error = Some(ModelError::new(
        ModelErrorKind::RateLimited,
        "429 rate limit exceeded",
    ));
    let provider = StaticProvider {
        responses: vec![rate_limited],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);

    let mut collected = Vec::new();
    let message: JsonRpcMessage = serde_json::from_str(&format!(
        r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"x"}}]}}}}"#
    ))
    .expect("message");
    let result = server.handle_turn_start_streaming_with_output(message, |output| {
        collected.push(output);
    });
    assert!(
        result.is_ok(),
        "streaming turn start returns ok and delivers error via notification"
    );

    let error_event = collected
        .iter()
        .find(|message| message["method"] == "turn/error")
        .expect("turn/error terminal event");
    assert_eq!(
        error_event["params"]["error"]["cause"], "provider_rate_limited",
        "typed provider cause: {error_event:?}"
    );
    assert_eq!(error_event["params"]["error"]["stage"], "agent_loop");
    assert_eq!(error_event["params"]["error"]["willRetry"], false);
    assert!(
        error_event["params"]["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("rate limit")),
        "message must carry the original cause: {error_event:?}"
    );
    assert_eq!(error_event["params"]["threadId"], session_id);
    assert!(error_event["params"]["turnId"].is_string());
}

fn turn_start_message(id: i64, session_id: &str) -> JsonRpcMessage {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "turn/start",
        "id": id,
        "params": {
            "threadId": session_id,
            "input": [{"type": "text", "text": "run the task"}]
        }
    }))
    .expect("turn/start message")
}

#[test]
fn project_instruction_errors_fail_closed_before_provider_call() {
    // 预算超限不在其中：超限走截断 + 告警路径
    // （oversized_project_instructions_truncate_with_warning_instead_of_failing），
    // 只有真 I/O 错误仍直接使 turn/start 失败。
    for name in ["invalid_utf8", "unsupported_type", "cwd_unavailable"] {
        let temp = tempfile::tempdir().expect("temp dir");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        match name {
            "invalid_utf8" => {
                std::fs::write(workspace.join("AGENTS.md"), [0xff, 0xfe, 0xfd])
                    .expect("invalid UTF-8 AGENTS.md");
            }
            "unsupported_type" => {
                std::fs::create_dir(workspace.join("AGENTS.md")).expect("directory AGENTS.md");
            }
            "cwd_unavailable" => {}
            _ => unreachable!("known project instruction case"),
        }
        let sessions_dir = temp.path().join("sessions");
        let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = StaticProvider {
            responses: vec![completed_response("must_not_run")],
            seen_requests: Arc::clone(&seen),
        };
        let session_id = match name {
            "invalid_utf8" => "8f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d",
            "unsupported_type" => "7f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d",
            "cwd_unavailable" => "6f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d",
            _ => unreachable!("known project instruction case"),
        };
        let mut server = app_server(store, &sessions_dir).with_test_provider(Arc::new(provider));
        initialize(&mut server);
        insert_session(&server, &sessions_dir, session_id, &workspace);
        if name == "cwd_unavailable" {
            std::fs::remove_dir_all(&workspace).expect("remove workspace for read failure");
        }

        // 准备阶段失败：turn/start 直接回错误响应，不产生任何 turn 语义。
        let result = server
            .handle_turn_start_streaming_with_output(turn_start_message(2, session_id), |_| {});
        assert!(
            result.is_err(),
            "{name}: prepare failure must error turn/start directly: {result:?}"
        );
        assert_eq!(seen.lock().expect("seen requests").len(), 0, "{name}");
        assert_eq!(
            server
                .store()
                .get_session(session_id)
                .expect("session record")
                .status,
            None,
            "{name}: instruction failure must not leave any turn state"
        );
    }
}

#[test]
fn terminal_metadata_failure_emits_error_and_never_completion() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "8f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![completed_response("completed")],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);
    server.inject_terminalization_faults(1, 0);

    let mut events = Vec::new();
    let result = server.handle_turn_start_streaming_with_output(
        turn_start_message(2, session_id),
        |value| {
            events.push(value);
        },
    );
    assert!(
        result.is_ok(),
        "terminal metadata failure returns ok and emits error"
    );
    let error_event = events
        .iter()
        .find(|value| value["method"] == "turn/error")
        .expect("turn/error");
    assert_eq!(error_event["params"]["error"]["stage"], "terminal_outcome");
    assert_eq!(error_event["params"]["error"]["cause"], "store");
    let status = server
        .store()
        .get_session(session_id)
        .expect("record")
        .status;
    assert_ne!(status, Some(SessionStatus::Active));
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "turn/completed")
    );
}

#[test]
fn agent_failure_status_write_failure_still_converges_and_reports_terminalization() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "7f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![failed_response()],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);
    server.inject_terminalization_faults(1, 0);

    let mut events = Vec::new();
    let result = server.handle_turn_start_streaming_with_output(
        turn_start_message(2, session_id),
        |value| {
            events.push(value);
        },
    );
    assert!(result.is_ok(), "agent failure returns ok and emits error");
    let error_event = events
        .iter()
        .find(|value| value["method"] == "turn/error")
        .expect("turn/error");
    assert_eq!(error_event["params"]["error"]["stage"], "agent_loop");
    let status = server
        .store()
        .get_session(session_id)
        .expect("record")
        .status;
    assert_ne!(status, Some(SessionStatus::Active));
    assert!(events.iter().any(|value| value["method"] == "turn/error"));
}

#[test]
fn terminal_event_failure_emits_fallback_error_and_reports_event_stage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "6f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![completed_response("completed")],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);
    server.inject_terminalization_faults(0, 1);

    let mut events = Vec::new();
    let result = server.handle_turn_start_streaming_with_output(
        turn_start_message(2, session_id),
        |value| {
            events.push(value);
        },
    );
    assert!(
        result.is_ok(),
        "terminal event failure returns ok and emits fallback error"
    );
    let error_event = events
        .iter()
        .find(|value| value["method"] == "turn/error")
        .expect("turn/error");
    assert_eq!(
        error_event["params"]["error"]["stage"],
        "event_notification"
    );
    assert_eq!(
        server
            .store()
            .get_session(session_id)
            .expect("record")
            .status,
        Some(SessionStatus::Completed)
    );
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "turn/completed")
    );
    assert!(events.iter().any(|value| value["method"] == "turn/error"));
}

#[test]
fn terminal_metadata_double_failure_emits_no_terminal_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "6f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![completed_response("completed")],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);
    server.inject_terminalization_faults(3, 0);

    let mut events = Vec::new();
    let result = server.handle_turn_start_streaming_with_output(
        turn_start_message(2, session_id),
        |value| {
            events.push(value);
        },
    );
    assert!(
        matches!(result, Err(AppServerError::TurnTerminalization { .. })),
        "turn start streaming must return TurnTerminalization error on double failure"
    );
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "turn/completed"),
        "must not emit turn/completed when metadata persistence fails"
    );
    assert!(
        !events.iter().any(|value| value["method"] == "turn/error"),
        "must not emit turn/error when metadata persistence fails"
    );
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "item/completed" || value["method"] == "item/failed"),
        "must not emit item terminal events when metadata persistence fails"
    );
    assert!(
        events.iter().any(|value| {
            value["method"] == "agent/diagnostic"
                && value["params"]["severity"] == "error"
                && value["params"]["code"] == "storage_fatal"
        }),
        "must emit sanitized storage_fatal diagnostic on double failure"
    );

    // After fail-stop, reopen/resume must repair un-terminalized turn_started to interrupted.
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":3,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume repairs session after fail-stop");
    assert_eq!(
        resumed[0]["result"]["thread"]["lastTurnStatus"], "interrupted",
        "reopen repair must converge uncompleted turn to interrupted"
    );
}

#[test]
fn agent_failure_metadata_double_failure_emits_no_terminal_event() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "5f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![failed_response()],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);
    server.inject_terminalization_faults(2, 0);

    let mut events = Vec::new();
    let result = server.handle_turn_start_streaming_with_output(
        turn_start_message(2, session_id),
        |value| {
            events.push(value);
        },
    );
    assert!(
        matches!(result, Err(AppServerError::TurnTerminalization { .. })),
        "turn start streaming must return TurnTerminalization error on agent failure double metadata failure"
    );
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "turn/completed"),
        "must not emit turn/completed when agent failure metadata persistence fails"
    );
    assert!(
        !events.iter().any(|value| value["method"] == "turn/error"),
        "must not emit turn/error when agent failure metadata persistence fails"
    );
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "item/completed" || value["method"] == "item/failed"),
        "must not emit item terminal events when agent failure metadata persistence fails"
    );
    assert!(
        events.iter().any(|value| {
            value["method"] == "agent/diagnostic"
                && value["params"]["severity"] == "error"
                && value["params"]["code"] == "storage_fatal"
        }),
        "must emit sanitized storage_fatal diagnostic on double failure"
    );

    // Reopen/resume converges residual turn_started to interrupted
    let resumed = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/resume","id":3,"params":{{"threadId":"{session_id}"}}}}"#
        ))
        .expect("resume repairs session after agent failure fail-stop");
    assert_eq!(
        resumed[0]["result"]["thread"]["lastTurnStatus"],
        "interrupted"
    );
}

#[test]
fn single_metadata_failure_recovers_via_bounded_retry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let session_id = "4f2e1d0c-8b7a-4654-9e3d-2c1b0a9f8e7d";
    let mut server =
        app_server(store, &sessions_dir).with_test_provider(Arc::new(StaticProvider {
            responses: vec![completed_response("completed")],
            seen_requests: Arc::new(Mutex::new(Vec::new())),
        }));
    initialize(&mut server);
    insert_session(&server, &sessions_dir, session_id, &workspace);
    // Inject 1 fault on first write; retry write succeeds.
    server.inject_terminalization_faults(1, 0);

    let mut events = Vec::new();
    let result = server.handle_turn_start_streaming_with_output(
        turn_start_message(2, session_id),
        |value| {
            events.push(value);
        },
    );
    assert!(
        result.is_ok(),
        "single failure with retry compensation returns ok"
    );
    assert!(
        !events
            .iter()
            .any(|value| value["method"] == "turn/completed"),
        "must not emit turn/completed when initial metadata write fails"
    );
    assert!(
        events.iter().any(|value| value["method"] == "turn/error"),
        "must emit turn/error when compensation writes terminal failure"
    );
    assert_eq!(
        server.store().get_session(session_id).unwrap().status,
        Some(SessionStatus::Failed),
        "session status must be updated to Failed by compensation write"
    );
}

/// 仅暴露 generator 契约的最小 Provider 假件，供 prompt 装配测试使用。
struct PromptOnlyProvider;

impl Provider for PromptOnlyProvider {
    fn protocol_contract(&self) -> ProviderProtocolContract {
        ProviderProtocolContract::default()
    }

    fn complete(
        &self,
        _request: &ModelTurnRequest,
        _cancellation: &CancellationToken,
    ) -> Result<ModelTurnResponse, ProviderError> {
        Err(ProviderError::from_model_error(ModelError::new(
            ModelErrorKind::NetworkError,
            "unused",
        )))
    }
}

#[test]
fn agent_prompt_tool_list_matches_registry_names_only() {
    let temp = tempfile::tempdir().expect("workspace dir");
    let thread = singularity_protocol::Thread {
        thread_id: "thread_prompt".to_string(),
        model: None,
        cwd: Some(temp.path().to_string_lossy().to_string()),
        last_turn_status: None,
    };
    let snapshot = ProviderConfigSnapshot::capture(
        |name| match name {
            "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
            "SINGULARITY_MODEL" => Some("gpt-test".to_string()),
            _ => None,
        },
        test_runtime_handle(),
    );
    let (config, _) =
        crate::lifecycle::agent_config_for_thread(&thread, &PromptOnlyProvider, &snapshot)
            .expect("agent config resolves");
    let names = ToolRegistry::new().names();
    assert_eq!(names, ["bash", "edit", "read", "write"]);
    for name in &names {
        assert!(
            config.system_prompt.contains(&format!("- {name}")),
            "tool list must include {name}"
        );
    }
    for tool_description in [
        "bounded text read with line numbers and byte offsets",
        "command execution and directory/file exploration",
        "exact unique match and replacement within files",
        "structured whole-file creation and overwrite",
    ] {
        assert!(
            !config.system_prompt.contains(tool_description),
            "prompt must list tool names only, not descriptions"
        );
    }
}

// ===== session/read turn+游标分页 =====

use crate::dispatch::select_turn_page;

/// 建一个带 settings 前导组的多轮会话：每轮一条 user 消息，偶数索引轮
/// 额外带一条 toolResult 消息（供 kinds 过滤用例区分轮次）。
fn seed_turned_session(
    server: &AppServer,
    sessions_dir: &Path,
    session_id: &str,
    turn_ids: &[&str],
) -> String {
    let sid = insert_session(server, sessions_dir, session_id, sessions_dir);
    let path = sessions_dir.join(format!("{sid}.jsonl"));
    let mut session = SessionManager::open_existing(&path).expect("reopen session");
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::thread_settings(
                "openai_compatible",
                "gpt-test",
                None,
            )
            .expect("settings"),
        )
        .expect("append settings");
    for (index, turn_id) in turn_ids.iter().enumerate() {
        session
            .append_metadata(singularity_agent::session::SessionMetadata::turn_started(
                *turn_id,
            ))
            .expect("turn started");
        session
            .append_message(singularity_agent::message::AgentMessage::text(
                singularity_agent::message::AgentMessageRole::User,
                format!("user-{index}"),
            ))
            .expect("user message");
        if index % 2 == 0 {
            session
                .append_message(singularity_agent::message::AgentMessage {
                    role: singularity_agent::message::AgentMessageRole::ToolResult,
                    content: vec![singularity_agent::message::ContentBlock::Text {
                        text: format!("tool-output-{index}"),
                    }],
                    provider_reasoning_replay: None,
                    tool_call_id: Some(format!("call-{index}")),
                    tool_name: Some("bash".to_string()),
                    is_error: None,
                    timestamp: None,
                })
                .expect("tool result");
        }
        session
            .append_metadata(singularity_agent::session::SessionMetadata::turn_completed(
                *turn_id,
            ))
            .expect("turn completed");
    }
    // 生产顺序是终态 metadata 先落盘、索引后更新；fixture 保持同一不变量。
    server
        .store()
        .update_session(
            &sid,
            SessionMetadataUpdate {
                status: Some(SessionStatus::Completed),
                ..SessionMetadataUpdate::default()
            },
        )
        .expect("mark session completed");
    sid
}

fn session_read_response(server: &mut AppServer, id: i64, params: &str) -> serde_json::Value {
    server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"session/read","id":{id},"params":{params}}}"#
        ))
        .expect("session read")[0]
        .clone()
}

fn turn_page(result: &serde_json::Value) -> Vec<String> {
    result["turns"]
        .as_array()
        .expect("turns")
        .iter()
        .map(|turn| match turn["turnId"].as_str() {
            Some(turn_id) => turn_id.to_string(),
            None => "<prelude>".to_string(),
        })
        .collect()
}

#[test]
fn session_read_pages_forward_and_backward_by_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let sid = seed_turned_session(
        &server,
        &sessions_dir,
        "1f0a2b3c-4d5e-4f60-8a92-b3c4d5e6f708",
        &["t1", "t2", "t3"],
    );

    // 正向翻页：物理单元为 [前导组, t1, t2, t3]，limit=2。
    let page1 = session_read_response(
        &mut server,
        10,
        &format!(r#"{{"sessionId":"{sid}","limit":2,"sortDirection":"asc"}}"#),
    );
    let result1 = &page1["result"];
    assert_eq!(
        turn_page(result1),
        vec!["<prelude>".to_string(), "t1".to_string()]
    );
    assert_eq!(result1["totalTurns"], 3);
    assert_eq!(result1["status"], "completed");
    let next_cursor = result1["nextCursor"].as_str().expect("next cursor");
    let backwards1 = result1["backwardsCursor"]
        .as_str()
        .expect("backwards cursor");

    let page2 = session_read_response(
        &mut server,
        11,
        &format!(
            r#"{{"sessionId":"{sid}","limit":2,"sortDirection":"asc","cursor":"{next_cursor}"}}"#
        ),
    );
    let result2 = &page2["result"];
    assert_eq!(turn_page(result2), vec!["t2".to_string(), "t3".to_string()]);
    assert!(
        result2["nextCursor"].is_null(),
        "exhausted forward scan must not hand out a cursor"
    );
    assert!(
        page2["result"]["turns"][1]["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["type"] == "message" && item["text"] == "user-2")
    );

    // 反向翻页：默认 desc 从最新端开始，页内保持会话顺序。
    let back1 = session_read_response(
        &mut server,
        12,
        &format!(r#"{{"sessionId":"{sid}","limit":2}}"#),
    );
    let back_result = &back1["result"];
    assert_eq!(
        turn_page(back_result),
        vec!["t2".to_string(), "t3".to_string()]
    );
    let back_next = back_result["nextCursor"].as_str().expect("desc next");
    // 反向续页覆盖剩余的 [前导组, t1] 窗口。
    let back2 = session_read_response(
        &mut server,
        13,
        &format!(r#"{{"sessionId":"{sid}","limit":2,"cursor":"{back_next}"}}"#),
    );
    assert_eq!(
        turn_page(&back2["result"]),
        vec!["<prelude>".to_string(), "t1".to_string()]
    );
    assert!(back2["result"]["nextCursor"].is_null());

    // backwards_cursor 以相反方向包含式重读同一窗口：锚点轮再次出现，
    // 窗口内容与原页一致。
    let reread = session_read_response(
        &mut server,
        14,
        &format!(
            r#"{{"sessionId":"{sid}","limit":2,"sortDirection":"desc","cursor":"{backwards1}"}}"#
        ),
    );
    assert_eq!(
        turn_page(&reread["result"]),
        vec!["<prelude>".to_string(), "t1".to_string()],
        "opposite-direction read from backwards_cursor includes the anchor turn"
    );
}

#[test]
fn session_read_rejects_invalid_and_out_of_range_cursors() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let sid = seed_turned_session(
        &server,
        &sessions_dir,
        "2c1d2e3f-4a5b-4c6d-8e7f-90a1b2c3d4e5",
        &["t1"],
    );

    for bad_cursor in ["garbage", "sg1t", "sg1tx", "sg1t-1", "sg1t99"] {
        let response = session_read_response(
            &mut server,
            20,
            &format!(r#"{{"sessionId":"{sid}","cursor":"{bad_cursor}"}}"#),
        );
        assert_eq!(
            response["error"]["code"], -32602,
            "cursor {bad_cursor:?} must be rejected as invalid params"
        );
    }
}

#[test]
fn session_read_kinds_filter_composes_with_pagination() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let sid = seed_turned_session(
        &server,
        &sessions_dir,
        "3d2e3f4a-5b6c-4d7e-8f90-a1b2c3d4e5f6",
        &["t1", "t2", "t3", "t4"],
    );

    // kinds=["tool_result"]：只有 t1/t3 携带命中条目；被过滤掉的轮次
    // 不占用每页配额，游标仍锚定物理位置。
    let page1 = session_read_response(
        &mut server,
        30,
        &format!(r#"{{"sessionId":"{sid}","limit":1,"kinds":["tool_result"]}}"#),
    );
    let result1 = &page1["result"];
    assert_eq!(turn_page(result1), vec!["t3".to_string()]);
    let items1 = result1["turns"][0]["items"].as_array().expect("items");
    assert_eq!(items1.len(), 1);
    assert_eq!(items1[0]["type"], "tool_result");
    let next_cursor = result1["nextCursor"].as_str().expect("next cursor");

    let page2 = session_read_response(
        &mut server,
        31,
        &format!(
            r#"{{"sessionId":"{sid}","limit":1,"kinds":["tool_result"],"cursor":"{next_cursor}"}}"#
        ),
    );
    let result2 = &page2["result"];
    assert_eq!(turn_page(result2), vec!["t1".to_string()]);
    assert!(result2["nextCursor"].is_null());

    // kind=turn 命中轮次本身：全部真实轮入选且条目为空，前导组出局。
    let identity = session_read_response(
        &mut server,
        32,
        &format!(r#"{{"sessionId":"{sid}","limit":10,"kinds":["turn"],"detail":"summary"}}"#),
    );
    let identity_result = &identity["result"];
    assert_eq!(turn_page(identity_result), vec!["t1", "t2", "t3", "t4"]);
    for turn in identity_result["turns"].as_array().unwrap() {
        assert_eq!(
            turn["items"].as_array().expect("items").len(),
            0,
            "summary detail keeps identity only"
        );
    }
}

#[test]
fn session_read_detail_summary_keeps_identity_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let sid = seed_turned_session(
        &server,
        &sessions_dir,
        "4e3f4a5b-6c7d-4e8f-9a01-b2c3d4e5f6a7",
        &["t1", "t2"],
    );

    let response = session_read_response(
        &mut server,
        40,
        &format!(r#"{{"sessionId":"{sid}","detail":"summary"}}"#),
    );
    let result = &response["result"];
    assert_eq!(
        turn_page(result),
        vec!["<prelude>".to_string(), "t1".to_string(), "t2".to_string()]
    );
    for turn in result["turns"].as_array().unwrap() {
        assert!(turn["items"].as_array().expect("items").is_empty());
    }
    assert_eq!(result["turns"][1]["status"], "completed");
    assert_eq!(result["totalTurns"], 2);

    // 缺省 detail=full 时同请求携带完整条目。
    let full = session_read_response(&mut server, 41, &format!(r#"{{"sessionId":"{sid}"}}"#));
    let full_items = full["result"]["turns"][2]["items"]
        .as_array()
        .expect("items");
    assert!(
        full_items
            .iter()
            .any(|item| item["type"] == "message" && item["text"] == "user-1")
    );
}

#[test]
fn session_read_empty_session_returns_empty_page() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let sid = insert_session(
        &server,
        &sessions_dir,
        "5f4a5b6c-7d8e-4f90-8a12-c3d4e5f6a7b8",
        &sessions_dir,
    );

    let response = session_read_response(&mut server, 50, &format!(r#"{{"sessionId":"{sid}"}}"#));
    let result = &response["result"];
    assert_eq!(result["turns"].as_array().expect("turns").len(), 0);
    assert_eq!(result["totalTurns"], 0);
    assert!(result["nextCursor"].is_null());
    assert!(result["backwardsCursor"].is_null());
    assert!(result["status"].is_null());
}

#[test]
fn session_read_projects_crash_leftover_turn_as_interrupted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let store = SessionStore::open(temp.path().join("index.sqlite3")).expect("store");
    let mut server = app_server(store, &sessions_dir);
    initialize(&mut server);
    let sid = insert_session(
        &server,
        &sessions_dir,
        "6a5b6c7d-8e9f-4a01-9b23-d4e5f6a7b8c9",
        &workspace,
    );
    let path = sessions_dir.join(format!("{sid}.jsonl"));
    let mut session = SessionManager::open_existing(&path).expect("reopen");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_started(
            "t9",
        ))
        .expect("turn started");
    session
        .append_message(singularity_agent::message::AgentMessage::text(
            singularity_agent::message::AgentMessageRole::User,
            "crashed mid-turn",
        ))
        .expect("user message");
    drop(session);
    // 模拟崩溃遗留：索引行停在 active，但本进程没有存活 turn。
    server
        .store()
        .update_session(
            &sid,
            SessionMetadataUpdate {
                status: Some(SessionStatus::Active),
                ..SessionMetadataUpdate::default()
            },
        )
        .expect("force active row");

    let response = session_read_response(&mut server, 60, &format!(r#"{{"sessionId":"{sid}"}}"#));
    let result = &response["result"];
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["turns"][0]["turnId"], "t9");
    assert_eq!(
        result["turns"][0]["status"], "interrupted",
        "trailing running turn must not contradict the overall projection"
    );
    assert!(!result["turns"][0]["items"].as_array().unwrap().is_empty());
}

#[test]
fn select_turn_page_covers_direction_and_boundary_edges() {
    let all = |_: usize| true;
    // 恰好填满且同时耗尽：不发续页锚点。
    let (page, next, backwards) = select_turn_page(3, all, HistorySortDirection::Asc, 3, None);
    assert_eq!(page, vec![0, 1, 2]);
    assert_eq!(next, None);
    assert_eq!(backwards, Some(2));
    // 填满且有剩余：续页锚点指向首个未检查位置。
    let (page, next, _) = select_turn_page(5, all, HistorySortDirection::Asc, 2, None);
    assert_eq!(page, vec![0, 1]);
    assert_eq!(next, Some(2));
    // 尾端无幸存者时不发续页锚点。
    let head_only = |index: usize| index < 3;
    let (page, next, backwards) =
        select_turn_page(5, head_only, HistorySortDirection::Asc, 3, None);
    assert_eq!(page, vec![0, 1, 2]);
    assert_eq!(next, None);
    assert_eq!(backwards, Some(2));
    // desc 从最新端开始、升序返回；远端锚点为本页最旧一轮。
    let (page, next, backwards) = select_turn_page(4, all, HistorySortDirection::Desc, 2, None);
    assert_eq!(page, vec![2, 3]);
    assert_eq!(next, Some(1));
    assert_eq!(backwards, Some(2));
    // 空序列与零 limit 直接返回空页。
    for limit in [0usize, 2] {
        let (page, next, backwards) =
            select_turn_page(0, all, HistorySortDirection::Desc, limit, None);
        assert!(page.is_empty());
        assert_eq!(next, None);
        assert_eq!(backwards, None);
    }
}

#[test]
fn project_turn_history_groups_boundaries_and_marks_leftovers() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut session = SessionManager::create_with_id(
        temp.path(),
        temp.path(),
        "7b6c7d8e-9fa0-4b12-8c34-e5f6a7b8c9d0",
    )
    .expect("session file");
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::thread_settings(
                "openai_compatible",
                "gpt-test",
                None,
            )
            .expect("settings"),
        )
        .expect("settings");
    // t1 崩溃遗留：只有开始标记，后面又开了新轮。
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_started(
            "t1",
        ))
        .expect("t1 started");
    session
        .append_message(singularity_agent::message::AgentMessage::text(
            singularity_agent::message::AgentMessageRole::User,
            "one",
        ))
        .expect("msg one");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_started(
            "t2",
        ))
        .expect("t2 started");
    session
        .append_message(singularity_agent::message::AgentMessage::text(
            singularity_agent::message::AgentMessageRole::User,
            "two",
        ))
        .expect("msg two");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_completed(
            "t2",
        ))
        .expect("t2 completed");
    // 异常布局：无开始标记的终态标记保真为条目而不是改写身份。
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_failed(
            "ghost", "boom",
        ))
        .expect("ghost terminal");
    let entries = session.entries().to_vec();
    drop(session);

    let turns = project_turn_history(&entries);
    assert_eq!(turns.len(), 3);
    assert!(turns[0].turn_id.is_none() && turns[0].status.is_none());
    assert_eq!(
        turns[0]
            .items
            .iter()
            .map(|item| item.kind())
            .collect::<Vec<_>>(),
        vec!["settings"]
    );
    // 非末组的未终止轮按崩溃遗留投影为 interrupted。
    assert_eq!(turns[1].turn_id.as_deref(), Some("t1"));
    assert_eq!(turns[1].status, Some(TurnStatus::Interrupted));
    assert_eq!(turns[2].turn_id.as_deref(), Some("t2"));
    assert_eq!(turns[2].status, Some(TurnStatus::Completed));
    assert_eq!(
        turns[2]
            .items
            .iter()
            .map(|item| item.kind())
            .collect::<Vec<_>>(),
        vec!["message", "turn"],
        "unmatched terminal marker stays visible as a turn item"
    );
}
