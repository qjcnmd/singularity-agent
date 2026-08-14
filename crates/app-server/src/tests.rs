use std::sync::{Arc, Mutex};

use singularity_agent::{agent::AgentOutcome, session::SessionManager};
use singularity_model::{
    ModelError, ModelErrorKind, ModelRole, ModelToolCall, ModelToolParseStatus, ModelTurnRequest,
    ModelTurnResponse, ModelTurnStatus, ModelUsage, Provider, ProviderError,
    ProviderProtocolContract,
};

use singularity_protocol::ConversationRole;

use super::*;

fn app_server(store: SessionStore) -> AppServer {
    // 隔离 trust 存储：挂载独立临时 trust home，避免读取/写入真实用户 trust.json。
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
}

#[test]
fn initialized_request_is_rejected_as_an_invalid_envelope() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    server
            .handle_json(
                r#"{"jsonrpc":"2.0","method":"initialize","id":1,"params":{"clientInfo":{"name":"test","title":"Test","version":"0.1.0"}}}"#,
            )
            .expect("initialize");

    let response = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"initialized","id":2,"params":{}}"#)
        .expect("initialized request");

    assert_eq!(response.len(), 1);
    assert_eq!(response[0]["jsonrpc"], "2.0");
    assert_eq!(response[0]["id"], 2);
    assert_eq!(response[0]["error"]["code"], -32600);
    assert_eq!(response[0]["error"]["message"], "Invalid Request");

    let still_uninitialized = server
        .handle_json(r#"{"jsonrpc":"2.0","method":"server/capabilities","id":3,"params":{}}"#)
        .expect("server remains unacknowledged");
    assert_eq!(
        still_uninitialized[0]["error"]["message"],
        "Not initialized"
    );
}

#[test]
fn duplicate_activation_preserves_the_original_and_global_stop_cancels_future_turns() {
    let server = app_server(SessionStore::open(":memory:").expect("store"));
    let (original, _guard) = server.activate_turn("turn_1").expect("activate turn");

    let duplicate = server.activate_turn("turn_1");

    assert!(matches!(duplicate, Err(AppServerError::Workspace(_))));
    assert!(!original.is_cancelled());
    server
        .request_execution_stop()
        .expect("request execution stop");
    assert!(original.is_cancelled());
    let (late, _late_guard) = server.activate_turn("turn_2").expect("late activation");
    assert!(late.is_cancelled());
}

#[test]
fn cancelled_run_commits_as_interrupted_and_safe_states_are_preserved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("running turn");
    let server = app_server(store);

    let cancelled_token = CancellationToken::new();
    cancelled_token.cancel();
    server
        .commit_turn_run_status(
            turn.clone(),
            &RunStatus::failed("late user result"),
            None,
            &cancelled_token,
        )
        .expect("user cancellation commit");
    let interrupted = server
        .store
        .get_turn(&turn.turn_id)
        .expect("interrupted turn");
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    assert_eq!(
        interrupted.agent_loop_status,
        AgentStatus::Cancelled.as_str()
    );

    // 已终态 turn 不被失败终态化覆盖。
    let mut emitted = Vec::new();
    let mut emit = |message| emitted.push(message);
    let result =
        server.finish_turn_failure(&mut emit, &interrupted, None, TurnFailureStage::AgentLoop);
    assert!(matches!(
        result,
        Err(AppServerError::TurnExecution {
            stage: TurnFailureStage::AgentLoop,
            ..
        })
    ));
    assert_eq!(
        server
            .store
            .get_turn(&turn.turn_id)
            .expect("safe turn")
            .status,
        TurnStatus::Interrupted
    );
}

#[test]
fn late_turn_failure_does_not_overwrite_a_blocked_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Blocked,
            AgentStatus::Blocked.as_str(),
        )
        .expect("blocked turn");
    let server = app_server(store);

    assert!(
        server
            .commit_turn_run_status(
                turn.clone(),
                &RunStatus::failed("stale run failure"),
                None,
                &CancellationToken::new(),
            )
            .is_err()
    );
    let persisted = server
        .store
        .get_turn(&turn.turn_id)
        .expect("persisted turn");
    assert_eq!(persisted.status, TurnStatus::Blocked);
    assert_eq!(persisted.agent_loop_status, AgentStatus::Blocked.as_str());
}

#[test]
fn running_turn_failure_stages_terminalize_as_failed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let stages = [
        TurnFailureStage::AgentLoop,
        TurnFailureStage::TerminalOutcome,
    ];
    let mut turns = Vec::new();
    for stage in stages {
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        turns.push((turn, stage));
    }
    let server = app_server(store);

    for (turn, stage) in turns {
        if stage == TurnFailureStage::TerminalOutcome {
            let invalid_commit =
                RunStatus::failed("invalid completion").with_status(AgentStatus::Completed);
            assert!(
                server
                    .commit_turn_run_status(
                        turn.clone(),
                        &invalid_commit,
                        None,
                        &CancellationToken::new(),
                    )
                    .is_err()
            );
        }
        let mut emitted = Vec::new();
        let mut emit = |message| emitted.push(message);
        let result = server.finish_turn_failure(&mut emit, &turn, None, stage);
        assert!(matches!(
            result,
            Err(AppServerError::TurnExecution { stage: actual, .. }) if actual == stage
        ));
        let persisted = server.store.get_turn(&turn.turn_id).expect("failed turn");
        assert_eq!(persisted.status, TurnStatus::Failed);
        assert_eq!(persisted.agent_loop_status, AgentStatus::Failed.as_str());
    }
}

#[test]
fn terminalization_preserves_interrupted_and_blocked_turns() {
    let cases = [
        (TurnStatus::Interrupted, AgentStatus::Cancelled),
        (TurnStatus::Blocked, AgentStatus::Blocked),
    ];
    for (expected_status, expected_agent_status) in cases {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
        let thread = store.create_thread(None, None).expect("thread");
        let turn = store
            .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
            .expect("turn");
        store
            .update_turn_state(
                &turn.turn_id,
                expected_status.clone(),
                expected_agent_status.as_str(),
            )
            .expect("safe state");
        let server = app_server(store);
        let mut emitted = Vec::new();
        let mut emit = |message| emitted.push(message);

        let result =
            server.finish_turn_failure(&mut emit, &turn, None, TurnFailureStage::AgentLoop);

        assert!(matches!(
            result,
            Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::AgentLoop,
                ..
            })
        ));
        let persisted = server.store.get_turn(&turn.turn_id).expect("safe turn");
        assert_eq!(persisted.status, expected_status);
        assert_eq!(persisted.agent_loop_status, expected_agent_status.as_str());
    }
}

#[test]
fn terminalization_failure_keeps_stage_and_redacts_cleanup_cause() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(dir.path().join("sessions.sqlite3")).expect("store");
    let thread = store.create_thread(None, None).expect("thread");
    let turn = store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let mut missing_turn = turn;
    missing_turn.turn_id = "missing-turn-with-secret-path".to_string();
    let server = app_server(store);
    let mut emitted = Vec::new();
    let mut emit = |message| emitted.push(message);

    let error = server
        .finish_turn_failure(
            &mut emit,
            &missing_turn,
            None,
            TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: None,
            },
        )
        .expect_err("terminalization must report its cleanup failure");
    assert!(matches!(
        error,
        AppServerError::TurnTerminalization {
            stage: TurnFailureStage::TerminalOutcome,
            cause: TurnFailureCause::Store,
            failure: TurnTerminalizationFailure::Store,
            ..
        }
    ));
    assert!(!error.to_string().contains("missing-turn-with-secret-path"));
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
        _cancellation: &singularity_core::CancellationToken,
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

fn failed_model_response(error: ModelError) -> ModelTurnResponse {
    let mut response = ModelTurnResponse::completed("request_1", "response_1", "unused");
    response.status = ModelTurnStatus::Failed;
    response.assistant_message = None;
    response.error = Some(error);
    response
}

#[test]
fn app_server_preserves_safe_provider_failure_via_turn_start() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let provider_sentinel = "provider-body-sentinel";
    let mut provider_error =
        ModelError::new(ModelErrorKind::AuthError, provider_sentinel.to_string());
    provider_error.validation_errors = vec![provider_sentinel.to_string()];
    let provider = StaticProvider {
        responses: vec![failed_model_response(provider_error)],
        seen_requests: Arc::new(Mutex::new(Vec::new())),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    let error = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect_err("provider failure must surface");
    assert!(matches!(
        error,
        AppServerError::TurnExecution {
            stage: TurnFailureStage::AgentLoop,
            cause: TurnFailureCause::Internal,
            ..
        }
    ));
    assert!(!error.to_string().contains(provider_sentinel));
    // turn 以 Failed 终态提交，provider 错误文本不落入持久化 turn 状态。
    assert!(error.to_string().contains("agent_loop"));
    // 真实失败文本经 original 字段携带到 RPC 边界（transport 层据此透出），
    // 但持久化分类（stage/cause）仍保持粗粒度。
    match &error {
        AppServerError::TurnExecution { original, .. } => {
            let original = original.as_deref().expect("original provider text carried");
            assert!(original.contains(provider_sentinel));
        }
        other => panic!("expected TurnExecution, got {other}"),
    }
}

#[test]
fn agent_loop_system_prompt_loads_bounded_agents_md_from_thread_cwd() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ancestor = temp
        .path()
        .join("SINGULARITY_API_KEY=must-not-leak")
        .join("ancestor");
    let workspace = ancestor.join("workspace");
    std::fs::create_dir_all(ancestor.join(".git")).expect("ancestor git marker");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(ancestor.join("AGENTS.md"), "ancestor instructions").expect("ancestor agents");
    std::fs::write(workspace.join("AGENTS.md"), "workspace instructions")
        .expect("workspace agents");
    std::fs::write(workspace.join("AGENTS.override.md"), "workspace override")
        .expect("workspace override");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    // 信任该 workspace（有 AGENTS.md 且无记录时默认 ask，非交互测试默认不加载）。
    let trust_home = tempfile::tempdir().expect("trust home");
    let mut decisions = singularity_core::TrustDecisions::load(trust_home.path());
    decisions.set(&workspace, true).expect("trust workspace");
    let mut server = app_server(store)
        .with_trust_home(trust_home.path())
        .with_test_provider(Arc::new(provider));

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    let developer = &requests[0].messages[0].content;
    assert!(
        developer.contains("workspace override"),
        "system prompt should contain workspace override only: {developer}"
    );
    assert!(!developer.contains("ancestor instructions"));
    assert_eq!(requests[0].messages[1].role, ModelRole::User);
    assert_eq!(requests[0].messages[1].content, "user goal");
    let hidden_workspace_marker = workspace.to_string_lossy();
    assert!(!requests[0].tools.iter().any(|tool| {
        serde_json::to_string(tool)
            .expect("serialize tool")
            .contains(hidden_workspace_marker.as_ref())
    }));
}

#[test]
fn agent_loop_replays_session_file_history_across_turns() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "project instructions")
        .expect("agents instructions");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![
            ModelTurnResponse::completed(
                "model_request_turn_1_0",
                "response_1",
                "previous assistant",
            ),
            ModelTurnResponse::completed("model_request_turn_2_0", "response_2", "done"),
        ],
        seen_requests: Arc::clone(&seen_requests),
    };
    // 信任该 workspace：断言依赖 developer 指令消息存在。
    let trust_home = tempfile::tempdir().expect("trust home");
    let mut decisions = singularity_core::TrustDecisions::load(trust_home.path());
    decisions.set(&workspace, true).expect("trust workspace");
    let mut server = app_server(store)
        .with_trust_home(trust_home.path())
        .with_test_provider(Arc::new(provider));

    for (id, input) in [(1, "previous user"), (2, "current user")] {
        let responses = server
            .turn_start(
                JsonRpcMessage::request(
                    Method::TurnStart,
                    id,
                    json!({
                        "threadId": thread.thread_id,
                        "input": [{"type": "text", "text": input}],
                    }),
                )
                .expect("request"),
            )
            .expect("turn start");
        let result: TurnStartResult = serde_json::from_value(
            responses
                .iter()
                .find(|message| message["id"] == id)
                .expect("response")["result"]
                .clone(),
        )
        .expect("turn result");
        assert_eq!(result.turn.status, TurnStatus::Completed);
    }

    // 第二轮请求上下文：上一轮 user/assistant 消息经 session 文件重放。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    let roles: Vec<ModelRole> = requests[1]
        .messages
        .iter()
        .map(|message| message.role.clone())
        .collect();
    assert_eq!(
        roles,
        vec![
            ModelRole::Developer,
            ModelRole::User,
            ModelRole::Assistant,
            ModelRole::User,
        ]
    );
    assert_eq!(
        requests[1]
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec![
            "project instructions",
            "previous user",
            "previous assistant",
            "current user",
        ]
    );
    // 会话文件生成且消息完整（跨轮历史的唯一事实源）。
    let session_file = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join(format!("{}.jsonl", thread.thread_id));
    assert!(session_file.exists());
    let session = SessionManager::open(&session_file).expect("session");
    let entries = session.build_context_entries().expect("entries");
    let texts: Vec<String> = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            singularity_agent::session::SessionEntryType::Message(message) => {
                Some(message.content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![
            "previous user",
            "previous assistant",
            "current user",
            "done"
        ]
    );
}

#[test]
fn partial_realtime_item_fails_without_persisting_or_completing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let server = app_server(store);

    for (label, cancelled) in [
        ("raw provider stream failure sentinel", false),
        ("raw terminal mismatch sentinel", false),
        ("raw cancellation sentinel", true),
    ] {
        let thread = server
            .store
            .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
            .expect("thread");
        let (turn, _) = server
            .store
            .create_turn_with_input(
                &thread.thread_id,
                AgentStatus::Running.as_str(),
                json!([{"type": "text", "text": "run"}]),
            )
            .expect("turn");
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        let mut events = server
            .project_assistant_delta(&mut assistant_events, "partial")
            .expect("project partial");
        let mut status = RunStatus::failed(label);
        if cancelled {
            status.status = AgentStatus::Cancelled;
        }

        let committed = server
            .commit_turn_run_status(
                turn,
                &status,
                Some(&assistant_events.item_id),
                &CancellationToken::new(),
            )
            .expect("commit safe terminal status");
        events.extend(
            server
                .committed_turn_events(&committed, Some(&assistant_events))
                .expect("terminal events"),
        );

        let methods = events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            vec![
                "item/started",
                "item/agentMessage/delta",
                "item/failed",
                "turn/completed",
            ]
        );
        assert_eq!(events[2]["params"]["error"], SAFE_ASSISTANT_ITEM_FAILURE);
        assert!(
            !serde_json::to_string(&events)
                .expect("events json")
                .contains(label)
        );
        assert!(committed.assistant_item.is_none());
        assert!(
            server
                .store
                .read_thread_history(&thread.thread_id, None, 10)
                .expect("history")
                .messages
                .iter()
                .all(|message| message.role != ConversationRole::Assistant)
        );
    }
}

#[test]
fn terminal_store_failure_fails_visible_realtime_item_without_completion() {
    let temp = tempfile::tempdir().expect("temp dir");
    let db_path = temp.path().join("sessions.sqlite3");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    let store = SessionStore::open(&db_path).expect("store");
    let assistant_item_id = SessionStore::allocate_assistant_item_id();
    let collision_thread = store.create_thread(None, None).expect("collision thread");
    let collision_turn = store
        .create_turn(&collision_thread.thread_id, AgentStatus::Running.as_str())
        .expect("collision turn");
    store
        .commit_turn_outcome(
            &collision_turn.turn_id,
            CommitTurnOutcomeParams {
                status: TurnStatus::Completed,
                agent_loop_status: AgentStatus::Completed.as_str(),
                assistant_item_id: Some(&assistant_item_id),
                assistant_delta: Some("existing assistant"),
            },
        )
        .expect("existing assistant");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            AgentStatus::Running.as_str(),
            json!([{"type": "text", "text": "complete"}]),
        )
        .expect("turn");
    let server = app_server(store);
    let status = outcome_to_run_status(AgentOutcome {
        final_text: "complete".to_string(),
        turns: 1,
        usage: ModelUsage::default(),
        compacted: false,
        aborted: false,
    });
    let mut assistant_events = AssistantItemEventState::new(assistant_item_id);
    let mut events = server
        .project_assistant_delta(&mut assistant_events, "partial")
        .expect("partial delta");

    assert!(
        server
            .commit_turn_run_status(
                turn.clone(),
                &status,
                Some(&assistant_events.item_id),
                &CancellationToken::new(),
            )
            .is_err()
    );
    let result = server.finish_turn_failure(
        &mut |event| events.push(event),
        &turn,
        Some(&assistant_events),
        TurnFailure {
            stage: TurnFailureStage::TerminalOutcome,
            cause: TurnFailureCause::Store,
            original: None,
        },
    );

    assert!(matches!(result, Err(AppServerError::TurnExecution { .. })));
    assert_eq!(
        events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>(),
        vec![
            "item/started",
            "item/agentMessage/delta",
            "item/failed",
            "turn/completed",
        ]
    );
    assert_eq!(
        server.store.get_turn(&turn.turn_id).expect("turn").status,
        TurnStatus::Failed
    );
    assert!(
        server
            .store
            .read_thread_history(&thread.thread_id, None, 10)
            .expect("history")
            .messages
            .iter()
            .all(|message| message.role != ConversationRole::Assistant)
    );
}

#[test]
fn realtime_item_events_are_always_emitted_and_deduplicated_at_commit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let server = app_server(store);

    let mut assistant_events =
        AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
    let mut events = server
        .project_assistant_delta(&mut assistant_events, "first")
        .expect("first delta");
    events.extend(
        server
            .project_assistant_delta(&mut assistant_events, "second")
            .expect("second delta"),
    );

    assert_eq!(
        events
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>(),
        vec![
            "item/started",
            "item/agentMessage/delta",
            "item/agentMessage/delta",
        ]
    );
    assert!(assistant_events.started_generated);
    assert!(assistant_events.delta_generated);

    // 终态提交不再重复 realtime 已发射的 item/started 与 delta，只补 item/completed。
    let committed_item = singularity_protocol::Item {
        item_id: assistant_events.item_id.as_str().to_string(),
        turn_id: "turn_1".to_string(),
        kind: singularity_protocol::ItemKind::AgentMessage,
        payload: serde_json::json!({"delta": "first"}),
        status: singularity_protocol::ItemStatus::Completed,
    };
    let terminal = server
        .agent_terminal_item_events(Some(&committed_item), Some(&assistant_events))
        .expect("terminal item events");
    assert_eq!(
        terminal
            .iter()
            .map(|event| event["method"].as_str().expect("event method"))
            .collect::<Vec<_>>(),
        vec!["item/completed"]
    );
}

#[test]
fn agent_capability_is_always_available_without_a_sandbox_gate() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    let responses = server
        .agent_capability(
            JsonRpcMessage::request(Method::AgentCapability, 1, json!({})).expect("request"),
        )
        .expect("agent capability");
    let result: AgentCapabilityResult =
        serde_json::from_value(responses[0]["result"].clone()).expect("capability result");
    // 无门禁语义：恒 available，协议形状保持（CLI doctor 依赖 status==completed）。
    assert!(result.agent_loop.available);
    assert!(result.agent_loop.blockers.is_empty());
    assert_eq!(result.agent_loop.status, "completed");
}

/// 构造带工具调用序列的 fake provider（write 工具 → 文本收尾）。
fn tool_using_static_provider(seen_requests: Arc<Mutex<Vec<ModelTurnRequest>>>) -> StaticProvider {
    let mut tool_response =
        ModelTurnResponse::completed("model_request_turn_1_0", "response_1", "");
    tool_response.tool_calls.push(ModelToolCall {
        tool_call_id: "call_1".to_string(),
        tool_name: "write".to_string(),
        arguments: json!({"path": "hello.txt", "content": "hello"}),
        raw_arguments: json!({"path": "hello.txt", "content": "hello"}).to_string(),
        parse_status: ModelToolParseStatus::Valid,
        validation_errors: Vec::new(),
    });
    StaticProvider {
        responses: vec![
            tool_response,
            ModelTurnResponse::completed("model_request_turn_1_1", "response_2", "done"),
        ],
        seen_requests,
    }
}

#[test]
fn turn_start_runs_new_core_with_tools_and_session_file() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let mut server = app_server(store).with_test_provider(Arc::new(tool_using_static_provider(
        Arc::clone(&seen_requests),
    )));

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "write hello.txt"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    assert_eq!(
        result.turn.agent_loop_status,
        AgentStatus::Completed.as_str()
    );
    // 事件流：item/started + item/agentMessage/delta + item/completed + turn/completed。
    let methods: Vec<&str> = responses
        .iter()
        .filter_map(|message| message["method"].as_str())
        .collect();
    assert_eq!(
        methods,
        vec![
            "turn/started",
            "item/started",
            "item/agentMessage/delta",
            "item/completed",
            "turn/completed",
        ]
    );
    // 工具真实执行：write 写入 workspace。
    assert_eq!(
        std::fs::read_to_string(workspace.join("hello.txt")).expect("read hello.txt"),
        "hello"
    );
    // 两轮 provider 调用，第二轮上下文重放 assistant tool call + tool result。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(second.messages[1].role, ModelRole::Assistant);
    assert_eq!(second.messages[1].tool_calls.len(), 1);
    assert_eq!(second.messages[2].role, ModelRole::Tool);
    // session 文件生成：thread_id.jsonl，消息完整。
    let session_file = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join(format!("{}.jsonl", thread.thread_id));
    assert!(session_file.exists());
    let session = SessionManager::open(&session_file).expect("session");
    let entries = session.build_context_entries().expect("entries");
    let messages: Vec<&singularity_agent::message::AgentMessage> = entries
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            singularity_agent::session::SessionEntryType::Message(message) => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[0].role,
        singularity_agent::message::AgentMessageRole::User
    );
    assert_eq!(messages[0].content, "write hello.txt");
    assert_eq!(
        messages[1].tool_name.as_deref(),
        Some("write"),
        "assistant tool call message persisted"
    );
    assert!(messages[2].content.contains("Successfully wrote"));
    assert_eq!(messages[3].content, "done");
}

#[test]
fn turn_input_during_run_pushes_the_registered_steer_handle() {
    let store = SessionStore::open(":memory:").expect("store");
    let mut server = app_server(store);
    let thread = server.store.create_thread(None, None).expect("thread");
    let turn = server
        .store
        .create_turn(&thread.thread_id, AgentStatus::Running.as_str())
        .expect("turn");
    let handle = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    server
        .steer_handles
        .lock()
        .expect("steer handles")
        .insert(turn.turn_id.clone(), handle.clone());

    let responses = server
        .turn_input(
            JsonRpcMessage::request(
                Method::TurnInput,
                1,
                json!({
                    "turnId": turn.turn_id,
                    "inputId": "input-live",
                    "delivery": "steer",
                    "input": [{"type": "text", "text": "live steer"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn input");
    assert_eq!(responses.len(), 1);
    // 运行中注入：共享 steer 队列收到消息（run 下一轮 drain）。
    let queued = handle.lock().expect("handle");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0], "live steer");
}

#[test]
fn turn_resume_reopens_session_and_runs_new_core() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let (turn, _) = store
        .create_turn_with_input(
            &thread.thread_id,
            AgentStatus::Paused.as_str(),
            json!([{"type": "text", "text": "resume me"}]),
        )
        .expect("turn");
    store
        .update_turn_state(
            &turn.turn_id,
            TurnStatus::Paused,
            AgentStatus::Paused.as_str(),
        )
        .expect("paused turn");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "resumed answer",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    let responses = server
        .turn_resume(
            JsonRpcMessage::request(Method::TurnResume, 1, json!({ "turnId": turn.turn_id }))
                .expect("request"),
        )
        .expect("turn resume");
    let result: TurnResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    // 恢复输入来自 turn 行；会话文件被创建并写入。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .messages
            .iter()
            .any(|message| message.content == "resume me")
    );
    let session_file = workspace
        .join(".singularity")
        .join("agent-sessions")
        .join(format!("{}.jsonl", thread.thread_id));
    assert!(session_file.exists());
    let session = SessionManager::open(&session_file).expect("session");
    let texts: Vec<String> = session
        .build_context_entries()
        .expect("entries")
        .iter()
        .filter_map(|entry| match &entry.entry_type {
            singularity_agent::session::SessionEntryType::Message(message) => {
                Some(message.content.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["resume me", "resumed answer"]);
}

#[test]
fn turn_start_requires_trust_decision_before_creating_the_turn() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "project instructions").expect("agents");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    // ask 未决 + 有交互 UI：turn/start 返回 -32010 且不创建 turn。
    let trust_home = tempfile::tempdir().expect("trust home");
    let mut server = app_server(store)
        .with_trust_home(trust_home.path())
        .with_interactive_ui(true)
        .with_test_provider(Arc::new(provider));

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    assert_eq!(responses.len(), 1, "no events before the trust decision");
    assert_eq!(responses[0]["error"]["code"], -32010);
    assert_eq!(
        responses[0]["error"]["data"]["cwd"],
        workspace.to_string_lossy().as_ref()
    );
    // 未创建 turn：无会话文件、无模型请求。
    assert!(
        !workspace
            .join(".singularity")
            .join("agent-sessions")
            .exists(),
        "turn must not be created when trust is pending"
    );
    assert!(seen_requests.lock().expect("seen requests").is_empty());

    // 设置决策后重试：turn 完成且加载项目指令。
    server
        .project_trust(
            JsonRpcMessage::request(
                Method::ProjectTrust,
                2,
                json!({ "path": workspace.to_string_lossy(), "decision": true }),
            )
            .expect("request"),
        )
        .expect("project trust set");
    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                3,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start retry");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 3)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, ModelRole::Developer);
    assert!(
        requests[0].messages[0]
            .content
            .contains("project instructions")
    );
}

#[test]
fn turn_start_skips_instructions_when_project_is_never_trusted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "project instructions").expect("agents");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let trust_home = tempfile::tempdir().expect("trust home");
    let mut decisions = singularity_core::TrustDecisions::load(trust_home.path());
    decisions.set(&workspace, false).expect("set never trusted");
    let mut server = app_server(store)
        .with_trust_home(trust_home.path())
        .with_interactive_ui(true)
        .with_test_provider(Arc::new(provider));

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    // 不信任不拒绝运行：仅不加载指令（无 developer 消息，首条为用户消息）。
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].messages[0].role, ModelRole::User);
}

#[test]
fn turn_start_ask_pending_without_interactive_ui_runs_without_instructions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(workspace.join("AGENTS.md"), "project instructions").expect("agents");
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let thread = store
        .create_thread(Some("gpt-test"), Some(&workspace.to_string_lossy()))
        .expect("thread");
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    let provider = StaticProvider {
        responses: vec![ModelTurnResponse::completed(
            "model_request_turn_1_0",
            "response_1",
            "done",
        )],
        seen_requests: Arc::clone(&seen_requests),
    };
    let mut server = app_server(store).with_test_provider(Arc::new(provider));

    let responses = server
        .turn_start(
            JsonRpcMessage::request(
                Method::TurnStart,
                1,
                json!({
                    "threadId": thread.thread_id,
                    "input": [{"type": "text", "text": "user goal"}],
                }),
            )
            .expect("request"),
        )
        .expect("turn start");
    let result: TurnStartResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("turn result");
    assert_eq!(result.turn.status, TurnStatus::Completed);
    // ask 未决 + 无交互 UI → 按不信任处理：不返回 -32010，也不加载指令。
    assert!(
        responses
            .iter()
            .all(|message| message.get("error").is_none())
    );
    let requests = seen_requests.lock().expect("seen requests");
    assert_eq!(requests[0].messages[0].role, ModelRole::User);
}

#[test]
fn project_trust_handler_queries_sets_and_resets_decisions() {
    let temp = tempfile::tempdir().expect("temp dir");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    // handler 返回 canonical 路径（Windows 上为 \\?\ 前缀形式）。
    let expected_path = std::fs::canonicalize(&workspace)
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
    let store = SessionStore::open(temp.path().join("sessions.sqlite3")).expect("store");
    let trust_home = tempfile::tempdir().expect("trust home");
    let mut server = app_server(store).with_trust_home(trust_home.path());

    // 查询：无记录 → decision 缺失。
    let responses = server
        .project_trust(
            JsonRpcMessage::request(Method::ProjectTrust, 1, json!({ "path": expected_path }))
                .expect("request"),
        )
        .expect("project trust query");
    let result: ProjectTrustResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 1)
            .expect("response")["result"]
            .clone(),
    )
    .expect("trust result");
    assert_eq!(result.path, expected_path);
    assert_eq!(result.decision, None);

    // 设置 true → 查询命中；文件落盘。
    server
        .project_trust(
            JsonRpcMessage::request(
                Method::ProjectTrust,
                2,
                json!({ "path": expected_path, "decision": true }),
            )
            .expect("request"),
        )
        .expect("project trust set");
    let responses = server
        .project_trust(
            JsonRpcMessage::request(Method::ProjectTrust, 3, json!({ "path": expected_path }))
                .expect("request"),
        )
        .expect("project trust query");
    let result: ProjectTrustResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 3)
            .expect("response")["result"]
            .clone(),
    )
    .expect("trust result");
    assert_eq!(result.decision, Some(true));
    assert_eq!(
        singularity_core::TrustDecisions::load(trust_home.path()).get(&workspace),
        Some(true)
    );

    // 重置为 ask（decision: null）→ 记录清除。
    server
        .project_trust(
            JsonRpcMessage::request(
                Method::ProjectTrust,
                4,
                json!({ "path": expected_path, "decision": null }),
            )
            .expect("request"),
        )
        .expect("project trust reset");
    let responses = server
        .project_trust(
            JsonRpcMessage::request(Method::ProjectTrust, 5, json!({ "path": expected_path }))
                .expect("request"),
        )
        .expect("project trust query");
    let result: ProjectTrustResult = serde_json::from_value(
        responses
            .iter()
            .find(|message| message["id"] == 5)
            .expect("response")["result"]
            .clone(),
    )
    .expect("trust result");
    assert_eq!(result.decision, None);
    assert_eq!(
        singularity_core::TrustDecisions::load(trust_home.path()).get(&workspace),
        None
    );
}
