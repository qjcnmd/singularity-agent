//! `singularity` 命令行测试共享的无交互执行测试夹具。
//!
//! 提供隔离的临时会话目录、固定工作区和可注入的提供方实现；
//! 输出 sink 支持捕获字节流并支持确定性注入写失败场景。

#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use singularity_agent::session::test_support::WorkspaceFixture;
use singularity_model::Provider;
use singularity_runtime::test_support::{provider_snapshot, temp_sessions};
use singularity_runtime::{Conversation, ThreadCatalog, TurnRunner};

/// 一次无交互执行的全部句柄：协调器、thread id 与隔离守卫。
pub struct HeadlessFixture {
    pub home: tempfile::TempDir,
    workspace: Option<WorkspaceFixture>,
    pub conversation: Arc<Conversation>,
    pub thread_id: String,
}

impl HeadlessFixture {
    pub fn new(provider: Arc<dyn Provider + Send + Sync>) -> Self {
        let home = temp_sessions();
        let workspace = WorkspaceFixture::new();
        workspace.write_file("notes.txt", "alpha\n");
        let runner = Arc::new(
            TurnRunner::new(home.path().join("sessions"), provider_snapshot())
                .with_provider_override(provider),
        );
        let catalog = ThreadCatalog::new(&runner);
        let thread = catalog
            .create_thread(&workspace.path().to_string_lossy(), None)
            .expect("create thread");
        let thread_id = thread.thread_id.clone();
        Self {
            home,
            workspace: Some(workspace),
            conversation: Conversation::new(runner, thread).expect("open conversation"),
            thread_id,
        }
    }

    pub fn read_file(&self, relative: &str) -> String {
        self.workspace
            .as_ref()
            .expect("workspace present")
            .read_file(relative)
    }

    pub fn session_path(&self) -> std::path::PathBuf {
        self.home
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", self.thread_id))
    }
}

/// 读回会话 entries 记录（只读打开，不竞争写者）。
pub fn session_entries(fixture: &HeadlessFixture) -> Vec<singularity_agent::session::SessionEntry> {
    singularity_agent::session::SessionManager::open_existing_read_only(&fixture.session_path())
        .expect("reopen")
        .entries()
        .to_vec()
}

/// 读回会话 ledger 记录（只读打开，不竞争写者）。
pub fn session_records(fixture: &HeadlessFixture) -> Vec<singularity_agent::session::LedgerRecord> {
    session_records_at(&fixture.session_path())
}

pub fn session_records_at(path: &Path) -> Vec<singularity_agent::session::LedgerRecord> {
    singularity_agent::session::SessionManager::open_existing_read_only(path)
        .expect("reopen")
        .ledger_records()
}

/// 字节累积 sink：测试据此断言 stdout/stderr 的精确内容。
#[derive(Clone, Default)]
pub struct BufferedSink(pub Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for BufferedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl BufferedSink {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8 sink")
    }
}

/// 按内容注入写失败：当「已见字节 + 本次写入」首次包含指定子串时该次写入
/// 返回错误（子串可能被分块送达，必须跨块判定），其余照常累积。
/// 用于精确制造「事件行成功、summary 行失败」这类形状。
pub struct FailOnSubstring {
    inner: BufferedSink,
    pattern: &'static str,
    seen: String,
}

impl FailOnSubstring {
    pub fn new(inner: BufferedSink, pattern: &'static str) -> Self {
        Self {
            inner,
            pattern,
            seen: String::new(),
        }
    }
}

impl Write for FailOnSubstring {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let candidate = format!("{}{}", self.seen, String::from_utf8_lossy(buf));
        if candidate.contains(self.pattern) && !self.seen.contains(self.pattern) {
            return Err(std::io::Error::other("simulated stdout failure"));
        }
        self.seen = candidate;
        self.inner.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
