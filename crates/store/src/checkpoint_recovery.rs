//! Checkpoint handoff, execution recovery, and workspace recovery.

use super::*;
use std::collections::BTreeSet;

/// Durable execution ownership state. `Unknown` is intentionally terminal for that execution:
/// without a tool-specific reconciliation contract it must never be replayed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionState {
    Running,
    Unknown,
}

impl ToolExecutionState {
    pub const fn as_storage_text(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_storage_text(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// Opaque persisted metadata for one tool execution. Raw tool output is never stored here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolExecution {
    pub execution_id: String,
    pub thread_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub state: ToolExecutionState,
    pub payload: Value,
}

/// 无 active owner turn 的终态化参数：previous 状态、终态 agent_loop_status、
/// recovery reason 与基础 trace（避免 helper 参数过多）。
#[derive(Debug, Clone)]
pub(crate) struct OwnerlessTerminalization<'a> {
    pub(crate) previous_status: TurnStatus,
    pub(crate) previous_agent_loop_status: &'a str,
    pub(crate) terminal_agent_loop_status: &'a str,
    pub(crate) recovery_reason: &'a str,
    pub(crate) trace: &'a TraceEvent,
}

impl SessionStore {
    /// Persist a safe turn boundary before entering a model/tool side effect.
    pub fn save_turn_checkpoint(
        &self,
        turn_id: &str,
        thread_id: &str,
        checkpoint: &Value,
        checkpoint_version: u32,
    ) -> StoreResult<()> {
        if turn_id.trim().is_empty() || thread_id.trim().is_empty() || !checkpoint.is_object() {
            return Err(StoreError::InvalidState(
                "turn checkpoint identity or payload is invalid".to_string(),
            ));
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let bound_thread: String = transaction
            .query_row(
                "select thread_id from turns where turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        if bound_thread != thread_id {
            return Err(StoreError::InvalidState(
                "turn checkpoint thread binding mismatch".to_string(),
            ));
        }
        Self::upsert_turn_checkpoint(
            &transaction,
            turn_id,
            thread_id,
            checkpoint,
            checkpoint_version,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn upsert_turn_checkpoint(
        transaction: &Connection,
        turn_id: &str,
        thread_id: &str,
        checkpoint: &Value,
        checkpoint_version: u32,
    ) -> StoreResult<()> {
        let payload = serde_json::to_string(checkpoint)?;
        transaction.execute(
            "insert into turn_checkpoints(turn_id, thread_id, payload, checkpoint_version) values(?1, ?2, ?3, ?4)
             on conflict(turn_id) do update set thread_id=excluded.thread_id, payload=excluded.payload, checkpoint_version=excluded.checkpoint_version, created_at=current_timestamp",
            params![turn_id, thread_id, payload, checkpoint_version],
        )?;
        Ok(())
    }

    /// Load the last safe boundary. Payload decoding/typed validation belongs to AgentLoop.
    pub fn get_turn_checkpoint(&self, turn_id: &str) -> StoreResult<Option<Value>> {
        self.connection
            .query_row(
                "select payload from turn_checkpoints where turn_id = ?1",
                params![turn_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload))
            .transpose()
            .map_err(StoreError::from)
    }

    /// Atomically fail a turn whose opaque checkpoint cannot be decoded.
    ///
    /// The expected turn owner/status forms the CAS boundary. Running tool
    /// executions are retained as `Unknown`; no external operation is started
    /// or replayed. The durable turn checkpoint remains available for audit and
    /// a typed failure trace is appended.
    pub fn terminalize_checkpoint_failure(
        &self,
        thread_id: &str,
        turn_id: &str,
        expected_status: TurnStatus,
        expected_agent_loop_status: &str,
    ) -> StoreResult<Turn> {
        if thread_id.trim().is_empty()
            || turn_id.trim().is_empty()
            || expected_agent_loop_status.trim().is_empty()
        {
            return Err(StoreError::InvalidState(
                "checkpoint failure terminalization identity is invalid".to_string(),
            ));
        }
        if matches!(
            expected_status,
            TurnStatus::Completed | TurnStatus::Interrupted
        ) {
            return Err(StoreError::InvalidState(
                "checkpoint failure cannot overwrite a terminal turn".to_string(),
            ));
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let current = self.turn_in_transaction(&transaction, turn_id)?;
        if current.thread_id != thread_id {
            return Err(StoreError::InvalidState(
                "checkpoint failure turn/thread binding mismatch".to_string(),
            ));
        }
        if current.status != expected_status
            || current.agent_loop_status != expected_agent_loop_status
        {
            return Err(StoreError::InvalidState(
                "checkpoint failure owner/status changed before terminalization".to_string(),
            ));
        }

        // A concurrent owner may have already committed the same failed
        // terminal state. Treat that exact state as an idempotent success, but
        // never overwrite another terminal outcome.
        if current.status == TurnStatus::Failed && current.agent_loop_status == "failed" {
            transaction.commit()?;
            return Ok(current);
        }

        let running_execution_count: i64 = transaction.query_row(
            "select count(*) from tool_executions
             where thread_id = ?1 and turn_id = ?2 and execution_state = 'running'",
            params![thread_id, turn_id],
            |row| row.get(0),
        )?;
        let mismatched_execution_binding: bool = transaction.query_row(
            "select exists(select 1 from tool_executions
             where turn_id = ?1 and thread_id <> ?2)",
            params![turn_id, thread_id],
            |row| row.get(0),
        )?;
        if mismatched_execution_binding {
            return Err(StoreError::InvalidState(
                "checkpoint failure execution thread binding mismatch".to_string(),
            ));
        }
        let marked_unknown = transaction.execute(
            "update tool_executions set execution_state = 'unknown'
             where thread_id = ?1 and turn_id = ?2 and execution_state = 'running'",
            params![thread_id, turn_id],
        )? as i64;
        if marked_unknown != running_execution_count {
            return Err(StoreError::InvalidState(
                "checkpoint failure changed an unexpected tool execution count".to_string(),
            ));
        }

        let mut trace = TraceEvent::for_turn(
            format!(
                "trace_{turn_id}_checkpoint_decode_failed_{}",
                Uuid::new_v4()
            ),
            thread_id,
            turn_id,
            "app_server",
            "turn failed because its checkpoint could not be decoded",
        );
        trace.payload = serde_json::json!({
            "failure_kind": "checkpoint_decode_failed",
            "recovery_reason": "checkpoint_decode_failed",
            "previous_status": current.status.clone(),
            "previous_agent_loop_status": current.agent_loop_status.clone(),
            "running_executions_marked_unknown": marked_unknown,
            "tool_replayed": false,
        });
        let trace = if find_trace_span_start(&transaction, thread_id, turn_id, TraceSpanKind::Turn)?
            .is_some()
        {
            typed_turn_end_trace(&transaction, &trace, &current, &TurnStatus::Failed)?
        } else {
            trace
        };

        let changed = transaction.execute(
            "update turns set status = ?1, agent_loop_status = ?2
             where turn_id = ?3 and thread_id = ?4 and status = ?5 and agent_loop_status = ?6",
            params![
                TurnStatus::Failed.to_db_text(),
                "failed",
                turn_id,
                thread_id,
                expected_status.to_db_text(),
                expected_agent_loop_status,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(
                "checkpoint failure owner/status changed before terminal commit".to_string(),
            ));
        }
        Self::insert_turn_trace(&transaction, &trace, thread_id, turn_id)?;
        transaction.commit()?;
        Ok(Turn {
            status: TurnStatus::Failed,
            agent_loop_status: "failed".to_string(),
            ..current
        })
    }

    /// Publish the pending-action checkpoint and claim every tool execution unless a steer or
    /// pause was already accepted at the same SQLite linearization point.
    pub fn begin_tool_executions_at_checkpoint(
        &self,
        executions: &[ToolExecution],
        checkpoint: &Value,
        checkpoint_version: u32,
    ) -> StoreResult<bool> {
        let Some(first) = executions.first() else {
            return Err(StoreError::InvalidState(
                "tool execution batch is empty".to_string(),
            ));
        };
        if executions.iter().any(|execution| {
            execution.turn_id != first.turn_id
                || execution.thread_id != first.thread_id
                || execution.state != ToolExecutionState::Running
                || execution.execution_id.trim().is_empty()
                || execution.tool_call_id.trim().is_empty()
                || !execution.payload.is_object()
        }) {
            return Err(StoreError::InvalidState(
                "tool execution batch binding is invalid".to_string(),
            ));
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let turn = self.turn_in_transaction(&transaction, &first.turn_id)?;
        if turn.thread_id != first.thread_id {
            return Err(StoreError::InvalidState(
                "tool execution turn thread binding mismatch".to_string(),
            ));
        }
        match turn.status {
            TurnStatus::Running => {}
            _ => {
                return Err(StoreError::InvalidState(
                    "tool execution batch requires a running turn".to_string(),
                ));
            }
        }
        let boundary_pending: bool = transaction.query_row(
            "select pause_requested = 1 or exists(
                select 1 from turn_inputs
                where turn_id = ?1 and delivery_state = 'pending' and delivery = 'steer'
             ) from turns where turn_id = ?1 and thread_id = ?2",
            params![first.turn_id, first.thread_id],
            |row| row.get(0),
        )?;
        Self::upsert_turn_checkpoint(
            &transaction,
            &first.turn_id,
            &first.thread_id,
            checkpoint,
            checkpoint_version,
        )?;
        if boundary_pending {
            transaction.commit()?;
            return Ok(false);
        }
        for execution in executions {
            transaction.execute(
                "insert into tool_executions(
                    execution_id, thread_id, turn_id, tool_call_id, execution_state, payload
                 ) values(?1, ?2, ?3, ?4, 'running', ?5)",
                params![
                    execution.execution_id,
                    execution.thread_id,
                    execution.turn_id,
                    execution.tool_call_id,
                    serde_json::to_string(&execution.payload)?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(true)
    }

    /// Read active execution ownership without exposing raw tool arguments.
    pub fn get_tool_execution(&self, execution_id: &str) -> StoreResult<Option<ToolExecution>> {
        self.connection
            .query_row(
                "select execution_id, thread_id, turn_id, tool_call_id, execution_state, payload from tool_executions where execution_id = ?1",
                params![execution_id],
                |row| {
                    let state: String = row.get(4)?;
                    let state = ToolExecutionState::from_storage_text(&state).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "unknown tool execution state")),
                        )
                    })?;
                    let payload: String = row.get(5)?;
                    let payload = serde_json::from_str(&payload).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(ToolExecution {
                        execution_id: row.get(0)?,
                        thread_id: row.get(1)?,
                        turn_id: row.get(2)?,
                        tool_call_id: row.get(3)?,
                        state,
                        payload,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Mark a possibly in-flight execution as Unknown; callers must not retry it.
    pub fn mark_tool_execution_unknown(&self, execution_id: &str) -> StoreResult<()> {
        let changed = self.connection.execute(
            "update tool_executions set execution_state = 'unknown' where execution_id = ?1 and execution_state = 'running'",
            params![execution_id],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidState(format!(
                "tool execution {execution_id} is not running"
            )));
        }
        Ok(())
    }

    /// Publish a complete ToolResult checkpoint only for the exact full running execution batch.
    pub fn commit_tool_results_checkpoint(
        &self,
        execution_ids: &[String],
        turn_id: &str,
        thread_id: &str,
        checkpoint: &Value,
        checkpoint_version: u32,
    ) -> StoreResult<()> {
        let unique_ids = execution_ids.iter().cloned().collect::<BTreeSet<_>>();
        if execution_ids.is_empty()
            || unique_ids.len() != execution_ids.len()
            || execution_ids.iter().any(|id| id.trim().is_empty())
            || !checkpoint.is_object()
        {
            return Err(StoreError::InvalidState(
                "tool result checkpoint batch is invalid".to_string(),
            ));
        }
        let payload = serde_json::to_string(checkpoint)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "select execution_id from tool_executions
             where turn_id = ?1 and thread_id = ?2 and execution_state = 'running'",
        )?;
        let running_ids = statement
            .query_map(params![turn_id, thread_id], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        drop(statement);
        if running_ids != unique_ids {
            return Err(StoreError::InvalidState(
                "tool result checkpoint must commit the complete running batch for the turn"
                    .to_string(),
            ));
        }
        transaction.execute(
            "insert into turn_checkpoints(turn_id, thread_id, payload, checkpoint_version) values(?1, ?2, ?3, ?4)
             on conflict(turn_id) do update set thread_id=excluded.thread_id, payload=excluded.payload, checkpoint_version=excluded.checkpoint_version, created_at=current_timestamp",
            params![turn_id, thread_id, payload, checkpoint_version],
        )?;
        let mut deleted = 0;
        for execution_id in execution_ids {
            deleted += transaction.execute(
                "delete from tool_executions where execution_id = ?1 and execution_state = 'running'",
                params![execution_id],
            )?;
        }
        if deleted != execution_ids.len() {
            return Err(StoreError::InvalidState(
                "tool execution batch changed before checkpoint commit".to_string(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Claim a paused or owner-loss-suspended turn exactly once for explicit resume.
    pub fn claim_suspended_turn(&self, turn_id: &str) -> StoreResult<(Turn, Value)> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let (thread_id, status, agent_loop_status): (String, String, String) = transaction
            .query_row(
                "select thread_id, status, agent_loop_status from turns where turn_id = ?1",
                params![turn_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("turn {turn_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let status = TurnStatus::from_storage_text(&status)
            .ok_or_else(|| StoreError::InvalidState("turn has unknown status".to_string()))?;
        let resumable = matches!(status, TurnStatus::Paused | TurnStatus::Suspended)
            && agent_loop_status == status.as_storage_text();
        if !resumable {
            return Err(StoreError::InvalidState(
                "turn is not paused or suspended and cannot be resumed".to_string(),
            ));
        }
        let unknown: i64 = transaction.query_row(
            "select count(*) from tool_executions where turn_id = ?1 and execution_state = 'unknown'",
            params![turn_id],
            |row| row.get(0),
        )?;
        if unknown != 0 {
            return Err(StoreError::InvalidState(
                "turn has unknown tool execution and cannot be resumed".to_string(),
            ));
        }
        let payload: String = transaction
            .query_row(
                "select payload from turn_checkpoints where turn_id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::InvalidState("suspended turn checkpoint is missing".to_string())
                }
                other => StoreError::Sqlite(other),
            })?;
        let changed = transaction.execute(
            "update turns set status = 'running', agent_loop_status = 'running',
                              pause_requested = 0
             where turn_id = ?1 and status = ?2 and agent_loop_status = ?2",
            params![turn_id, status.as_storage_text()],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(
                "resumable turn was claimed by another resume".to_string(),
            ));
        }
        let turn = Turn {
            turn_id: turn_id.to_string(),
            thread_id,
            status: TurnStatus::Running,
            agent_loop_status: "running".to_string(),
        };
        let checkpoint = serde_json::from_str(&payload)?;
        transaction.commit()?;
        Ok((turn, checkpoint))
    }

    /// Release a claimed turn after its owner fails, preserving unknown side effects.
    pub fn suspend_claimed_turn_after_failure(&self, turn_id: &str) -> StoreResult<Turn> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let turn = self.turn_in_transaction(&transaction, turn_id)?;
        if turn.status != TurnStatus::Running || turn.agent_loop_status != "running" {
            return Ok(turn);
        }
        let checkpoint_exists: bool = transaction.query_row(
            "select exists(select 1 from turn_checkpoints where turn_id = ?1)",
            params![turn_id],
            |row| row.get(0),
        )?;
        if !checkpoint_exists {
            return Err(StoreError::InvalidState(
                "claimed turn failure has no durable checkpoint".to_string(),
            ));
        }
        transaction.execute(
            "update tool_executions set execution_state = 'unknown'
             where turn_id = ?1 and execution_state = 'running'",
            params![turn_id],
        )?;
        let changed = transaction.execute(
            "update turns set status = 'suspended', agent_loop_status = 'suspended'
             where turn_id = ?1 and status = 'running' and agent_loop_status = 'running'",
            params![turn_id],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidState(
                "claimed turn changed before failure suspension".to_string(),
            ));
        }
        transaction.commit()?;
        Ok(Turn {
            status: TurnStatus::Suspended,
            agent_loop_status: "suspended".to_string(),
            ..turn
        })
    }
}

impl SessionStore {
    /// 对执行保护覆盖的每个线程应用所有权丢失恢复。
    pub(crate) fn recover_abandoned_workspace_execution(
        &self,
        guard: &WorkspaceExecutionGuard,
    ) -> StoreResult<()> {
        self.validate_workspace_execution_guard(guard)?;
        for thread_id in self.workspace_execution_thread_ids(&guard.execution_scope)? {
            self.recover_abandoned_thread_execution(&thread_id)?;
        }
        Ok(())
    }

    // 查询执行锁覆盖的 thread 集合。
    pub(crate) fn workspace_execution_thread_ids(
        &self,
        execution_scope: &WorkspaceExecutionScope,
    ) -> StoreResult<Vec<String>> {
        match execution_scope {
            WorkspaceExecutionScope::Workspace(workspace) => {
                let mut statement = self
                    .connection
                    .prepare("select thread_id from threads where cwd = ?1 order by rowid")?;
                let thread_ids = statement
                    .query_map(params![workspace], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(thread_ids)
            }
            WorkspaceExecutionScope::Thread(thread_id) => Ok(vec![thread_id.clone()]),
        }
    }

    // 恢复单个 thread 的 abandoned execution。
    pub(crate) fn recover_abandoned_thread_execution(&self, thread_id: &str) -> StoreResult<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        Self::recover_tool_executions_for_thread(&transaction, thread_id)?;
        Self::recover_abandoned_turns_for_thread(&transaction, thread_id)?;
        transaction.commit()?;
        Ok(())
    }

    /// Convert every owner-visible running execution to Unknown and suspend only turns that still
    /// have a validated safe checkpoint. Unknown executions remain durable to block auto-retry.
    fn recover_tool_executions_for_thread(
        transaction: &Connection,
        thread_id: &str,
    ) -> StoreResult<Vec<String>> {
        let mut statement = transaction.prepare(
            "select execution_id, turn_id from tool_executions
             where thread_id = ?1 and execution_state = 'running' order by rowid",
        )?;
        let executions = statement
            .query_map(params![thread_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut recovered = Vec::new();
        for (execution_id, turn_id) in executions {
            transaction.execute(
                "update tool_executions set execution_state = 'unknown' where execution_id = ?1 and execution_state = 'running'",
                params![&execution_id],
            )?;
            let has_checkpoint: bool = transaction.query_row(
                "select exists(select 1 from turn_checkpoints where turn_id = ?1)",
                params![&turn_id],
                |row| row.get(0),
            )?;
            if has_checkpoint {
                transaction.execute(
                    "update turns set status = 'suspended', agent_loop_status = 'suspended'
                     where turn_id = ?1 and status not in ('completed', 'failed', 'interrupted')",
                    params![&turn_id],
                )?;
                let trace = TraceEvent {
                    task_id: Some(turn_id.clone()),
                    payload: serde_json::json!({
                        "turn_id": &turn_id,
                        "execution_id": &execution_id,
                        "recovery_reason": "execution_owner_lost",
                        "execution_state": "unknown",
                        "tool_replayed": false,
                    }),
                    ..TraceEvent::for_turn(
                        format!("trace_{turn_id}_suspended_{}", Uuid::new_v4()),
                        thread_id,
                        turn_id.clone(),
                        "app_server",
                        "turn suspended after execution owner was lost",
                    )
                };
                Self::insert_turn_trace(transaction, &trace, thread_id, &turn_id)?;
                recovered.push(turn_id);
            }
        }
        Ok(recovered)
    }

    /// 将无 active owner 的非终态 turn 终态化为 Interrupted。
    ///
    /// B1（用户 interrupt）与 B2（启动恢复）共用：同一事务内更新状态、写
    /// typed trace；保留 checkpoint 与 tool_executions 审计证据；不执行/不重放/
    /// 不删除 unknown。trace 插入失败会使事务回滚。
    pub(crate) fn terminalize_ownerless_turn(
        transaction: &Connection,
        thread_id: &str,
        turn_id: &str,
        params: OwnerlessTerminalization<'_>,
    ) -> StoreResult<()> {
        transaction.execute(
            "update turns set status = ?1, agent_loop_status = ?2 where turn_id = ?3",
            params![
                TurnStatus::Interrupted.to_db_text(),
                params.terminal_agent_loop_status,
                turn_id
            ],
        )?;
        let mut terminal_trace = params.trace.clone();
        terminal_trace.summary = match params.recovery_reason {
            "execution_owner_lost" => "turn interrupted after execution owner was lost".to_string(),
            _ => "turn interrupted after inconsistent state was detected".to_string(),
        };
        terminal_trace.payload = serde_json::json!({
            "turn_id": turn_id,
            "previous_status": params.previous_status,
            "previous_agent_loop_status": params.previous_agent_loop_status,
            "recovery_reason": params.recovery_reason,
            "tool_replayed": false,
        });
        Self::insert_turn_trace(transaction, &terminal_trace, thread_id, turn_id)?;
        Ok(())
    }

    // 将 thread 中遗留的非终态 turn 收敛为可恢复状态。
    pub(crate) fn recover_abandoned_turns_for_thread(
        transaction: &Connection,
        thread_id: &str,
    ) -> StoreResult<Vec<String>> {
        let mut statement = transaction.prepare(
            "select turn_id, status, agent_loop_status from turns
             where thread_id = ?1 and status not in (?2, ?3, ?4)
             order by turn_sequence",
        )?;
        let turns = statement
            .query_map(
                params![
                    thread_id,
                    TurnStatus::Completed.to_db_text(),
                    TurnStatus::Failed.to_db_text(),
                    TurnStatus::Interrupted.to_db_text(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut recovered = Vec::new();
        for (turn_id, serialized_status, agent_loop_status) in turns {
            let status = TurnStatus::from_db_text(&serialized_status)
                .ok_or_else(|| unknown_db_enum("turn status", &serialized_status))?;
            let unknown_count: i64 = transaction.query_row(
                "select count(*) from tool_executions
                 where turn_id = ?1 and execution_state = 'unknown'",
                params![&turn_id],
                |row| row.get(0),
            )?;
            let has_checkpoint: bool = transaction.query_row(
                "select exists(select 1 from turn_checkpoints where turn_id = ?1)",
                params![&turn_id],
                |row| row.get(0),
            )?;
            if status == TurnStatus::Suspended && agent_loop_status == "suspended" {
                // 可归属不一致：缺 checkpoint。正常 suspended（有 checkpoint）保持可恢复。
                if !has_checkpoint {
                    let trace = TraceEvent::for_turn(
                        format!("trace_{turn_id}_inconsistent_{}", Uuid::new_v4()),
                        thread_id,
                        turn_id.clone(),
                        "app_server",
                        "turn interrupted after inconsistent state was detected",
                    );
                    Self::terminalize_ownerless_turn(
                        transaction,
                        thread_id,
                        &turn_id,
                        OwnerlessTerminalization {
                            previous_status: status,
                            previous_agent_loop_status: &agent_loop_status,
                            terminal_agent_loop_status: "interrupted",
                            recovery_reason: "inconsistent_turn_state",
                            trace: &trace,
                        },
                    )?;
                    recovered.push(turn_id);
                }
                continue;
            }
            if status == TurnStatus::Paused && agent_loop_status == "paused" {
                if !has_checkpoint {
                    let trace = TraceEvent::for_turn(
                        format!("trace_{turn_id}_inconsistent_{}", Uuid::new_v4()),
                        thread_id,
                        turn_id.clone(),
                        "app_server",
                        "turn interrupted after inconsistent state was detected",
                    );
                    Self::terminalize_ownerless_turn(
                        transaction,
                        thread_id,
                        &turn_id,
                        OwnerlessTerminalization {
                            previous_status: status,
                            previous_agent_loop_status: &agent_loop_status,
                            terminal_agent_loop_status: "interrupted",
                            recovery_reason: "inconsistent_turn_state",
                            trace: &trace,
                        },
                    )?;
                    recovered.push(turn_id);
                }
                continue;
            }
            if has_checkpoint && status == TurnStatus::Running && unknown_count == 0 {
                transaction.execute(
                    "update turns set status = 'suspended', agent_loop_status = 'suspended' where turn_id = ?1",
                    params![&turn_id],
                )?;
                continue;
            }

            transaction.execute(
                "update turns set status = ?1, agent_loop_status = 'interrupted'
                 where turn_id = ?2",
                params![TurnStatus::Interrupted.to_db_text(), &turn_id],
            )?;
            let trace = TraceEvent {
                task_id: Some(turn_id.clone()),
                payload: serde_json::json!({
                    "turn_id": &turn_id,
                    "previous_status": status,
                    "previous_agent_loop_status": agent_loop_status,
                    "recovery_reason": "execution_owner_lost",
                    "tool_replayed": false,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{turn_id}_owner_lost_{}", Uuid::new_v4()),
                    thread_id,
                    turn_id.clone(),
                    "app_server",
                    "turn interrupted after execution owner was lost",
                )
            };
            Self::insert_turn_trace(transaction, &trace, thread_id, &turn_id)?;
            recovered.push(turn_id);
        }
        Ok(recovered)
    }

    // 确认 guard 仍属于当前 store 和正确的执行范围。
    pub(crate) fn validate_workspace_execution_guard(
        &self,
        guard: &WorkspaceExecutionGuard,
    ) -> StoreResult<()> {
        if self.runtime_path.as_ref() != Some(&guard.store_path) {
            return Err(StoreError::InvalidState(
                "workspace execution guard belongs to another store".to_string(),
            ));
        }
        Ok(())
    }
}
