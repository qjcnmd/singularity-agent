//! 隔离的 session-ledger 与 workspace 测试夹具（feature `test-support`）。
//!
//! 全部确定性会话测试共用这一套夹具：每个夹具拥有独立的临时 home 与
//! sessions 目录，绝不触碰真实 `SINGULARITY_HOME`，也绝不触网。夹具只提供
//! 构造与打开入口，不拥有任何产品行为。
#![allow(clippy::expect_used)] // 夹具构造失败即测试环境损坏，直接 panic 是正确语义

use std::path::{Path, PathBuf};

use super::format::Result;
use super::manager::{SessionAccess, SessionManager};
use super::writer_lock::WriterLockCoordinator;
use std::sync::Arc;

/// 一个隔离的会话测试环境：临时 home、sessions 目录与进程级协调器。
pub struct SessionFixture {
    home: tempfile::TempDir,
    sessions_dir: PathBuf,
    coordinator: Arc<WriterLockCoordinator>,
}

impl SessionFixture {
    /// 新建隔离环境（临时目录随本结构 drop 清理）。
    pub fn new() -> Self {
        let home = tempfile::tempdir().expect("temp session home");
        let sessions_dir = home.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let coordinator = Arc::new(WriterLockCoordinator::new(&sessions_dir));
        Self {
            home,
            sessions_dir,
            coordinator,
        }
    }

    pub fn home(&self) -> &Path {
        self.home.path()
    }

    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub fn coordinator(&self) -> &Arc<WriterLockCoordinator> {
        &self.coordinator
    }

    /// 以指定 session id 创建会话（写者锁由本夹具协调器持有）。
    pub fn create_session(&self, cwd: &Path, session_id: &str) -> Result<SessionManager> {
        SessionManager::create_with_id_with_coordinator(
            cwd,
            &self.sessions_dir,
            session_id,
            &self.coordinator,
        )
    }

    /// 以写者意图重开既有会话（执行修复）。
    pub fn open_for_repair(&self, session_id: &str) -> Result<SessionManager> {
        SessionManager::open_existing_with_access(
            &self.session_path(session_id),
            &self.coordinator,
            session_id,
            SessionAccess::RepairWrite,
        )
    }

    /// 以只读意图重开既有会话（不修复、不取锁）。
    pub fn open_read_only(&self, session_id: &str) -> Result<SessionManager> {
        SessionManager::open_existing_read_only(&self.session_path(session_id))
    }

    pub fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.jsonl"))
    }
}

impl Default for SessionFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// 隔离的 workspace 夹具：一个临时工作目录，工具与项目指令测试的 cwd。
pub struct WorkspaceFixture {
    dir: tempfile::TempDir,
}

impl WorkspaceFixture {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("temp workspace"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// 在 workspace 内写入一个文件（自动建父目录）。
    pub fn write_file(&self, relative: &str, content: &str) {
        let path = self.dir.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write file");
    }

    /// 读取 workspace 内文件的当前内容。
    pub fn read_file(&self, relative: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(relative)).expect("read file")
    }

    /// 主动删除 workspace 目录（准备失败路径的 cwd 不可用注入）。
    pub fn remove(self) {
        self.dir.close().expect("remove workspace");
    }
}

impl Default for WorkspaceFixture {
    fn default() -> Self {
        Self::new()
    }
}
