//! Trace and artifact persistence, redaction, integrity, and retrieval.

use super::support::*;
use super::*;

/// 注册脱敏、内容寻址产物引用的输入。
pub struct RegisterArtifactRefParams<'a> {
    /// artifact 所属 run。
    pub run_id: &'a str,
    /// 可选的来源 item。
    pub item_id: Option<&'a str>,
    /// artifact 类型。
    pub kind: &'a str,
    /// artifact URI。
    pub uri: &'a str,
    /// 内容摘要。
    pub content_digest: &'a str,
    /// 面向用户的摘要。
    pub summary: &'a str,
    /// 需要持久化并按规则脱敏的 metadata。
    pub metadata: Value,
}

impl SessionStore {
    /// 脱敏并追加一条带完整性校验的 trace event。
    pub fn append_trace(&self, event: &TraceEvent) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        validate_public_trace_binding(&transaction, event)?;
        let _ = Self::insert_trace(&transaction, event)?;
        transaction.commit()?;
        Ok(())
    }

    /// 读取 run 的全部 trace，并校验每条事件完整性。
    pub fn list_trace(&self, run_id: &str) -> StoreResult<Vec<TraceEvent>> {
        self.list_trace_page(run_id, None, None)
    }

    /// 按 rowid 游标分页读取 run trace。
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
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select event_id, run_id, session_id, payload
             from trace_events where run_id = ?1 order by rowid limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let raw_events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut events = Vec::new();
        for row in raw_events {
            let (event_id, stored_run_id, stored_session_id, payload) = row;
            let event =
                decode_stored_trace_row(&event_id, &stored_run_id, &stored_session_id, &payload)?;
            events.push(event);
        }
        validate_public_trace_bindings(&transaction, &events)?;
        if events.is_empty() {
            if Self::exists_in_transaction(
                &transaction,
                "select 1 from trace_events where run_id = ?1",
                run_id,
            )? {
                transaction.commit()?;
                return Ok(events);
            }
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        transaction.commit()?;
        Ok(events)
    }

    /// 读取 run trace 的有界最新窗口并恢复时间顺序。
    pub fn tail_trace(
        &self,
        run_id: &str,
        limit: usize,
        offset: Option<usize>,
    ) -> StoreResult<Vec<TraceEvent>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset.unwrap_or(0)).unwrap_or(i64::MAX);
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select event_id, run_id, session_id, payload
             from trace_events where run_id = ?1 order by rowid desc limit ?2 offset ?3",
        )?;
        let rows = statement.query_map(params![run_id, limit, offset], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let raw_events = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut events = Vec::new();
        for row in raw_events {
            let (event_id, stored_run_id, stored_session_id, payload) = row;
            let event =
                decode_stored_trace_row(&event_id, &stored_run_id, &stored_session_id, &payload)?;
            events.push(event);
        }
        validate_public_trace_bindings(&transaction, &events)?;
        if events.is_empty()
            && !Self::exists_in_transaction(
                &transaction,
                "select 1 from trace_events where run_id = ?1",
                run_id,
            )?
        {
            return Err(StoreError::NotFound(format!("trace run {run_id}")));
        }
        events.reverse();
        transaction.commit()?;
        Ok(events)
    }

    /// 读取单条 trace event 并校验完整性。
    pub fn show_trace(&self, event_id: &str) -> StoreResult<TraceEvent> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let (stored_event_id, stored_run_id, stored_session_id, payload): (
            String,
            String,
            String,
            String,
        ) = transaction
            .query_row(
                "select event_id, run_id, session_id, payload
                 from trace_events where event_id = ?1",
                params![event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("trace event {event_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let event = decode_stored_trace_row(
            &stored_event_id,
            &stored_run_id,
            &stored_session_id,
            &payload,
        )?;
        validate_public_trace_binding(&transaction, &event)?;
        transaction.commit()?;
        Ok(event)
    }
}

impl SessionStore {
    /// 脱敏并持久化一个 content-addressed artifact ref。
    pub fn register_artifact_ref(
        &self,
        params: RegisterArtifactRefParams<'_>,
    ) -> StoreResult<ArtifactRef> {
        let RegisterArtifactRefParams {
            run_id,
            item_id,
            kind,
            uri,
            content_digest,
            summary,
            metadata,
        } = params;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        validate_artifact_binding(&transaction, run_id, item_id)?;
        let content_digest =
            validate_artifact_fields(kind, uri, content_digest, summary, &metadata)?;
        let duplicate = transaction
            .query_row(
                "select artifact_id from artifact_refs
                 where run_id = ?1 and kind = ?2 and uri = ?3 and content_digest = ?4
                   and ((item_id = ?5) or (item_id is null and ?5 is null))
                 limit 1",
                params![run_id, kind, uri, content_digest, item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(artifact_id) = duplicate {
            return Err(StoreError::AlreadyExists(format!("artifact {artifact_id}")));
        }
        let artifact = ArtifactRef {
            artifact_id: format!("artifact_{}", short_id()),
            run_id: run_id.to_string(),
            item_id: item_id.map(str::to_string),
            kind: kind.to_string(),
            uri: uri.to_string(),
            content_digest,
            summary: redact_secret_like_text(summary),
            redacted: artifact_needs_redaction(uri, summary, &metadata),
            metadata: redact_secret_like_value(metadata),
        };
        transaction.execute(
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
        transaction.commit()?;
        Ok(artifact)
    }

    /// 读取指定 artifact ref。
    pub fn get_artifact_ref(&self, artifact_id: &str) -> StoreResult<ArtifactRef> {
        let artifact = self
            .connection
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
            })?;
        validate_stored_artifact(&self.connection, &artifact)?;
        Ok(artifact)
    }

    /// 列出 run 关联的 artifact refs。
    pub fn list_artifact_refs(&self, run_id: &str) -> StoreResult<Vec<ArtifactRef>> {
        validate_artifact_run(&self.connection, run_id)?;
        let mut statement = self.connection.prepare(
            "select artifact_id, run_id, item_id, kind, uri, content_digest, summary, metadata, redacted from artifact_refs where run_id = ?1 order by rowid",
        )?;
        let rows = statement.query_map(params![run_id], artifact_from_row)?;
        let artifacts = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut validated = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            validate_stored_artifact(&self.connection, &artifact)?;
            validated.push(artifact);
        }
        Ok(validated)
    }
}

pub(crate) fn validate_turn_trace_binding(
    event: &TraceEvent,
    thread_id: &str,
    turn_id: &str,
) -> StoreResult<()> {
    event.validate_turn_binding(thread_id, turn_id)?;
    Ok(())
}

// Public generic trace append may store external runs, but it cannot weaken a
// trace that identifies an existing thread or turn.
pub(crate) fn validate_public_trace_binding(
    connection: &Connection,
    event: &TraceEvent,
) -> StoreResult<()> {
    validate_public_trace_bindings(connection, std::slice::from_ref(event))
}

// Batch-prefetch the small set of thread/turn rows needed by a trace page.
// This keeps payload decoding row-local without issuing one binding query per event.
pub(crate) fn validate_public_trace_bindings(
    connection: &Connection,
    events: &[TraceEvent],
) -> StoreResult<()> {
    let thread_ids = events
        .iter()
        .map(|event| event.run_id.clone())
        .collect::<BTreeSet<_>>();
    let turn_ids = events
        .iter()
        .flat_map(|event| {
            event
                .task_id
                .iter()
                .chain(std::iter::once(&event.session_id))
                .cloned()
        })
        .collect::<BTreeSet<_>>();

    let existing_threads = select_trace_thread_ids(connection, &thread_ids)?;
    let turns = select_trace_turn_bindings(connection, &turn_ids)?;
    for event in events {
        let thread_exists = existing_threads.contains(&event.run_id);
        let turn_id = event.task_id.as_deref().unwrap_or(&event.session_id);
        match (thread_exists, turns.get(turn_id)) {
            (false, None) if event.task_id.is_none() => {}
            (false, None) => {
                return Err(StoreError::InvalidState(
                    "trace task_id must identify an existing turn".to_string(),
                ));
            }
            (true, None) if event.task_id.is_none() && event.session_id == event.run_id => {}
            (true, Some(thread_id)) | (false, Some(thread_id)) => {
                validate_turn_trace_binding(event, thread_id, turn_id)?;
            }
            (true, None) => {
                return Err(StoreError::InvalidState(
                    "trace for an existing thread must bind to the thread or an existing turn"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn select_trace_thread_ids(
    connection: &Connection,
    thread_ids: &BTreeSet<String>,
) -> StoreResult<BTreeSet<String>> {
    if thread_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let placeholders = std::iter::repeat_n("?", thread_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("select thread_id from threads where thread_id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(thread_ids.iter()), |row| row.get(0))?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(StoreError::Sqlite)
}

pub(crate) fn select_trace_turn_bindings(
    connection: &Connection,
    turn_ids: &BTreeSet<String>,
) -> StoreResult<BTreeMap<String, String>> {
    if turn_ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = std::iter::repeat_n("?", turn_ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("select turn_id, thread_id from turns where turn_id in ({placeholders})");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map(params_from_iter(turn_ids.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let (turn_id, thread_id) = row?;
        if bindings.insert(turn_id.clone(), thread_id).is_some() {
            return Err(StoreError::InvalidState(format!(
                "duplicate turn binding {turn_id}"
            )));
        }
    }
    Ok(bindings)
}
