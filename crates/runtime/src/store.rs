//! Thread 目录操作：创建、定位、修复重开、只读分页投影与归档。
//!
//! JSONL 会话文件是唯一持久事实源；这里只做路径、权限与打开/修复的统一
//! 入口，不复制会话状态。[`ThreadCatalog`] 吸收 `sessions_dir` 与写者锁协调器，
//! 布局与纯函数（[`SESSIONS_DIR_NAME`]、[`thread_session_path`]、
//! [`prepare_session_dirs`]）经 crate 根导出。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use singularity_agent::session::{
    SessionAccess, SessionEntry, SessionError, SessionManager, WriterLockCoordinator,
    project_session,
};
use singularity_protocol::ThreadTurn;
use uuid::Uuid;

use crate::history::project_turn_history;
use crate::objects::Thread;
use crate::runner::TurnRunner;

/// 进程级写者锁协调器：TurnRunner 构造一次并贯穿所有会话打开路径，stale
/// 清理每进程只发生一次。从 store 层向下传给 `SessionManager`。
pub type ThreadLockCoordinator = Arc<WriterLockCoordinator>;

/// Thread 摘要的唯一结构由会话层拥有（JSONL 派生事实的投影者），runtime
/// 只转发导出；全链路（store、目录、TUI）共用同一类型。
pub use singularity_agent::session::ThreadSummary;

pub const SESSIONS_DIR_NAME: &str = "sessions";

/// Thread 目录操作与只读投影的入口。
#[derive(Clone)]
pub struct ThreadCatalog {
    sessions_dir: PathBuf,
    coordinator: ThreadLockCoordinator,
}

impl ThreadCatalog {
    pub fn new(runner: &TurnRunner) -> Self {
        Self {
            sessions_dir: runner.sessions_dir().to_path_buf(),
            coordinator: Arc::clone(runner.coordinator()),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_parts(sessions_dir: PathBuf, coordinator: ThreadLockCoordinator) -> Self {
        Self {
            sessions_dir,
            coordinator,
        }
    }
}

/// 创建 home 下的 sessions 目录（Unix 收紧为属主专用）。
pub fn prepare_session_dirs(home: &Path) -> Result<(), String> {
    singularity_core::create_owner_only_dir(&home.join(SESSIONS_DIR_NAME))?;
    Ok(())
}

/// Thread 会话文件的规范位置。
pub fn thread_session_path(sessions_dir: &Path, thread_id: &str) -> PathBuf {
    sessions_dir.join(format!("{thread_id}.jsonl"))
}

/// 创建新 Thread（uuid v7 会话文件，属主权限）。
///
/// 传入的 `cwd` 只是起点：会话层把它归一为绝对路径并写入会话头，返回的
/// Thread 直接采用会话头记录的字符串，因此新建、恢复与列表三条路径上的
/// 同一事实共享一个写法。
impl ThreadCatalog {
    pub fn create_thread(&self, cwd: &str, model: Option<String>) -> Result<Thread, String> {
        let thread_id = Uuid::now_v7().to_string();
        let session = SessionManager::create_with_id_with_coordinator(
            Path::new(cwd),
            &self.sessions_dir,
            &thread_id,
            &self.coordinator,
        )
        .map_err(|_| "failed to create session file".to_string())?;
        singularity_core::ensure_owner_only_file(session.path())?;
        Ok(Thread {
            thread_id,
            cwd: session.cwd_string(),
            model,
        })
    }
}

/// 重开既有 Thread 并执行崩溃修复；返回投影后的 Thread。
///
/// 修复语义与 turn 打开路径一致：未终态的 run operation 补写 synthetic
/// `operation_finished`（interrupted），已启动而未落结果的 `replay: never` 工具
/// 补写 synthetic failed ToolResult，绝不重放。管理器在投影后关闭；每个 turn
/// 由 runner 按单写者合同重新独占打开。
impl ThreadCatalog {
    pub fn resume_thread(&self, thread_id: &str) -> Result<Thread, ResumeError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        if !path.exists() {
            return Err(ResumeError::NotFound(thread_id.to_string()));
        }
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::RepairWrite,
        )
        .map_err(|error| ResumeError::Store(error.to_string()))?;
        singularity_agent::session::context::ContextView::validate(&session)
            .map_err(|error| ResumeError::Store(error.to_string()))?;
        let projection = project_session(&session, false);
        let thread = Thread {
            thread_id: thread_id.to_string(),
            cwd: session.cwd_string(),
            model: projection.model,
        };
        Ok(thread)
    }
}

/// 会话列表项：头部事实 + 文件 mtime。列表只读每个文件的首行，
/// 不解析条目、不做聚合；完整投影由单文件入口（`read_thread_summary`）按需承担。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadListing {
    pub thread_id: String,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
}

/// 列出可恢复 Thread；损坏或非规范文件不会阻断其余会话。
impl ThreadCatalog {
    pub fn list_threads(&self) -> Result<Vec<ThreadListing>, String> {
        if !self.sessions_dir.exists() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&self.sessions_dir)
            .map_err(|error| format!("failed to list sessions: {error}"))?;
        let mut threads = Vec::new();
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(header) = singularity_agent::session::read_session_header(&path) else {
                continue;
            };
            if path.file_stem().and_then(|value| value.to_str()) != Some(header.session_id.as_str())
            {
                continue;
            }
            let Some(updated_at) = singularity_agent::session::file::file_modified_iso(&path)
            else {
                continue;
            };
            threads.push(ThreadListing {
                thread_id: header.session_id,
                cwd: header.cwd,
                created_at: header.created_at,
                updated_at,
            });
        }
        threads.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
        Ok(threads)
    }
}

fn open_thread_read_only(
    sessions_dir: &Path,
    thread_id: &str,
) -> Result<SessionManager, ResumeError> {
    let path = thread_session_path(sessions_dir, thread_id);
    if !path.exists() {
        return Err(ResumeError::NotFound(thread_id.to_string()));
    }
    let session = SessionManager::open_existing_read_only(&path)
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    session
        .verify_session_id(thread_id)
        .map_err(|error| ResumeError::Store(error.to_string()))?;
    Ok(session)
}

/// 只读投影一个 Thread；不执行崩溃修复或写入。
impl ThreadCatalog {
    pub fn read_thread_summary(&self, thread_id: &str) -> Result<ThreadSummary, ResumeError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        Ok(project_session(
            &session,
            self.coordinator.has_local_run(thread_id),
        ))
    }
}

/// 为 Thread 追加名称 metadata；JSONL 仍是唯一事实源。
impl ThreadCatalog {
    pub fn rename(&self, thread_id: &str, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("thread name must not be empty".to_string());
        }
        let path = thread_session_path(&self.sessions_dir, thread_id);
        let mut session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .map_err(|error| error.to_string())?;
        session
            .append_metadata(singularity_agent::session::SessionMetadata::thread_name(
                name,
            ))
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// Thread 定位与持久化错误。
#[derive(Debug, thiserror::Error)]
pub enum ResumeError {
    #[error("thread {0} was not found")]
    NotFound(String),
    #[error("thread has an active writer")]
    WriterActive,
    #[error("before item {0} was not found in the thread history")]
    AnchorNotFound(String),
    #[error("{0}")]
    Store(String),
}

/// thread/read 的一页只读投影：摘要 + 最近一次 compaction 摘要 + 按轮分页的
/// 公开历史。
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadReadPage {
    pub summary: ThreadSummary,
    /// 最近一次 compaction 摘要；无 compaction 时为 None。
    pub compaction_summary: Option<String>,
    /// 本页轮次，按会话顺序（旧→新）排列。
    pub turns: Vec<ThreadTurn>,
}

/// 单次无锁只读解析完成 thread/read 的全部投影：摘要 + 分页条目 +
/// 状态/用量投影。分页是单向往回读：默认返回最新 `limit` 轮；给
/// `before_item`（上一页最旧轮内任意 item 的公开 id）则定位其所属轮，
/// 返回该轮之前的 `limit` 轮。未知锚点返回 [`ResumeError::AnchorNotFound`]。
///
impl ThreadCatalog {
    pub fn paged_read(
        &self,
        thread_id: &str,
        limit: usize,
        before_item: Option<&str>,
    ) -> Result<ThreadReadPage, ResumeError> {
        let session = open_thread_read_only(&self.sessions_dir, thread_id)?;
        let entries = session.entries();
        let live_run = self.coordinator.has_local_run(thread_id);
        let summary = project_session(&session, live_run);
        let compaction_summary = entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Compaction { compaction, .. } => Some(compaction.summary.clone()),
            _ => None,
        });
        let mut turns = project_turn_history(entries, live_run);
        let before_index = match before_item {
            None => None,
            Some(anchor) => match turns
                .iter()
                .position(|turn| turn.items.iter().any(|item| item.id() == anchor))
            {
                Some(index) => Some(index),
                None => return Err(ResumeError::AnchorNotFound(anchor.to_string())),
            },
        };
        let page_start = before_index.unwrap_or(turns.len()).saturating_sub(limit);
        let page_end = before_index.unwrap_or(turns.len());
        turns = turns[page_start..page_end].to_vec();
        Ok(ThreadReadPage {
            summary,
            compaction_summary,
            turns,
        })
    }
}

/// 归档会话的子目录（相对 sessions_dir）：删除改为归档保留，列表/摘要
/// 扫描只读顶层 `.jsonl`，对 `archived/` 天然跳过——这是列表过滤的耦合
/// 前提，改动扫描方式时必须复核。
pub const ARCHIVED_SESSIONS_DIR_NAME: &str = "archived";

/// 归档 Thread 的会话文件：从 sessions 顶层 rename 进 `archived/` 子目录，
/// 归档保留而非物理删除。持写者锁完成：其他写者正在 append 时拒绝
///（[`ResumeError::WriterActive`]），避免归档窗口内写入落入 unlinked inode。
/// 同 id 已归档或原文件不存在时语义等同 [`ResumeError::NotFound`]。
impl ThreadCatalog {
    pub fn archive(&self, thread_id: &str) -> Result<(), ResumeError> {
        let path = thread_session_path(&self.sessions_dir, thread_id);
        if !path.exists() {
            return Err(ResumeError::NotFound(thread_id.to_string()));
        }
        let archived_dir = self.sessions_dir.join(ARCHIVED_SESSIONS_DIR_NAME);
        if let Err(error) = std::fs::create_dir_all(&archived_dir) {
            return Err(ResumeError::Store(format!(
                "failed to create archive directory {}: {error}",
                archived_dir.display()
            )));
        }
        let archived = archived_dir.join(format!("{thread_id}.jsonl"));
        if archived.exists() {
            // 同 id 已归档：语义等同 NotFound（重复归档无新动作）。
            return Err(ResumeError::NotFound(thread_id.to_string()));
        }
        let session = SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .map_err(|error| match error {
            SessionError::WriterConflict { .. } => ResumeError::WriterActive,
            other => ResumeError::Store(other.to_string()),
        })?;
        // 锁释放前先把会话文件挪出原路径：窗口内新写者 open 原路径得
        // NotFound，不会再 append 进即将归档的文件。
        if let Err(error) = std::fs::rename(&path, &archived) {
            // Windows 可能拒绝移动当前进程仍打开的会话文件。释放句柄后重试
            // 归档；仍失败再返回包含两段原因的错误。
            drop(session);
            return std::fs::rename(&path, &archived).map_err(|retry_error| {
                ResumeError::Store(format!(
                    "failed to archive session rollout {}: {error}; {retry_error}",
                    path.display()
                ))
            });
        }
        drop(session);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
    use super::*;
    use singularity_agent::message::{AgentMessage, AgentMessageRole};
    use singularity_agent::session::{
        LedgerRecord, OperationIntent, OperationKind, ToolReplayClass,
    };
    use singularity_model::{
        ModelConfigurationSnapshot, ProviderApiProtocol, ProviderProtocolContract, TurnRetryPolicy,
    };
    use singularity_protocol::{TurnModelUsage, TurnStatus};

    fn run_operation(operation_id: &str, turn_id: &str) -> LedgerRecord {
        LedgerRecord::OperationStarted {
            operation_id: operation_id.to_string(),
            kind: OperationKind::Run,
            turn_id: Some(turn_id.to_string()),
            intent: OperationIntent::Run {
                model: ModelConfigurationSnapshot {
                    provider: "openai_compatible".to_string(),
                    model: "test-model-a".to_string(),
                    reasoning_variant: None,
                    protocol: ProviderApiProtocol::OpenAiChatCompletions,
                    capabilities: ProviderProtocolContract::default(),
                    credential_provenance: "test".to_string(),
                    retry: TurnRetryPolicy::default(),
                },
                input: String::new(),
            },
        }
    }

    /// 构造一轮已完成 + 一轮未终止的会话（格式 v4 operation 记录），返回首轮
    /// message 的 entry id（供锚点定位）。
    fn session_with_two_turns(sessions_dir: &Path, thread_id: &str) -> String {
        let mut session = SessionManager::create_with_id(Path::new("."), sessions_dir, thread_id)
            .expect("create session");
        session
            .append_record(run_operation("op-1", "turn-1"))
            .expect("append turn start");
        let message_id = session
            .append_message(AgentMessage::text(
                AgentMessageRole::User,
                "first turn text",
            ))
            .expect("append message");
        session
            .append_record(LedgerRecord::OperationFinished {
                operation_id: "op-1".to_string(),
                turn_id: Some("turn-1".to_string()),
                outcome: TurnStatus::Completed,
                usage: Some(TurnModelUsage {
                    input_tokens: 1,
                    usage_present: true,
                    usage_complete: true,
                    ..TurnModelUsage::default()
                }),
                truncated: false,
            })
            .expect("append turn terminal");
        session
            .append_record(run_operation("op-2", "turn-2"))
            .expect("append second turn start");
        message_id
    }

    #[test]
    fn archive_thread_archives_and_rejects_active_writer() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sessions_dir = temp.path().join("sessions");
        let thread_id = "01914f6b-0000-7000-8000-000000000002";
        let _ = session_with_two_turns(&sessions_dir, thread_id);
        let coordinator = Arc::new(WriterLockCoordinator::new(&sessions_dir));
        let catalog = ThreadCatalog::from_parts(sessions_dir.clone(), Arc::clone(&coordinator));

        let session = SessionManager::open_existing_with_access(
            &thread_session_path(&sessions_dir, thread_id),
            &coordinator,
            thread_id,
            SessionAccess::Append,
        )
        .expect("open session");
        // 存活写者持有锁：归档必须拒绝，避免归档窗口内写入落入 unlinked inode。
        match catalog.archive(thread_id) {
            Err(ResumeError::WriterActive) => {}
            other => panic!("expected WriterActive, got {other:?}"),
        }
        drop(session);
        catalog
            .archive(thread_id)
            .expect("archive after writer release");
        // 原路径不再存在，列表不可见；归档目录内保留文件。
        assert!(!thread_session_path(&sessions_dir, thread_id).exists());
        let archived = sessions_dir
            .join(ARCHIVED_SESSIONS_DIR_NAME)
            .join(format!("{thread_id}.jsonl"));
        assert!(archived.exists(), "archived copy must be preserved");
        // 列表/摘要扫描天然跳过 archived/ 子目录：归档后不可恢复为活动会话。
        assert!(
            catalog
                .list_threads()
                .expect("list threads")
                .iter()
                .all(|thread| thread.thread_id != thread_id),
            "archived thread must not appear in the active list"
        );
        match catalog.read_thread_summary(thread_id) {
            Err(ResumeError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        // 重复归档：语义等同 NotFound。
        match catalog.archive(thread_id) {
            Err(ResumeError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// T017 seam：崩溃遗留的未终结 run（含已启动的 `replay: never` 工具）在
    /// resume 时被收敛为 interrupted，投影不再报告 running，且修复不重放工具。
    #[test]
    fn resume_converges_crashed_open_operation_to_interrupted() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sessions_dir = temp.path().join("sessions");
        let thread_id = "01914f6b-0000-7000-8000-000000000003";
        {
            let mut session =
                SessionManager::create_with_id(Path::new("."), &sessions_dir, thread_id)
                    .expect("create session");
            session
                .append_record(run_operation("op-1", "turn-1"))
                .expect("start run");
            session
                .append_message(AgentMessage::text(AgentMessageRole::User, "go"))
                .expect("append user");
            session
                .append_record(LedgerRecord::ToolStarted {
                    operation_id: "op-1".to_string(),
                    tool_call_id: "call-1".to_string(),
                    tool_name: "bash".to_string(),
                    source_order: 0,
                    effective_args: serde_json::json!({"command": "rm -rf /tmp/x"}),
                    result_entry_id: "res-1".to_string(),
                    replay: ToolReplayClass::Never,
                })
                .expect("start never-replay tool");
            // 崩溃：operation 未终结即丢失进程。
        }

        let coordinator = Arc::new(WriterLockCoordinator::new(&sessions_dir));
        let catalog = ThreadCatalog::from_parts(sessions_dir.clone(), coordinator);
        catalog.resume_thread(thread_id).expect("resume");
        assert_eq!(
            catalog
                .read_thread_summary(thread_id)
                .expect("summary projection")
                .status,
            Some(TurnStatus::Interrupted),
            "crashed open run must project as interrupted after repair"
        );

        // 重开后 operation 已终结，且未产生任何新的工具执行事实（不重放）。
        let reopened =
            SessionManager::open_existing_read_only(&thread_session_path(&sessions_dir, thread_id))
                .expect("reopen");
        let records = reopened.ledger_records();
        let finished = records.iter().find_map(|record| match record {
            LedgerRecord::OperationFinished {
                operation_id,
                outcome,
                ..
            } if operation_id == "op-1" => Some(*outcome),
            _ => None,
        });
        assert_eq!(finished, Some(TurnStatus::Interrupted));
        let started_tools = records
            .iter()
            .filter(|record| matches!(record, LedgerRecord::ToolStarted { .. }))
            .count();
        assert_eq!(
            started_tools, 1,
            "repair must not start a new tool execution"
        );
    }
}
