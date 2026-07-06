#![forbid(unsafe_code)]

use std::path::Path;

use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_policy::{ApprovalDecision, ApprovalRequest};
use singularity_protocol::{
    ArtifactRef, Item, ItemKind, ItemStatus, Thread, ThreadStatus, TraceEvent, Turn, TurnStatus,
};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: u32 = 2;
const INITIAL_SCHEMA_MIGRATION: &str = "0001_initial_session_store";
const DURABLE_LEDGER_SCHEMA_MIGRATION: &str = "0002_durable_ledger";
const REDACTED_ARTIFACT_VALUE: &str = "[redacted]";
const SENSITIVE_ARTIFACT_MARKERS: [&str; 5] =
    ["api_key", "authorization", "password", "secret", "token"];

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("record not found: {0}")]
    NotFound(String),
    #[error("record already exists: {0}")]
    AlreadyExists(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionStoreDescriptor {
    pub backend: String,
    pub path: String,
    pub schema_version: u32,
}

pub struct SessionStore {
    connection: Connection,
    descriptor: SessionStoreDescriptor,
}

impl SessionStore {
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref();
        let connection = Connection::open(path)?;
        let store = Self {
            connection,
            descriptor: SessionStoreDescriptor {
                backend: "sqlite".to_string(),
                path: path.to_string_lossy().to_string(),
                schema_version: SCHEMA_VERSION,
            },
        };
        store.init_schema()?;
        Ok(store)
    }

    pub fn descriptor(&self) -> &SessionStoreDescriptor {
        &self.descriptor
    }

    pub fn create_thread(&self, model: Option<&str>, cwd: Option<&str>) -> StoreResult<Thread> {
        let thread = Self::new_thread(model, cwd);
        Self::insert_thread(&self.connection, &thread)?;
        Ok(thread)
    }

    pub fn list_threads(&self) -> StoreResult<Vec<Thread>> {
        let mut statement = self
            .connection
            .prepare("select thread_id, model, cwd, status from threads order by rowid")?;
        let rows = statement.query_map([], |row| self.thread_from_row(row))?;
        let mut threads = Vec::new();
        for row in rows {
            threads.push(row?);
        }
        Ok(threads)
    }

    pub fn get_thread(&self, thread_id: &str) -> StoreResult<Thread> {
        self.connection
            .query_row(
                "select thread_id, model, cwd, status from threads where thread_id = ?1",
                params![thread_id],
                |row| self.thread_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("thread {thread_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    pub fn update_thread_status(
        &self,
        thread_id: &str,
        status: ThreadStatus,
    ) -> StoreResult<Thread> {
        let changed = self.connection.execute(
            "update threads set status = ?1 where thread_id = ?2",
            params![serde_json::to_string(&status)?, thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        self.get_thread(thread_id)
    }

    pub fn delete_thread(&self, thread_id: &str) -> StoreResult<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "delete from threads where thread_id = ?1",
            params![thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        transaction.execute(
            "delete from items where turn_id in (select turn_id from turns where thread_id = ?1)",
            params![thread_id],
        )?;
        transaction.execute("delete from turns where thread_id = ?1", params![thread_id])?;
        transaction.execute(
            "delete from trace_events where run_id = ?1 or session_id = ?1",
            params![thread_id],
        )?;
        transaction.execute(
            "delete from artifact_refs where run_id = ?1",
            params![thread_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_thread_with_trace(
        &self,
        model: Option<&str>,
        cwd: Option<&str>,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Thread, TraceEvent)> {
        let transaction = self.connection.unchecked_transaction()?;
        let thread = Self::new_thread(model, cwd);
        Self::insert_thread(&transaction, &thread)?;
        let trace = TraceEvent::new(
            format!("trace_{}", thread.thread_id),
            thread.thread_id.clone(),
            thread.thread_id.clone(),
            component,
            summary,
        );
        Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok((thread, trace))
    }

    pub fn create_turn(&self, thread_id: &str, agent_loop_status: &str) -> StoreResult<Turn> {
        if !self.thread_exists(thread_id)? {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        let turn = Self::new_turn(thread_id, agent_loop_status);
        Self::insert_turn(&self.connection, &turn)?;
        Ok(turn)
    }

    pub fn create_turn_with_input_and_trace(
        &self,
        thread_id: &str,
        agent_loop_status: &str,
        input: Value,
        component: &str,
        summary: &str,
    ) -> StoreResult<(Turn, Item, TraceEvent)> {
        let transaction = self.connection.unchecked_transaction()?;
        if !Self::exists_in_transaction(
            &transaction,
            "select 1 from threads where thread_id = ?1",
            thread_id,
        )? {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        let turn = Self::new_turn(thread_id, agent_loop_status);
        Self::insert_turn(&transaction, &turn)?;
        let item = Self::new_item(&turn.turn_id, ItemKind::UserMessage, input);
        Self::insert_item(&transaction, &item)?;
        let trace = TraceEvent::new(
            format!("trace_{}", turn.turn_id),
            thread_id,
            turn.turn_id.clone(),
            component,
            summary,
        );
        Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok((turn, item, trace))
    }

    pub fn get_turn(&self, turn_id: &str) -> StoreResult<Turn> {
        self.connection
            .query_row(
                "select turn_id, thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| self.turn_from_row(row),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::NotFound(format!("turn {turn_id}")),
                other => StoreError::Sqlite(other),
            })
    }

    pub fn update_turn_status(&self, turn_id: &str, status: TurnStatus) -> StoreResult<Turn> {
        let changed = self.connection.execute(
            "update turns set status = ?1 where turn_id = ?2",
            params![serde_json::to_string(&status)?, turn_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        self.get_turn(turn_id)
    }

    pub fn append_item(&self, turn_id: &str, kind: ItemKind, payload: Value) -> StoreResult<Item> {
        if !self.turn_exists(turn_id)? {
            return Err(StoreError::NotFound(format!("turn {turn_id}")));
        }
        let item = Self::new_item(turn_id, kind, payload);
        Self::insert_item(&self.connection, &item)?;
        Ok(item)
    }

    pub fn append_trace(&self, event: &TraceEvent) -> StoreResult<()> {
        Self::insert_trace(&self.connection, event)?;
        Ok(())
    }

    pub fn list_trace(&self, run_id: &str) -> StoreResult<Vec<TraceEvent>> {
        self.list_trace_page(run_id, None, None)
    }

    pub fn list_trace_page(
        &self,
        run_id: &str,
        limit: Option<usize>,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = limit.unwrap_or(usize::MAX);
        let offset = offset.unwrap_or(0);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let mut statement = self.connection.prepare(
            "select payload from trace_events where run_id = ?1 order by rowid limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            row.get::<_, String>(0)
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        if events.is_empty() {
            if self.trace_run_exists(run_id)? {
                return Ok(events);
            }
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        Ok(events)
    }

    pub fn tail_trace(
        &self,
        run_id: &str,
        limit: usize,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let mut events = self.list_trace(run_id)?;
        if let Some(offset) = offset {
            let keep = events.len().saturating_sub(offset);
            events.truncate(keep);
        }
        if events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
        Ok(events)
    }

    pub fn show_trace(&self, event_id: &str) -> StoreResult<TraceEvent> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from trace_events where event_id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("trace event {event_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub fn create_approval(&self, request: &ApprovalRequest) -> StoreResult<()> {
        insert_approval(&self.connection, request)?;
        Ok(())
    }

    pub fn create_approval_with_trace(
        &self,
        request: &ApprovalRequest,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        let transaction = self.connection.unchecked_transaction()?;
        insert_approval(&transaction, request)?;
        let trace = TraceEvent::new(
            format!("trace_{}", request.request_id),
            request.request_id.clone(),
            request.session_id.clone(),
            component,
            summary,
        );
        Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok(trace)
    }

    pub fn list_pending_approvals(&self) -> StoreResult<Vec<ApprovalRequest>> {
        let mut statement = self.connection.prepare(
            "select payload from approvals where decision_outcome is null order by rowid",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(serde_json::from_str(&row?)?);
        }
        Ok(approvals)
    }

    pub fn list_approval_decisions(&self) -> StoreResult<Vec<ApprovalDecision>> {
        let mut statement = self
            .connection
            .prepare("select payload from approval_decisions order by rowid")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut decisions = Vec::new();
        for row in rows {
            decisions.push(serde_json::from_str(&row?)?);
        }
        Ok(decisions)
    }

    pub fn record_approval_decision(
        &self,
        decision: &ApprovalDecision,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        let transaction = self.connection.unchecked_transaction()?;
        let request_payload: String = transaction
            .query_row(
                "select payload from approvals where request_id = ?1 and decision_outcome is null",
                params![decision.request_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {}", decision.request_id))
                }
                other => StoreError::Sqlite(other),
            })?;
        let request: ApprovalRequest = serde_json::from_str(&request_payload)?;
        let changed = transaction.execute(
            "update approvals set decision_outcome = ?1, decision_reason = ?2 where request_id = ?3 and decision_outcome is null",
            params![
                serde_json::to_string(&decision.outcome)?,
                decision.reason,
                decision.request_id
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!(
                "approval {}",
                decision.request_id
            )));
        }
        transaction.execute(
            "insert into approval_decisions(decision_id, request_id, outcome, reason, payload) values(?1, ?2, ?3, ?4, ?5)",
            params![
                decision.decision_id,
                decision.request_id,
                serde_json::to_string(&decision.outcome)?,
                decision.reason,
                serde_json::to_string(decision)?
            ],
        )?;
        let trace = TraceEvent::new(
            format!("trace_{}", decision.decision_id),
            request.session_id.clone(),
            request.session_id,
            component,
            summary,
        );
        let trace = TraceEvent {
            task_id: Some(request.task_id),
            payload: serde_json::json!({
                "request_id": decision.request_id,
                "decision_id": decision.decision_id,
                "outcome": decision.outcome,
            }),
            ..trace
        };
        Self::insert_trace(&transaction, &trace)?;
        transaction.commit()?;
        Ok(trace)
    }

    pub fn get_approval_decision(&self, decision_id: &str) -> StoreResult<ApprovalDecision> {
        let payload: String = self
            .connection
            .query_row(
                "select payload from approval_decisions where decision_id = ?1",
                params![decision_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval decision {decision_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub fn register_artifact_ref(
        &self,
        run_id: &str,
        item_id: Option<&str>,
        kind: &str,
        uri: &str,
        content_digest: &str,
        summary: &str,
        metadata: Value,
    ) -> StoreResult<ArtifactRef> {
        let artifact = ArtifactRef {
            artifact_id: format!("artifact_{}", short_id()),
            run_id: run_id.to_string(),
            item_id: item_id.map(str::to_string),
            kind: kind.to_string(),
            uri: redact_secret_like_text(uri),
            content_digest: content_digest.to_string(),
            summary: redact_secret_like_text(summary),
            redacted: artifact_needs_redaction(uri, summary, &metadata),
            metadata: redact_secret_like_value(metadata),
        };
        self.connection.execute(
            "insert into artifact_refs(artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted) values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                artifact.artifact_id,
                artifact.run_id,
                artifact.item_id,
                artifact.kind,
                artifact.uri,
                artifact.content_digest,
                artifact.summary,
                serde_json::to_string(&artifact.metadata)?,
                artifact.redacted,
            ],
        )?;
        Ok(artifact)
    }

    pub fn get_artifact_ref(&self, artifact_id: &str) -> StoreResult<ArtifactRef> {
        self.connection
            .query_row(
                "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where artifact_id = ?1",
                params![artifact_id],
                artifact_from_row,
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("artifact {artifact_id}"))
                }
                other => StoreError::Sqlite(other),
            })
    }

    pub fn list_artifact_refs(&self, run_id: &str) -> StoreResult<Vec<ArtifactRef>> {
        let mut statement = self.connection.prepare(
            "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where run_id = ?1 order by rowid",
        )?;
        let rows = statement.query_map(params![run_id], artifact_from_row)?;
        let mut artifacts = Vec::new();
        for row in rows {
            artifacts.push(row?);
        }
        Ok(artifacts)
    }

    pub fn applied_migrations(&self) -> StoreResult<Vec<String>> {
        let mut statement = self
            .connection
            .prepare("select migration_id from schema_migrations order by migration_id")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut migrations = Vec::new();
        for row in rows {
            migrations.push(row?);
        }
        Ok(migrations)
    }

    fn trace_run_exists(&self, run_id: &str) -> StoreResult<bool> {
        self.exists(
            "select 1 from trace_events where run_id = ?1 limit 1",
            run_id,
        )
    }

    fn thread_exists(&self, thread_id: &str) -> StoreResult<bool> {
        self.exists("select 1 from threads where thread_id = ?1", thread_id)
    }

    fn thread_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
        let status: String = row.get(3)?;
        Ok(Thread {
            thread_id: row.get(0)?,
            model: row.get(1)?,
            cwd: row.get(2)?,
            status: serde_json::from_str(&status).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        })
    }

    fn turn_from_row(&self, row: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
        let status: String = row.get(2)?;
        Ok(Turn {
            turn_id: row.get(0)?,
            thread_id: row.get(1)?,
            status: serde_json::from_str(&status).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
            agent_loop_status: row.get(3)?,
        })
    }

    fn turn_exists(&self, turn_id: &str) -> StoreResult<bool> {
        self.exists("select 1 from turns where turn_id = ?1", turn_id)
    }

    fn exists(&self, query: &str, value: &str) -> StoreResult<bool> {
        let result = self.connection.query_row(query, params![value], |_| Ok(()));
        match result {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }

    fn init_schema(&self) -> StoreResult<()> {
        self.connection.execute_batch(
            "
            create table if not exists schema_migrations(
                migration_id text primary key,
                applied_at text not null default current_timestamp
            );
            create table if not exists threads(
                thread_id text primary key,
                model text,
                cwd text,
                status text not null
            );
            create table if not exists turns(
                turn_id text primary key,
                thread_id text not null,
                status text not null,
                agent_loop_status text not null
            );
            create table if not exists items(
                item_id text primary key,
                turn_id text not null,
                kind text not null,
                payload text not null,
                status text not null
            );
            create table if not exists trace_events(
                event_id text primary key,
                run_id text not null,
                session_id text not null default '',
                payload text not null
            );
            create table if not exists approvals(
                request_id text primary key,
                payload text not null,
                decision_outcome text,
                decision_reason text
            );
            create table if not exists approval_decisions(
                decision_id text primary key,
                request_id text not null,
                outcome text not null,
                reason text not null,
                payload text not null
            );
            create table if not exists artifact_refs(
                artifact_id text primary key,
                run_id text not null,
                item_id text,
                kind text not null,
                uri text not null,
                content_digest text not null,
                summary text not null,
                metadata text not null,
                redacted integer not null
            );
            ",
        )?;
        self.ensure_trace_session_id_column()?;
        self.connection.execute(
            "insert or ignore into schema_migrations(migration_id) values(?1)",
            params![INITIAL_SCHEMA_MIGRATION],
        )?;
        self.connection.execute(
            "insert or ignore into schema_migrations(migration_id) values(?1)",
            params![DURABLE_LEDGER_SCHEMA_MIGRATION],
        )?;
        Ok(())
    }

    fn ensure_trace_session_id_column(&self) -> StoreResult<()> {
        let mut statement = self.connection.prepare("pragma table_info(trace_events)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        for row in rows {
            if row? == "session_id" {
                return Ok(());
            }
        }
        self.connection.execute(
            "alter table trace_events add column session_id text not null default ''",
            [],
        )?;
        Ok(())
    }

    fn new_thread(model: Option<&str>, cwd: Option<&str>) -> Thread {
        Thread {
            thread_id: format!("thread_{}", short_id()),
            model: model.map(str::to_string),
            cwd: cwd.map(str::to_string),
            status: ThreadStatus::Active,
        }
    }

    fn new_turn(thread_id: &str, agent_loop_status: &str) -> Turn {
        Turn {
            turn_id: format!("turn_{}", short_id()),
            thread_id: thread_id.to_string(),
            status: TurnStatus::Running,
            agent_loop_status: agent_loop_status.to_string(),
        }
    }

    fn new_item(turn_id: &str, kind: ItemKind, payload: Value) -> Item {
        Item {
            item_id: format!("item_{}", short_id()),
            turn_id: turn_id.to_string(),
            kind,
            payload,
            status: ItemStatus::Completed,
        }
    }

    fn insert_thread(connection: &Connection, thread: &Thread) -> StoreResult<()> {
        connection.execute(
            "insert into threads(thread_id, model, cwd, status) values(?1, ?2, ?3, ?4)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                serde_json::to_string(&thread.status)?
            ],
        )?;
        Ok(())
    }

    fn insert_turn(connection: &Connection, turn: &Turn) -> StoreResult<()> {
        connection.execute(
            "insert into turns(turn_id, thread_id, status, agent_loop_status) values(?1, ?2, ?3, ?4)",
            params![
                turn.turn_id,
                turn.thread_id,
                serde_json::to_string(&turn.status)?,
                turn.agent_loop_status
            ],
        )?;
        Ok(())
    }

    fn insert_item(connection: &Connection, item: &Item) -> StoreResult<()> {
        connection.execute(
            "insert into items(item_id, turn_id, kind, payload, status) values(?1, ?2, ?3, ?4, ?5)",
            params![
                item.item_id,
                item.turn_id,
                serde_json::to_string(&item.kind)?,
                serde_json::to_string(&item.payload)?,
                serde_json::to_string(&item.status)?
            ],
        )?;
        Ok(())
    }

    fn insert_trace(connection: &Connection, event: &TraceEvent) -> StoreResult<()> {
        connection.execute(
            "insert into trace_events(event_id, run_id, session_id, payload) values(?1, ?2, ?3, ?4)",
            params![
                event.event_id,
                event.run_id,
                event.session_id,
                serde_json::to_string(event)?
            ],
        )?;
        Ok(())
    }

    fn exists_in_transaction(
        connection: &Connection,
        query: &str,
        value: &str,
    ) -> StoreResult<bool> {
        let result = connection.query_row(query, params![value], |_| Ok(()));
        match result {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(StoreError::Sqlite(error)),
        }
    }
}

fn insert_approval(connection: &Connection, request: &ApprovalRequest) -> StoreResult<()> {
    connection
        .execute(
            "insert into approvals(request_id, payload, decision_outcome, decision_reason) values(?1, ?2, null, null)",
            params![request.request_id, serde_json::to_string(request)?],
        )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(ref sqlite_error, _)
                    if sqlite_error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    StoreError::AlreadyExists(format!("approval {}", request.request_id))
                }
                other => StoreError::Sqlite(other),
            })?;
    Ok(())
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtifactRef> {
    let metadata: String = row.get(7)?;
    Ok(ArtifactRef {
        artifact_id: row.get(0)?,
        run_id: row.get(1)?,
        item_id: row.get(2)?,
        kind: row.get(3)?,
        uri: row.get(4)?,
        content_digest: row.get(5)?,
        summary: row.get(6)?,
        metadata: serde_json::from_str(&metadata).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        redacted: row.get(8)?,
    })
}

fn redact_secret_like_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_secret_like_text(&text)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(redact_secret_like_value).collect())
        }
        Value::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| {
                    if contains_secret_like(&key) {
                        (key, Value::String(REDACTED_ARTIFACT_VALUE.to_string()))
                    } else {
                        (key, redact_secret_like_value(value))
                    }
                })
                .collect(),
        ),
        other => other,
    }
}

fn artifact_needs_redaction(uri: &str, summary: &str, metadata: &Value) -> bool {
    contains_secret_like(uri)
        || contains_secret_like(summary)
        || value_contains_secret_like(metadata)
}

fn value_contains_secret_like(value: &Value) -> bool {
    match value {
        Value::String(text) => contains_secret_like(text),
        Value::Array(items) => items.iter().any(value_contains_secret_like),
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| contains_secret_like(key) || value_contains_secret_like(value)),
        _ => false,
    }
}

fn redact_secret_like_text(text: &str) -> String {
    if contains_secret_like(text) {
        REDACTED_ARTIFACT_VALUE.to_string()
    } else {
        text.to_string()
    }
}

fn contains_secret_like(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    SENSITIVE_ARTIFACT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}
