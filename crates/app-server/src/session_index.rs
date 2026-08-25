//! 进程内会话索引：JSONL rollout 是唯一持久事实源，索引只缓存定位与展示
//! 所需的元数据。启动时扫描 sessions 目录的 `*.jsonl` 重建，会话打开/更新
//! 时增量刷新对应记录；进程退出即消失，重启用 JSONL 再次重建。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use singularity_agent::session::{SessionManager, SessionProjectionStatus, project_session};
use thiserror::Error;

/// 会话索引状态：最近一次 turn 的状态。`None` 表示尚无 turn（新会话）；`Active`
/// 仅在 turn 真正运行期间写入，读取侧需要结合存活 turn 判定，崩溃遗留的 `Active`
/// 由消费方投影为终态而不是回写索引。
///
/// 该状态只有一份事实：协议层 `ThreadStatus`（同为 snake_case 存储文本），
/// 索引存/取与协议投影共用同一枚举。
pub use singularity_protocol::ThreadStatus as SessionStatus;

/// 会话索引的一行：只保存定位与展示会话所需的元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub rollout_path: String,
    pub cwd: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub status: Option<SessionStatus>,
    pub created_at: String,
    pub updated_at: String,
    pub token_usage: Value,
}

/// 更新已有索引记录时可修改的字段；`None` 表示不修改。
#[derive(Debug, Default)]
pub struct SessionMetadataUpdate<'a> {
    pub title: Option<Option<&'a str>>,
    pub model: Option<Option<&'a str>>,
    pub status: Option<SessionStatus>,
    pub token_usage: Option<&'a Value>,
}

/// 会话索引操作错误。
#[derive(Debug, Error)]
pub enum SessionIndexError {
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("invalid session index state: {0}")]
    InvalidState(String),
    #[error("session index io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type SessionIndexResult<T> = Result<T, SessionIndexError>;

pub use singularity_agent::session::now_iso;

/// 进程内会话索引：由唯一 AppServer owner 串行维护，turn worker 的终态更新
/// 通过内部锁与读取互斥。
#[derive(Debug)]
pub struct SessionIndex {
    sessions: Mutex<HashMap<String, SessionRecord>>,
}

impl Default for SessionIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionIndex {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 从 sessions 目录的 JSONL rollout 构建全新索引（启动路径）。
    pub fn from_sessions_dir(sessions_dir: &Path) -> SessionIndexResult<Self> {
        let index = Self::new();
        index.rebuild_from_sessions_dir(sessions_dir)?;
        Ok(index)
    }

    /// 从 sessions 目录的 JSONL rollout 重建整份索引。启动扫描以有界、只读、
    /// no-repair 方式解析每个 session 文件：严格验证文件名/UUID、版本号、
    /// header 及 entry 线性结构；跳过单个坏 rollout，不移动、不修改且不阻断
    /// 其它会话。从 JSONL 权威恢复 session id、cwd、created/updated、最新
    /// model、最新 terminal status、最新 factual usage 及首条 user message title。
    pub fn rebuild_from_sessions_dir(&self, sessions_dir: &Path) -> Result<(), SessionIndexError> {
        let mut rebuilt = HashMap::new();
        if sessions_dir.exists() {
            let entries = std::fs::read_dir(sessions_dir)?;
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                    continue;
                }
                let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let metadata = match std::fs::metadata(&path) {
                    Ok(m) => m,
                    Err(error) => {
                        eprintln!(
                            "skipping unreadable session file {}: {error}",
                            path.display()
                        );
                        continue;
                    }
                };
                if metadata.len() > 512 * 1024 * 1024 {
                    eprintln!(
                        "skipping oversized session file {}: {} bytes",
                        path.display(),
                        metadata.len()
                    );
                    continue;
                }
                let session = match SessionManager::open_existing_read_only(&path) {
                    Ok(session) => session,
                    Err(error) => {
                        eprintln!(
                            "skipping invalid session rollout {}: {error}",
                            path.display()
                        );
                        continue;
                    }
                };
                if session.session_id() != file_stem {
                    eprintln!(
                        "skipping mismatched session id in rollout {}: expected {}, got {}",
                        path.display(),
                        file_stem,
                        session.session_id()
                    );
                    continue;
                }
                rebuilt.insert(
                    session.session_id().to_string(),
                    record_from_session(&session, &path),
                );
            }
        }
        let mut sessions = self.sessions.lock().map_err(|_| {
            SessionIndexError::InvalidState("session index lock poisoned".to_string())
        })?;
        *sessions = rebuilt;
        Ok(())
    }

    /// 列出全部会话，按 updated_at 降序（同时间戳按 session_id）排序。
    pub fn list_sessions(&self) -> SessionIndexResult<Vec<SessionRecord>> {
        let sessions = self.sessions.lock().map_err(lock_poisoned)?;
        let mut records: Vec<SessionRecord> = sessions.values().cloned().collect();
        records.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(records)
    }

    pub fn get_session(&self, session_id: &str) -> SessionIndexResult<SessionRecord> {
        let sessions = self.sessions.lock().map_err(lock_poisoned)?;
        sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionIndexError::NotFound(format!("session {session_id}")))
    }

    /// 以 JSONL 解析结果重建或刷新单条索引投影（唯一的写入入口之一）。
    pub fn upsert_session(&self, record: &SessionRecord) -> SessionIndexResult<()> {
        let mut sessions = self.sessions.lock().map_err(lock_poisoned)?;
        sessions.insert(record.session_id.clone(), record.clone());
        Ok(())
    }

    pub fn insert_session(&self, record: &SessionRecord) -> SessionIndexResult<()> {
        let mut sessions = self.sessions.lock().map_err(lock_poisoned)?;
        if sessions.contains_key(&record.session_id) {
            return Err(SessionIndexError::InvalidState(format!(
                "session index insert collision for {}",
                record.session_id
            )));
        }
        sessions.insert(record.session_id.clone(), record.clone());
        Ok(())
    }

    pub fn update_session(
        &self,
        session_id: &str,
        update: SessionMetadataUpdate<'_>,
    ) -> SessionIndexResult<SessionRecord> {
        let mut sessions = self.sessions.lock().map_err(lock_poisoned)?;
        let mut current = sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| SessionIndexError::NotFound(format!("session {session_id}")))?;
        current.title = match update.title {
            Some(title) => title.map(str::to_string),
            None => current.title,
        };
        current.model = match update.model {
            Some(model) => model.map(str::to_string),
            None => current.model,
        };
        current.status = update.status.or(current.status);
        if let Some(token_usage) = update.token_usage {
            current.token_usage = token_usage.clone();
        }
        current.updated_at = now_iso();
        sessions.insert(session_id.to_string(), current.clone());
        Ok(current)
    }

    pub fn delete_session(&self, session_id: &str) -> SessionIndexResult<()> {
        let mut sessions = self.sessions.lock().map_err(lock_poisoned)?;
        if sessions.remove(session_id).is_none() {
            return Err(SessionIndexError::NotFound(format!("session {session_id}")));
        }
        Ok(())
    }
}

fn lock_poisoned<T>(_: std::sync::PoisonError<T>) -> SessionIndexError {
    SessionIndexError::InvalidState("session index lock poisoned".to_string())
}

/// 从已打开的 SessionManager 与其 rollout 路径投影索引记录：model 取最新
/// thread_settings、status 取最新 terminal 标记、usage 取最新 factual usage、
/// title 取首条 user 消息。
fn record_from_session(session: &SessionManager, rollout_path: &Path) -> SessionRecord {
    let projection = project_session(session);
    let status = projection.status.map(|status| match status {
        SessionProjectionStatus::Active => SessionStatus::Active,
        SessionProjectionStatus::Completed => SessionStatus::Completed,
        SessionProjectionStatus::Failed => SessionStatus::Failed,
        SessionProjectionStatus::Interrupted => SessionStatus::Interrupted,
    });
    SessionRecord {
        session_id: projection.session_id,
        rollout_path: rollout_path.to_string_lossy().to_string(),
        cwd: projection.cwd,
        title: projection.title,
        model: projection.model,
        status,
        created_at: projection.created_at,
        updated_at: projection.updated_at,
        token_usage: projection
            .latest_usage
            .unwrap_or_else(|| serde_json::json!({})),
    }
}
