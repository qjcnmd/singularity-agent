#![forbid(unsafe_code)]

use std::path::Path;

use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_policy::{ApprovalOutcome, ApprovalRequest};
use singularity_protocol::{
    Item, ItemKind, ItemStatus, Thread, ThreadStatus, TraceEvent, Turn, TurnStatus,
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("record not found: {0}")]
    NotFound(String),
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
                schema_version: 1,
            },
        };
        store.init_schema()?;
        Ok(store)
    }

    pub fn descriptor(&self) -> &SessionStoreDescriptor {
        &self.descriptor
    }

    pub fn create_thread(&self, model: Option<&str>, cwd: Option<&str>) -> StoreResult<Thread> {
        let thread = Thread {
            thread_id: format!("thread_{}", short_id()),
            model: model.map(str::to_string),
            cwd: cwd.map(str::to_string),
            status: ThreadStatus::Active,
        };
        self.connection.execute(
            "insert into threads(thread_id, model, cwd, status) values(?1, ?2, ?3, ?4)",
            params![
                thread.thread_id,
                thread.model,
                thread.cwd,
                serde_json::to_string(&thread.status)?
            ],
        )?;
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
        let changed = self.connection.execute(
            "delete from threads where thread_id = ?1",
            params![thread_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        self.connection.execute(
            "delete from trace_events where run_id = ?1 or payload like ?2",
            params![thread_id, format!("%{thread_id}%")],
        )?;
        Ok(())
    }

    pub fn create_turn(&self, thread_id: &str, agent_loop_status: &str) -> StoreResult<Turn> {
        if !self.thread_exists(thread_id)? {
            return Err(StoreError::NotFound(format!("thread {thread_id}")));
        }
        let turn = Turn {
            turn_id: format!("turn_{}", short_id()),
            thread_id: thread_id.to_string(),
            status: TurnStatus::Running,
            agent_loop_status: agent_loop_status.to_string(),
        };
        self.connection.execute(
            "insert into turns(turn_id, thread_id, status, agent_loop_status) values(?1, ?2, ?3, ?4)",
            params![
                turn.turn_id,
                turn.thread_id,
                serde_json::to_string(&turn.status)?,
                turn.agent_loop_status
            ],
        )?;
        Ok(turn)
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
        let item = Item {
            item_id: format!("item_{}", short_id()),
            turn_id: turn_id.to_string(),
            kind,
            payload,
            status: ItemStatus::Completed,
        };
        self.connection.execute(
            "insert into items(item_id, turn_id, kind, payload, status) values(?1, ?2, ?3, ?4, ?5)",
            params![
                item.item_id,
                item.turn_id,
                serde_json::to_string(&item.kind)?,
                serde_json::to_string(&item.payload)?,
                serde_json::to_string(&item.status)?
            ],
        )?;
        Ok(item)
    }

    pub fn append_trace(&self, event: &TraceEvent) -> StoreResult<()> {
        self.connection.execute(
            "insert into trace_events(event_id, run_id, payload) values(?1, ?2, ?3)",
            params![event.event_id, event.run_id, serde_json::to_string(event)?],
        )?;
        Ok(())
    }

    pub fn list_trace(&self, run_id: &str) -> StoreResult<Vec<TraceEvent>> {
        let mut statement = self
            .connection
            .prepare("select payload from trace_events where run_id = ?1 order by rowid")?;
        let rows = statement.query_map(params![run_id], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        if events.is_empty() {
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        Ok(events)
    }

    pub fn tail_trace(&self, run_id: &str, limit: usize) -> StoreResult<Vec<TraceEvent>> {
        let mut events = self.list_trace(run_id)?;
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
        self.connection.execute(
            "insert into approvals(request_id, payload, decision_outcome, decision_reason) values(?1, ?2, null, null)",
            params![request.request_id, serde_json::to_string(request)?],
        )?;
        Ok(())
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

    pub fn record_approval_decision(
        &self,
        request_id: &str,
        outcome: ApprovalOutcome,
        reason: &str,
    ) -> StoreResult<()> {
        let changed = self.connection.execute(
            "update approvals set decision_outcome = ?1, decision_reason = ?2 where request_id = ?3 and decision_outcome is null",
            params![serde_json::to_string(&outcome)?, reason, request_id],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(format!("approval {request_id}")));
        }
        Ok(())
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
                payload text not null
            );
            create table if not exists approvals(
                request_id text primary key,
                payload text not null,
                decision_outcome text,
                decision_reason text
            );
            ",
        )?;
        Ok(())
    }
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}
