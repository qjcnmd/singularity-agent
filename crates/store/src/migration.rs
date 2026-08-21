//! Session index schema creation and schema-version validation.
//!
//! SQLite 只保存会话元数据；会话正文唯一权威是 JSONL rollout 文件。
//! 模式版本只通过 `PRAGMA user_version` 标记，不做表结构内省比对。

use super::*;

pub(crate) const CURRENT_SCHEMA_SQL: &str = r#"
create table if not exists session_index(
session_id text primary key,
rollout_path text not null,
cwd text not null,
title text,
model text,
status text
    check(status in ('active', 'completed', 'failed', 'interrupted')),
created_at text not null,
updated_at text not null,
token_usage text not null
);
"#;

pub(crate) const CURRENT_INDEX_SQL: &str = r#"
create index if not exists session_index_updated_at on session_index(updated_at);
create index if not exists session_index_cwd on session_index(cwd);
"#;

/// 在新库或空库上创建当前 schema，并把版本标记写入 `PRAGMA user_version`。
pub(crate) fn initialize_or_validate_schema(connection: &Connection) -> StoreResult<()> {
    match user_version(connection)? {
        0 => create_current_schema(connection),
        SCHEMA_VERSION => Ok(()),
        found => Err(StoreError::UnsupportedSchema {
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

/// 校验已初始化库的 schema 版本标记是当前实现的版本（不创建 schema）。
pub(crate) fn validate_current_schema(connection: &Connection) -> StoreResult<()> {
    match user_version(connection)? {
        SCHEMA_VERSION => Ok(()),
        found => Err(StoreError::UnsupportedSchema {
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

fn create_current_schema(connection: &Connection) -> StoreResult<()> {
    let transaction =
        rusqlite::Transaction::new_unchecked(connection, rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(CURRENT_SCHEMA_SQL)?;
    transaction.execute_batch(CURRENT_INDEX_SQL)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION as i64)?;
    transaction.commit()?;
    Ok(())
}

fn user_version(connection: &Connection) -> StoreResult<u32> {
    let version: i64 = connection.query_row("pragma user_version", [], |row| row.get(0))?;
    Ok(version as u32)
}
