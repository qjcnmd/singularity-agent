use super::*;

fn input(text: &str) -> UserInput {
    UserInput {
        text: text.to_string(),
        ..UserInput::default()
    }
}

struct TestDirectory(std::path::PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove isolated test directory");
    }
}

#[tokio::test]
async fn submission_returns_before_saving_and_preserves_input_after_preparation_failure() {
    let directory = TestDirectory(
        std::env::temp_dir().join(format!("singularity-input-acceptance-test-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&directory.0).expect("create isolated test directory");
    let sessions = directory.0.join("sessions");
    let models = Arc::new(ModelConfigManager::open(directory.0.clone()));
    let mcp = Arc::new(singularity_mcp::McpManager::open(directory.0.clone()));
    let runner = Arc::new(TurnRunner::new(sessions.clone(), Arc::clone(&models), Arc::clone(&mcp)));
    let server = AppServer::new(
        runner,
        ThreadCatalog::new(sessions.clone()),
        WorkspaceStore::open(&directory.0).expect("open workspace store"),
        models,
        mcp,
        directory.0.clone(),
    );
    let workspace = server
        .add_workspace(directory.0.to_str().expect("temporary path is UTF-8"))
        .expect("add workspace");
    let mut events = server.subscribe();
    let session = server
        .start_session(&workspace.workspace_id, None, input("first input"))
        .expect("start session");
    let session_id = session.session_id;
    for text in ["first input", "next input"] {
        // 当前线程尚未让出执行权，后台执行器不能先保存消息来满足这个断言。
        if text == "next input" {
            server.submit(&session_id, input(text)).expect("acknowledge queued input");
        }
        let bytes =
            std::fs::read_to_string(sessions.join(format!("{session_id}.jsonl"))).expect("read session");
        assert!(!bytes.contains(text), "submission does not wait for persistence");
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !matches!(
                events.recv().await.expect("operation event").event,
                StreamEvent::SessionSettled { .. }
            ) {}
        })
        .await
        .expect("missing model configuration fails preparation without a model request");
    }
    let snapshot = server.read_session(&session_id, 40, None).expect("read settled session");
    assert_eq!(snapshot.runtime.phase, singularity_protocol::SessionPhase::Idle);
    assert_eq!(snapshot.runtime.terminal.expect("preparation failure").status, TurnStatus::Failed);
    assert_eq!(
        snapshot
            .runtime
            .pending_messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
        ["first input", "next input"]
    );
}

#[tokio::test]
async fn stopped_before_first_input_is_saved_does_not_deliver_it_later() {
    let directory = TestDirectory(
        std::env::temp_dir().join(format!("singularity-input-stop-test-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&directory.0).expect("create isolated test directory");
    // 使用真实执行器；在第一条消息保存前停止，不发送模型请求。
    std::fs::write(
        directory.0.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "default_model": "deepseek/deepseek-chat",
            "providers": {
                "deepseek": {
                    "api_key": "unused-before-cancellation",
                    "base_url": "https://api.deepseek.com/v1",
                    "api_protocol": "chat",
                    "models": {
                        "deepseek-chat": { "max_context_tokens": 128000, "max_output_tokens": 8192 }
                    }
                }
            }
        }))
        .expect("serialize execution configuration"),
    )
    .expect("write execution configuration");
    let sessions = directory.0.join("sessions");
    let runner = Arc::new(TurnRunner::new(
        sessions.clone(),
        Arc::new(ModelConfigManager::open(directory.0.clone())),
        Arc::new(singularity_mcp::McpManager::open(directory.0.clone())),
    ));
    let thread = ThreadCatalog::new(sessions)
        .create_thread(directory.0.to_str().expect("temporary path is UTF-8"), None)
        .expect("create test session");
    let (conversation, _updates) = Conversation::new(runner, thread);
    let reservation =
        conversation.submit(&input("first input")).expect("submit input").expect("reserve turn");
    assert_eq!(conversation.snapshot().pending_messages[0].text, "first input");
    let mut messages = Vec::new();
    let mut started_at = None;
    let mut terminal = None;
    let result = reservation
        .execute(&mut |event| match event {
            TurnEvent::TurnStarted { started_at: timestamp, .. } => {
                started_at = Some(timestamp);
                conversation.submit(&input("steer input")).expect("enqueue steer input");
                conversation.abort().expect("stop before saving queued messages");
            }
            TurnEvent::UserMessage { item, .. } => messages.push(item.item_id),
            TurnEvent::TurnCompleted { manually_stopped, finished_at, .. } => {
                terminal = Some((manually_stopped, finished_at));
            }
            _ => {}
        })
        .await;
    let outcome = match result {
        OperationResult::Turn(Ok(outcome)) => outcome,
        OperationResult::Turn(Err(error)) => panic!("stopped turn must settle normally: {error}"),
        OperationResult::Compaction(_) => panic!("input must start a turn"),
    };
    assert_eq!(outcome.turn_status, TurnStatus::Interrupted);
    assert!(outcome.manually_stopped);
    assert!(messages.is_empty(), "stopping before consumption saves neither first nor steering input");
    conversation.finish_operation();
    assert!(conversation.snapshot().pending_messages.is_empty(), "stopped input cannot run on retry");
    assert!(!conversation.is_occupied());
    let history = ThreadCatalog::new(directory.0.join("sessions"))
        .open_session(&conversation.thread().thread_id)
        .expect("read saved outcome");
    let history = singularity_runtime::ThreadSnapshot::new(history.data_snapshot())
        .page(40, None)
        .expect("project saved outcome");
    let turn = history.turns.last().expect("saved turn");
    let (manually_stopped, finished_at) = terminal.expect("published terminal");
    assert!(manually_stopped);
    assert!(turn.manually_stopped);
    assert_eq!(turn.started_at, started_at);
    assert_eq!(turn.finished_at.as_deref(), Some(finished_at.as_str()));
}

#[tokio::test]
async fn damaged_history_returns_a_read_error_and_keeps_other_sessions_readable() {
    let directory = TestDirectory(
        std::env::temp_dir().join(format!("singularity-history-test-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&directory.0).expect("create isolated test directory");
    let sessions = directory.0.join("sessions");
    let models = Arc::new(ModelConfigManager::open(directory.0.clone()));
    let mcp = Arc::new(singularity_mcp::McpManager::open(directory.0.clone()));
    let runner = Arc::new(TurnRunner::new(sessions.clone(), Arc::clone(&models), Arc::clone(&mcp)));
    let server = AppServer::new(
        runner,
        ThreadCatalog::new(sessions.clone()),
        WorkspaceStore::open(&directory.0).expect("open isolated workspace store"),
        models,
        mcp,
        directory.0.clone(),
    );
    let workspace = server
        .add_workspace(directory.0.to_str().expect("temporary path is UTF-8"))
        .expect("add test workspace");
    let healthy = server.catalog.create_thread(&workspace.root, None).expect("create healthy session");
    let request = serde_json::json!({
        "type": "record",
        "id": "request-record",
        "timestamp": "2026-10-15T00:00:00Z",
        "record": {
            "recordType": "model_request",
            "observation": {
                "requestId": "request-1", "purpose": "generation",
                "provider": "test", "model": "test", "status": "ok", "durationMs": 1,
                "usage": null
            }
        }
    });
    let tool_result = serde_json::json!({
        "type": "message",
        "id": "tool-result",
        "timestamp": "2026-10-15T00:00:00Z",
        "message": {
            "role": "toolResult", "content": [], "toolCallId": "missing-tool", "isError": false
        }
    });
    let mut missing_total = request;
    missing_total["record"]["observation"]["usage"] = serde_json::json!({
        "inputTokens": 1, "outputTokens": 1, "cachedInputTokens": null
    });
    for (entry, expected) in [
        (tool_result, "missing-tool"),
        (missing_total, "missing field `totalTokens`"),
    ] {
        let session = server.catalog.create_thread(&workspace.root, None).expect("create test session");
        let session_id = session.session_id().to_owned();
        let path = sessions.join(format!("{session_id}.jsonl"));
        let mut bytes = std::fs::read(&path).expect("read test session");
        serde_json::to_writer(&mut bytes, &entry).expect("serialize damaged entry");
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("write damaged test session");

        server.lock_sessions().remove(&session_id);
        let error = server.read_session(&session_id, 40, None).expect_err("reject damaged history");
        assert_eq!(error.code, RpcErrorCode::Internal);
        assert!(error.message.contains(path.to_string_lossy().as_ref()), "{}", error.message);
        assert!(error.message.contains(expected), "{}", error.message);
        assert_eq!(std::fs::read(&path).expect("reread damaged session"), bytes);
        server
            .read_session(healthy.session_id(), 40, None)
            .expect("other sessions remain readable");
    }
}
