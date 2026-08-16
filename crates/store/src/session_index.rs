//! Lightweight session metadata index over the authoritative JSONL rollouts.

use super::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 会话索引状态：最近一次 turn 的状态。`None` 表示尚无 turn（新会话或迁移
/// 时终态未知）；`Active` 仅在 turn 真正运行期间写入，读取侧需要结合存活
/// turn 判定，崩溃遗留的 `Active` 由消费方投影为终态而不是回写索引。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

impl SessionStatus {
    pub const fn as_storage_text(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

/// `session_index` 的一行：只保存定位与展示会话所需的元数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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

/// 更新已有索引行时可修改的字段；`None` 表示不修改。
#[derive(Debug, Default)]
pub struct SessionMetadataUpdate<'a> {
    pub title: Option<Option<&'a str>>,
    pub model: Option<Option<&'a str>>,
    pub status: Option<SessionStatus>,
    pub token_usage: Option<&'a Value>,
}

impl SessionStore {
    pub fn insert_session(&self, record: &SessionRecord) -> StoreResult<()> {
        let changed = self.connection.execute(
            "insert into session_index(
                 session_id, rollout_path, cwd, title, model, status,
                 created_at, updated_at, token_usage
             ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.session_id,
                record.rollout_path,
                record.cwd,
                record.title,
                record.model,
                record.status.map(|status| status.as_storage_text()),
                record.created_at,
                record.updated_at,
                serde_json::to_string(&record.token_usage)?,
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidState(
                "session index insert did not create a row".to_string(),
            ));
        }
        Ok(())
    }

    /// 列出全部项目会话；`cwd` 直接来自索引，供 `sg threads` 展示。
    pub fn list_sessions(&self) -> StoreResult<Vec<SessionRecord>> {
        let mut statement = self.connection.prepare(
            "select session_id, rollout_path, cwd, title, model, status,
                    created_at, updated_at, token_usage
             from session_index
             order by updated_at desc, session_id",
        )?;
        let rows = statement.query_map([], |row| self.session_from_row(row))?;
        let mut sessions = Vec::new();
        for session in rows {
            sessions.push(session?);
        }
        Ok(sessions)
    }

    pub fn get_session(&self, session_id: &str) -> StoreResult<SessionRecord> {
        self.connection
            .query_row(
                "select session_id, rollout_path, cwd, title, model, status,
                        created_at, updated_at, token_usage
                 from session_index where session_id = ?1",
                params![session_id],
                |row| self.session_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("session {session_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    pub fn update_session(
        &self,
        session_id: &str,
        update: SessionMetadataUpdate<'_>,
    ) -> StoreResult<SessionRecord> {
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let current = transaction
            .query_row(
                "select session_id, rollout_path, cwd, title, model, status,
                        created_at, updated_at, token_usage
                 from session_index where session_id = ?1",
                params![session_id],
                |row| self.session_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("session {session_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let title = match update.title {
            Some(title) => title,
            None => current.title.as_deref(),
        };
        let model = match update.model {
            Some(model) => model,
            None => current.model.as_deref(),
        };
        let status = update.status.or(current.status);
        let token_usage = update.token_usage.unwrap_or(&current.token_usage);
        let updated_at = now_iso();
        transaction.execute(
            "update session_index
             set title = ?1, model = ?2, status = ?3,
                 updated_at = ?4, token_usage = ?5
             where session_id = ?6",
            params![
                title,
                model,
                status.map(|status| status.as_storage_text()),
                updated_at,
                serde_json::to_string(token_usage)?,
                session_id,
            ],
        )?;
        transaction.commit()?;
        self.get_session(session_id)
    }

    pub fn delete_session(&self, session_id: &str) -> StoreResult<()> {
        let changed = self.connection.execute(
            "delete from session_index where session_id = ?1",
            params![session_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("session {session_id}")));
        }
        Ok(())
    }

    fn session_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
        let status: Option<String> = row.get(5)?;
        let token_usage: String = row.get(8)?;
        let status = match status.as_deref() {
            None => None,
            Some(value) => Some(SessionStatus::from_storage_text(value).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(StoreError::InvalidState(format!(
                        "unknown session status database value {value:?}"
                    ))),
                )
            })?),
        };
        Ok(SessionRecord {
            session_id: row.get(0)?,
            rollout_path: row.get(1)?,
            cwd: row.get(2)?,
            title: row.get(3)?,
            model: row.get(4)?,
            status,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            token_usage: serde_json::from_str(&token_usage)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
        })
    }
}

/// 只读盘点旧项目 SQLite，供迁移前判定“是否只含空状态表”。
pub struct LegacySqliteReport {
    pub user_rows: u64,
}

pub fn inspect_legacy_sqlite(path: &Path) -> StoreResult<LegacySqliteReport> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let mut statement = connection.prepare(
        "select name from sqlite_master where type = 'table' and name not like 'sqlite_%'",
    )?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut user_rows = 0u64;
    for name in names {
        if name.starts_with("schema_") {
            continue;
        }
        let rows: i64 =
            connection.query_row(&format!("select count(*) from \"{name}\""), [], |row| {
                row.get(0)
            })?;
        user_rows = user_rows.saturating_add(rows as u64);
    }
    Ok(LegacySqliteReport { user_rows })
}

pub fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
