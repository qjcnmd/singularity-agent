use std::sync::{Arc, Mutex};

use super::*;
use singularity_agent::session::SessionManager;
use singularity_core::CancellationToken;
use singularity_model::{
    ModelError, ModelErrorKind, ModelTurnRequest, ModelTurnResponse, ModelTurnStatus, Provider,
    ProviderConfigSnapshot, ProviderError, ProviderProtocolContract, ProviderReasoningReplay,
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

fn app_server(sessions_dir: &Path) -> AppServer {
    AppServer::new(
        ProviderConfigSnapshot::capture(
            |name| match name {
                "SINGULARITY_MODEL_PROVIDER" => Some("openai_compatible".to_string()),
                "SINGULARITY_MODEL" => Some("test-model".to_string()),
                "SINGULARITY_BASE_URL" => Some("http://127.0.0.1:1/v1".to_string()),
                "SINGULARITY_API_KEY" => Some("test-key".to_string()),
                _ => None,
            },
            test_runtime_handle(),
        ),
        sessions_dir,
    )
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

fn insert_session(sessions_dir: &Path, session_id: &str, cwd: &Path) -> String {
    SessionManager::create_with_id(cwd, sessions_dir, session_id).expect("create session file");
    session_id.to_string()
}

#[test]
fn jsonl_projection_does_not_repair_incomplete_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let session_id = "9b63cd69-94af-4e42-a53d-dac832be76f8";
    let mut session =
        SessionManager::create_with_id(&workspace, &sessions_dir, session_id).expect("session");
    session
        .append_metadata(singularity_agent::session::SessionMetadata::turn_started(
            "turn-1",
        ))
        .expect("turn metadata");
    let discovered = singularity_runtime::read_thread_summary(&sessions_dir, session_id)
        .expect("project session");
    assert_eq!(
        discovered.status,
        Some(singularity_runtime::ThreadStatus::Active)
    );
    let discovered_session = SessionManager::open_existing(session.path()).expect("reopen");
    assert_eq!(discovered_session.metadata_entries().len(), 1);

    // 重开路径（continue 直接走 turn/start）在 turn 开始前完成一次幂等 repair：
    // 残留无终态 turn 收敛为 interrupted，不阻碍新 turn。
    let mut server = app_server(&sessions_dir).with_test_provider(Arc::new(StaticProvider {
        responses: vec![completed_response("reopen")],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    }));
    initialize(&mut server);
    let reopened = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"reopen"}}]}}}}"#
        ))
        .expect("reopen turn repairs session");
    assert!(
        reopened
            .iter()
            .any(|value| value["method"] == "turn/completed"),
        "reopened turn must complete after repair: {reopened:?}"
    );
    assert_eq!(
        SessionManager::open_existing(session.path())
            .unwrap()
            .metadata_entries()
            .iter()
            .filter(|entry| {
                entry.kind() == singularity_agent::session::SessionMetadataKind::TurnInterrupted
            })
            .count(),
        1,
        "residual turn must be repaired to interrupted exactly once"
    );
    server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":3,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"reopen again"}}]}}}}"#
        ))
        .expect("second reopen turn");
    assert_eq!(
        SessionManager::open_existing(session.path())
            .unwrap()
            .metadata_entries()
            .iter()
            .filter(|entry| {
                entry.kind() == singularity_agent::session::SessionMetadataKind::TurnInterrupted
            })
            .count(),
        1,
        "second reopen must not repeat the repair"
    );
}

#[test]
fn jsonl_projection_isolates_one_corrupt_rollout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let valid_id = "c2e0d5f5-7d50-4ef7-a6f9-0f0c1b3f44ab";
    SessionManager::create_with_id(&workspace, &sessions_dir, valid_id).expect("valid session");
    std::fs::write(sessions_dir.join("broken.jsonl"), b"not-json\n").expect("broken session");

    let sessions = singularity_runtime::list_threads(&sessions_dir).expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, valid_id);
}

#[test]
fn jsonl_projection_recovers_all_fields_including_title_model_usage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
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

    let record = singularity_runtime::read_thread_summary(&sessions_dir, session_id)
        .expect("project session");
    assert_eq!(
        record.title.as_deref(),
        Some("Implement feature X for the system")
    );
    assert_eq!(
        record.model.as_deref(),
        Some("anthropic/claude-3-7-sonnet#high")
    );
    assert_eq!(
        record.status,
        Some(singularity_runtime::ThreadStatus::Completed)
    );
    assert_eq!(
        record.token_usage,
        json!({"input_tokens": 120, "output_tokens": 45})
    );
}

#[test]
fn thread_settings_are_jsonl_first_and_never_store_credentials() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let mut server = app_server(&sessions_dir);
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
                    "model": "test-model",
                },
            })
            .to_string(),
        )
        .expect("settings");
    assert_eq!(settings[0]["result"]["updated"], true);
    let record = singularity_runtime::read_thread_summary(&sessions_dir, &thread_id)
        .expect("project session");
    assert_eq!(
        record.model.as_deref(),
        Some("openai_compatible/test-model")
    );
    assert_eq!(
        singularity_runtime::read_thread_summary(&sessions_dir, &thread_id)
            .unwrap()
            .model
            .as_deref(),
        Some("openai_compatible/test-model")
    );
    let rollout = std::fs::read_to_string(singularity_runtime::thread_session_path(
        &sessions_dir,
        &record.thread_id,
    ))
    .expect("rollout");
    assert!(rollout.contains("thread_settings"));
    assert!(!rollout.contains("apiKey"));
    assert!(!rollout.contains("authorization"));
}

#[test]
fn thread_settings_null_clears_reasoning_while_missing_keeps_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let session_id = "27474472-1f3d-4e7f-9018-cc74461a82b4";
    let mut session = SessionManager::create_with_id(&workspace, &sessions_dir, session_id)
        .expect("create session");
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::thread_settings(
                "openai_compatible",
                "test-model",
                None,
            )
            .expect("settings metadata"),
        )
        .expect("append settings");
    drop(session);

    let mut server = app_server(&sessions_dir);
    initialize(&mut server);

    let kept = server
        .handle_json(
            &json!({
                "jsonrpc": "2.0",
                "method": "thread/settings",
                "id": 20,
                "params": {"threadId": session_id},
            })
            .to_string(),
        )
        .expect("missing reasoning keeps current value");
    assert_eq!(kept[0]["result"]["updated"], false);
    assert_eq!(
        singularity_runtime::read_thread_summary(&sessions_dir, session_id)
            .expect("record")
            .model
            .as_deref(),
        Some("openai_compatible/test-model")
    );

    let cleared = server
        .handle_json(
            &json!({
                "jsonrpc": "2.0",
                "method": "thread/settings",
                "id": 21,
                "params": {"threadId": session_id, "reasoning": null},
            })
            .to_string(),
        )
        .expect("null reasoning clears current value");
    assert_eq!(cleared[0]["result"]["updated"], true);
    assert_eq!(cleared[0]["result"]["reasoning"], serde_json::Value::Null);
    assert_eq!(
        singularity_runtime::read_thread_summary(&sessions_dir, session_id)
            .expect("record")
            .model
            .as_deref(),
        Some("openai_compatible/test-model")
    );
}

#[test]
fn public_history_projection_omits_private_replay_and_internal_tree_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
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
            stop_reason: None,
            provider_reasoning_replay: Some(ProviderReasoningReplay::Responses {
                provider_name: "private-provider".to_string(),
                model_name: "private-model".to_string(),
                reasoning_effort: Some("high".to_string()),
                tool_call_ids: vec!["call-1".to_string()],
                items: vec![
                    json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque-secret"}),
                    json!({"type":"function_call","call_id":"call-1","name":"write","arguments":"{}"}),
                ],
            }),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
        })
        .expect("assistant");
    session
        .append_message(singularity_agent::message::AgentMessage {
            role: singularity_agent::message::AgentMessageRole::ToolResult,
            content: vec![singularity_agent::message::ContentBlock::Text {
                text: "write failed".to_string(),
            }],
            stop_reason: None,
            provider_reasoning_replay: None,
            tool_call_id: Some("call-1".to_string()),
            tool_name: Some("write".to_string()),
            is_error: Some(true),
        })
        .expect("tool result");
    let mut server = app_server(&sessions_dir);
    initialize(&mut server);
    let output = server
        .handle_json(
            &json!({
                "jsonrpc":"2.0",
                "method":"thread/read",
                "id":2,
                "params":{"sessionId":session_id}
            })
            .to_string(),
        )
        .expect("thread/read");
    let wire = serde_json::to_string(&output).expect("wire");
    assert!(wire.contains("visible reasoning"));
    assert!(wire.contains("out.txt"));
    assert!(wire.contains("\"isError\":true"));
    assert!(!wire.contains("opaque-secret"));
    assert!(!wire.contains("private-provider"));
    assert!(!wire.contains("providerReasoningReplay"));
    assert!(!wire.contains("parentId"));
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
    let provider = StaticProvider {
        responses: vec![
            completed_response("first"),
            failed_response(),
            completed_response("third"),
        ],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut server = app_server(&sessions_dir).with_test_provider(Arc::new(provider));
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
    // 尚无 turn：lastTurnStatus 与 JSONL 投影 status 均为 null。
    assert_eq!(
        started[1]["result"]["thread"]["lastTurnStatus"],
        serde_json::Value::Null
    );
    assert_eq!(
        singularity_runtime::read_thread_summary(&sessions_dir, &session_id)
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
        singularity_runtime::read_thread_summary(&sessions_dir, &session_id)
            .expect("record")
            .status,
        Some(singularity_runtime::ThreadStatus::Completed)
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
        singularity_runtime::read_thread_summary(&sessions_dir, &session_id)
            .expect("record")
            .status,
        Some(singularity_runtime::ThreadStatus::Failed)
    );

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

    // completed 会话可以继续执行下一轮。
    let reopened = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":8,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"reopen"}}]}}}}"#
        ))
        .expect("reopen interrupted");
    assert_eq!(
        reopened
            .iter()
            .find(|m| m["method"] == "turn/completed")
            .expect("completed")["params"]["turn"]["status"],
        "completed"
    );
    assert_eq!(
        singularity_runtime::read_thread_summary(&sessions_dir, &session_id)
            .unwrap()
            .status,
        Some(singularity_runtime::ThreadStatus::Completed)
    );
}

#[test]
fn request_methods_as_notifications_are_rejected_without_side_effects() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let session_id = "0b0c1d2e-3f40-4152-8263-9474a5b6c7d8";
    let mut server = app_server(&sessions_dir);
    initialize(&mut server);
    insert_session(&sessions_dir, session_id, &workspace);

    // 每个 Request 方法以 notification（无 id）提交 → 静默忽略且不执行任何副作用。
    for (method, params) in [
        (
            "initialize",
            r#"{"clientInfo":{"name":"t","title":"T","version":"0"}}"#,
        ),
        ("thread/start", r#"{"cwd":"/tmp"}"#),
        ("thread/read", r#"{"sessionId":"<session>"}"#),
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
        ("provider/status", r#"{}"#),
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
    assert!(singularity_runtime::read_thread_summary(&sessions_dir, session_id).is_ok());
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
    let mut server = app_server(&temp.path().join("sessions"));
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
    let session_id = "5b5c6d7e-8f90-4a1b-9c2d-3e4f5a6b7c8d";
    let mut server = app_server(&sessions_dir);
    insert_session(&sessions_dir, session_id, &workspace);

    // initialize 之前发 turn/start：普通管线门禁拒绝，不产生任何 turn 语义。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"turn/start","id":2,"params":{{"threadId":"{session_id}","input":[{{"type":"text","text":"early"}}]}}}}"#
        ))
        .expect("turn/start before initialize");
    assert_eq!(responses[0]["error"]["code"], -32002);
    assert_eq!(
        singularity_runtime::read_thread_summary(&sessions_dir, session_id)
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
fn session_delete_rejects_reserved_turn_and_succeeds_after_reservation_drops() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let session_id = "c14e4e8b-9b4a-4c1d-8f0a-2d5e6f7a8b9c";
    let mut server = app_server(&sessions_dir);
    initialize(&mut server);
    insert_session(&sessions_dir, session_id, &workspace);

    let message: JsonRpcMessage = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "turn/start",
        "id": 10,
        "params": {
            "threadId": session_id,
            "input": [{"type": "text", "text": "reserved"}],
        }
    }))
    .expect("turn/start");
    let claim = match server.claim_turn(message) {
        Ok(TurnClaim::Accepted(claim)) => claim,
        Ok(other) => panic!("claim must accept the turn: {other:?}"),
        Err(error) => panic!("claim failed: {error}"),
    };

    // 请求已获执行权但工作线程尚未启动时，删除仍须被活动窗口拒绝。
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"session/delete","id":2,"params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .expect("delete rejected");
    assert_eq!(responses[0]["error"]["code"], -32005);
    assert!(singularity_runtime::read_thread_summary(&sessions_dir, session_id).is_ok());
    assert!(sessions_dir.join(format!("{session_id}.jsonl")).is_file());

    drop(claim);
    let responses = server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"session/delete","id":3,"params":{{"sessionId":"{session_id}"}}}}"#
        ))
        .expect("delete after turn");
    assert_eq!(responses[0]["result"]["deleted"], true);
    assert!(matches!(
        singularity_runtime::read_thread_summary(&sessions_dir, session_id),
        Err(singularity_runtime::ResumeError::NotFound(_))
    ));
}

#[test]
fn project_instructions_load_from_workspace_root_to_cwd() {
    // 回归固定：root→cwd 逐层加载。
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    let nested = workspace.join("src").join("nested");
    std::fs::create_dir_all(nested.join("..")).expect("nested parent");
    std::fs::create_dir_all(&nested).expect("nested cwd");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    std::fs::write(workspace.join("AGENTS.md"), "ROOT INSTRUCTION").expect("root agents");
    std::fs::write(nested.join("AGENTS.md"), "NESTED INSTRUCTION").expect("nested agents");
    let sessions_dir = temp.path().join("sessions");
    let requests: Arc<Mutex<Vec<ModelTurnRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![completed_response("instructions_turn")],
        seen_requests: Arc::clone(&requests),
    };
    let mut server = app_server(&sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(
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
    let requests: Arc<Mutex<Vec<ModelTurnRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![completed_response("truncated_instructions_turn")],
        seen_requests: Arc::clone(&requests),
    };
    let mut server = app_server(&sessions_dir).with_test_provider(Arc::new(provider));
    initialize(&mut server);
    insert_session(
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
        let mut server = app_server(&sessions_dir).with_test_provider(Arc::new(provider));
        initialize(&mut server);
        insert_session(&sessions_dir, session_id, &workspace);
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
            singularity_runtime::read_thread_summary(&sessions_dir, session_id)
                .expect("session record")
                .status,
            None,
            "{name}: instruction failure must not leave any turn state"
        );
    }
}

// ===== thread/read turn 分页 =====

/// 建一个带 settings 前导组的多轮会话：每轮一条 user 消息，偶数索引轮
/// 额外带一条 toolResult 消息（供条目内容断言区分轮次）。
fn seed_turned_session(sessions_dir: &Path, session_id: &str, turn_ids: &[&str]) -> String {
    let sid = insert_session(sessions_dir, session_id, sessions_dir);
    let path = sessions_dir.join(format!("{sid}.jsonl"));
    let mut session = SessionManager::open_existing(&path).expect("reopen session");
    session
        .append_metadata(
            singularity_agent::session::SessionMetadata::thread_settings(
                "openai_compatible",
                "test-model",
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
                    stop_reason: None,
                    provider_reasoning_replay: None,
                    tool_call_id: Some(format!("call-{index}")),
                    tool_name: Some("bash".to_string()),
                    is_error: None,
                })
                .expect("tool result");
        }
        session
            .append_metadata(singularity_agent::session::SessionMetadata::turn_completed(
                *turn_id,
            ))
            .expect("turn completed");
    }
    sid
}

fn thread_read_response(server: &mut AppServer, id: i64, params: &str) -> serde_json::Value {
    server
        .handle_json(&format!(
            r#"{{"jsonrpc":"2.0","method":"thread/read","id":{id},"params":{params}}}"#
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
fn thread_read_pages_newest_first_and_before_item() {
    let temp = tempfile::tempdir().expect("temp dir");
    let sessions_dir = temp.path().join("sessions");
    let mut server = app_server(&sessions_dir);
    initialize(&mut server);
    let sid = seed_turned_session(
        &sessions_dir,
        "1f0a2b3c-4d5e-4f60-8a92-b3c4d5e6f708",
        &["t1", "t2", "t3"],
    );

    // 默认页：最新 limit 轮，按会话顺序排列；前导组不入默认最新窗口。
    let page1 = thread_read_response(
        &mut server,
        10,
        &format!(r#"{{"sessionId":"{sid}","limit":2}}"#),
    );
    let result1 = &page1["result"];
    assert_eq!(turn_page(result1), vec!["t2".to_string(), "t3".to_string()]);
    assert_eq!(result1["totalTurns"], 3);
    assert_eq!(result1["status"], "completed");
    assert!(
        result1["turns"][0]["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["type"] == "message" && item["text"] == "user-1")
    );

    // beforeItem 锚定上一页最旧轮（t2）内任意公开 item id：返回该轮之前的 limit 轮。
    let anchor = result1["turns"][0]["items"][0]["id"]
        .as_str()
        .expect("anchor id");
    let page2 = thread_read_response(
        &mut server,
        11,
        &format!(r#"{{"sessionId":"{sid}","limit":2,"beforeItem":"{anchor}"}}"#),
    );
    assert_eq!(
        turn_page(&page2["result"]),
        vec!["<prelude>".to_string(), "t1".to_string()],
        "beforeItem must page back to the turns before the anchor turn"
    );
    assert!(
        page2["result"]["turns"][1]["items"]
            .as_array()
            .expect("items")
            .iter()
            .any(|item| item["type"] == "message" && item["text"] == "user-0")
    );

    // 再向上滚一页：剩余前导组所在的窗口。
    let earlier_anchor = page2["result"]["turns"][1]["items"][0]["id"]
        .as_str()
        .expect("earlier anchor id");
    let page3 = thread_read_response(
        &mut server,
        12,
        &format!(r#"{{"sessionId":"{sid}","limit":2,"beforeItem":"{earlier_anchor}"}}"#),
    );
    assert_eq!(
        turn_page(&page3["result"]),
        vec!["<prelude>".to_string()],
        "paging past the first turn returns the leading group only"
    );
}

#[test]
fn thread_read_projects_crash_leftover_turn_as_interrupted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let sessions_dir = temp.path().join("sessions");
    let mut server = app_server(&sessions_dir);
    initialize(&mut server);
    let sid = insert_session(
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
    let response = thread_read_response(&mut server, 60, &format!(r#"{{"sessionId":"{sid}"}}"#));
    let result = &response["result"];
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["turns"][0]["turnId"], "t9");
    assert_eq!(
        result["turns"][0]["status"], "interrupted",
        "trailing running turn must not contradict the overall projection"
    );
    assert!(!result["turns"][0]["items"].as_array().unwrap().is_empty());
}
