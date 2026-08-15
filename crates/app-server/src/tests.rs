use std::sync::{Arc, Mutex};

use super::*;
use singularity_agent::session::SessionManager;
use singularity_model::{
    ModelToolCall, ModelToolParseStatus, ModelTurnRequest, ModelTurnResponse, Provider,
    ProviderError, ProviderProtocolContract,
};

fn app_server(store: SessionStore, sessions_dir: &Path) -> AppServer {
    let trust_home = Box::leak(Box::new(tempfile::tempdir().expect("trust home")));
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
    .with_trust_home(trust_home.path())
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
            status: SessionStatus::Active,
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
    assert_eq!(record.status, SessionStatus::Completed);
    assert_eq!(record.title.as_deref(), Some("write hello.txt"));
    let session = SessionManager::open_existing(&rollout).expect("session");
    assert_eq!(session.session_id(), session_id);
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
