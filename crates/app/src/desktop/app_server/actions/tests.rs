use super::*;
use std::future::Future;
use std::task::{Context, Poll, Waker};

struct TestDirectory(std::path::PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove isolated test directory");
    }
}

#[tokio::test]
async fn failed_history_read_releases_waiting_stop() {
    for compaction in [false, true] {
        let directory = TestDirectory(
            std::env::temp_dir().join(format!("singularity-reservation-test-{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir_all(&directory.0).expect("create isolated test directory");
        let sessions = directory.0.join("sessions");
        let models = Arc::new(ModelConfigManager::open(directory.0.clone()));
        let mcp = Arc::new(singularity_mcp::McpManager::open(directory.0.clone()));
        let runner = Arc::new(TurnRunner::new(sessions.clone(), Arc::clone(&models), Arc::clone(&mcp)));
        let catalog = ThreadCatalog::new(sessions.clone());
        let thread = catalog
            .create_thread(directory.0.to_str().expect("temporary path is UTF-8"), None)
            .expect("create test session");
        let session_id = thread.thread_id.clone();
        let server = AppServer::new(
            runner,
            tokio::runtime::Handle::current(),
            catalog,
            WorkspaceStore::open(&directory.0).expect("open isolated workspace store"),
            models,
            mcp,
            directory.0.clone(),
        );
        let slot = server.insert_slot(thread);
        let reservation = if compaction {
            slot.conversation().reserve_compaction().expect("reserve compaction")
        } else {
            slot.conversation().reserve_start("test input").expect("reserve turn")
        };

        // 归档已经开始等待，随后启动路径才遇到日志读取失败。
        let slots = [Arc::clone(&slot)];
        let mut stopping = std::pin::pin!(server.stop_sessions(&slots));
        let mut context = Context::from_waker(Waker::noop());
        assert!(matches!(stopping.as_mut().poll(&mut context), Poll::Pending));
        let session_file = sessions.join(format!("{session_id}.jsonl"));
        std::fs::write(&session_file, b"invalid session\n").expect("make isolated history unreadable");
        assert!(server.begin_operation(&session_id, &slot, reservation, SlotState::begin_turn).is_err());
        assert!(!slot.conversation().is_occupied());

        tokio::time::timeout(std::time::Duration::from_millis(100), stopping)
            .await
            .expect("stopping must wake after the reservation is released");
    }
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
        tokio::runtime::Handle::current(),
        ThreadCatalog::new(sessions.clone()),
        WorkspaceStore::open(&directory.0).expect("open isolated workspace store"),
        models,
        mcp,
        directory.0.clone(),
    );
    let workspace = server
        .add_workspace(directory.0.to_str().expect("temporary path is UTF-8"))
        .expect("add test workspace");
    let healthy = server.create_session(&workspace.workspace_id).expect("create healthy session");
    let request = serde_json::json!({
        "type": "record",
        "id": "request-record",
        "timestamp": "2026-10-15T00:00:00Z",
        "record": {
            "recordType": "model_request",
            "observation": {
                "requestId": "request-1", "purpose": "generation", "attempt": 1,
                "provider": "test", "model": "test", "status": "ok", "durationMs": 1,
                "inputTokens": null, "outputTokens": null, "cachedInputTokens": null, "error": null
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
    let mut missing_definition = request.clone();
    missing_definition["record"]["observation"]["status"] = "started".into();
    missing_definition["record"]["context"] = serde_json::json!({
        "definitions": "missing-definition", "model_preferences": { "maxOutputTokens": 16 }
    });
    let mut missing_total = request.clone();
    missing_total["record"]["observation"]["inputTokens"] = 1.into();
    missing_total["record"]["observation"]["outputTokens"] = 1.into();
    for (entry, expected) in [
        (tool_result, "missing-tool"),
        (request, "request-1"),
        (missing_definition, "missing-definition"),
        (missing_total, "total"),
    ] {
        let session = server.create_session(&workspace.workspace_id).expect("create test session");
        let session_id = session.history.summary.thread_id;
        let path = sessions.join(format!("{session_id}.jsonl"));
        let mut bytes = std::fs::read(&path).expect("read test session");
        serde_json::to_writer(&mut bytes, &entry).expect("serialize damaged entry");
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).expect("write damaged test session");

        let error = server.read_session(&session_id, 40, None).expect_err("reject damaged history");
        assert_eq!(error.code, RpcErrorCode::Internal);
        assert!(error.message.contains(path.to_string_lossy().as_ref()), "{}", error.message);
        assert!(error.message.contains(expected), "{}", error.message);
        assert_eq!(std::fs::read(&path).expect("reread damaged session"), bytes);
        server
            .read_session(&healthy.history.summary.thread_id, 40, None)
            .expect("other sessions remain readable");
    }
}
