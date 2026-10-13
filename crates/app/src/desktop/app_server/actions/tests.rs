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
