//! Checkpoint handoff, approval execution recovery, and workspace recovery.

use super::support::*;
use super::*;
use crate::approval::typed_approval_wait_start_trace;

impl SessionStore {
    /// 原子地用 turn 结果和后续检查点（如有）解决执行中的 approval。
    pub fn commit_turn_outcome_and_resolve_pending_execution(
        &self,
        request_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        next_approvals: &[(ApprovalRequest, Value)],
    ) -> StoreResult<CommittedTurnOutcome> {
        self.commit_turn_outcome_and_resolve_pending_execution_with_authority(
            request_id,
            params,
            next_approvals,
            TurnOutcomeAuthority::AgentLoop,
        )
    }

    /// 在 typed 基础设施故障权限下提交 executing approval 的终态并清理 checkpoint。
    pub fn commit_turn_outcome_and_resolve_pending_execution_with_authority(
        &self,
        request_id: &str,
        params: CommitTurnOutcomeParams<'_>,
        next_approvals: &[(ApprovalRequest, Value)],
        authority: TurnOutcomeAuthority,
    ) -> StoreResult<CommittedTurnOutcome> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if !next_approvals.is_empty()
            && (params.status != TurnStatus::Blocked || params.agent_loop_status != "blocked")
        {
            return Err(StoreError::InvalidState(
                "next approval handoff requires a blocked turn outcome".to_string(),
            ));
        }
        let bound_turn_id: String = transaction
            .query_row(
                "select turn_id from pending_tool_calls where request_id = ?1 and execution_state = 'executing'",
                params![request_id],
                |row| row.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidState(format!(
                    "pending execution {request_id} is not in executing state"
                )),
                other => StoreError::Sqlite(other),
            })?;
        // 先提交当前 executing approval 的 turn 结果。
        let committed = self.commit_turn_outcome_in_transaction(
            &transaction,
            &bound_turn_id,
            params,
            authority,
        )?;
        // 原子移除已解决 checkpoint，再写入 successor approvals。
        let deleted = transaction.execute(
            "delete from pending_tool_calls where request_id = ?1 and execution_state = 'executing'",
            params![request_id],
        )?;
        if deleted != 1 {
            return Err(StoreError::InvalidState(format!(
                "pending execution {request_id} was not resolved"
            )));
        }
        for (request, checkpoint) in next_approvals {
            if request.turn_id != bound_turn_id {
                return Err(StoreError::InvalidState(
                    "next approval turn binding mismatch".to_string(),
                ));
            }
            insert_approval(&transaction, request)?;
            let tool_call_id = request.tool_call_id.as_deref().ok_or_else(|| {
                StoreError::InvalidState(
                    "pending approval checkpoint requires an explicit tool_call_id".to_string(),
                )
            })?;
            ensure_request_turn_binding(&transaction, request)?;
            transaction.execute(
                "insert into pending_tool_calls(request_id, thread_id, turn_id, tool_call_id, payload, execution_state) values(?1, ?2, ?3, ?4, ?5, 'pending')",
                params![
                    request.request_id,
                    request.thread_id,
                    request.turn_id,
                    tool_call_id,
                    serde_json::to_string(checkpoint)?
                ],
            )?;
            let approval_trace = typed_approval_wait_start_trace(
                &transaction,
                request,
                "approval",
                "approval requested",
            )?;
            Self::insert_turn_trace(
                &transaction,
                &approval_trace,
                &request.thread_id,
                &request.turn_id,
            )?;
        }
        transaction.commit()?;
        Ok(committed)
    }

    /// 对执行中的 approval 进行协调，而不重放其未知的外部副作用。
    pub(crate) fn recover_incomplete_approval_executions_for_thread(
        transaction: &Connection,
        thread_id: &str,
    ) -> StoreResult<Vec<String>> {
        // approval 与 pending execution 的联合恢复行。
        struct RecoveryRow {
            approval_rowid: i64,
            request: ApprovalRequest,
            decision: Option<ApprovalOutcome>,
            pending_rowid: Option<i64>,
            pending_state: Option<String>,
            turn_status: TurnStatus,
            agent_loop_status: String,
        }

        let mut statement = transaction.prepare(
            "select a.rowid, a.request_id, a.thread_id, a.turn_id, a.payload,
                    a.decision_outcome, a.decision_reason,
                    p.rowid, p.thread_id, p.turn_id, p.tool_call_id, p.payload,
                    p.execution_state
             from approvals a
             left join pending_tool_calls p on p.request_id = a.request_id
             where a.thread_id = ?1
             order by a.rowid",
        )?;
        // 先读取 approval、decision 与 pending execution 的联合快照。
        let persisted_rows = statement
            .query_map(params![thread_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        // 再将快照解码为可逐行校验和修复的恢复记录。
        let mut rows = Vec::with_capacity(persisted_rows.len());
        for (
            approval_rowid,
            request_id,
            stored_thread_id,
            stored_turn_id,
            request_payload,
            decision,
            decision_reason,
            pending_rowid,
            pending_thread_id,
            pending_turn_id,
            pending_tool_call_id_value,
            pending_payload,
            pending_state,
        ) in persisted_rows
        {
            let request = decode_stored_approval_request_row(
                transaction,
                &request_id,
                &stored_thread_id,
                &stored_turn_id,
                &request_payload,
                decision.as_deref(),
                decision_reason.as_deref(),
            )?;
            if request.thread_id != thread_id {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} recovery thread filter mismatch"
                )));
            }
            let (turn_status, agent_loop_status) = transaction
                .query_row(
                    "select status, agent_loop_status from turns where turn_id = ?1 and thread_id = ?2",
                    params![request.turn_id, request.thread_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::InvalidState(format!(
                        "approval {request_id} is not bound to an existing turn"
                    ))
                })?;
            let status = TurnStatus::from_db_text(&turn_status)
                .ok_or_else(|| unknown_db_enum("turn status", &turn_status))?;
            let decision = decision
                .as_deref()
                .map(decode_final_approval_outcome)
                .transpose()?;
            let mut decision_statement = transaction.prepare(
                "select decision_id, request_id, outcome, reason, payload
                 from approval_decisions where request_id = ?1 order by rowid",
            )?;
            let decision_rows = decision_statement
                .query_map(params![request_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(decision_statement);
            match decision {
                None if !decision_rows.is_empty() => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has inconsistent decision history"
                    )));
                }
                None => {}
                Some(ApprovalOutcome::Defer) => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has inconsistent decision history"
                    )));
                }
                Some(expected_outcome) => {
                    let [(decision_id, history_request_id, outcome, reason, payload)] =
                        decision_rows.as_slice()
                    else {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has inconsistent decision history"
                        )));
                    };
                    let history_decision: ApprovalDecision = serde_json::from_str(payload)?;
                    if history_request_id != &request_id
                        || decode_final_approval_outcome(outcome)? != expected_outcome
                        || decision_reason.as_deref() != Some(reason.as_str())
                        || history_decision.decision_id != *decision_id
                        || history_decision.request_id != request_id
                        || history_decision.outcome != expected_outcome
                        || history_decision.reason != *reason
                    {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has inconsistent decision history"
                        )));
                    }
                }
            }

            match (
                pending_rowid,
                pending_thread_id.as_deref(),
                pending_turn_id.as_deref(),
                pending_tool_call_id_value.as_deref(),
                pending_payload.as_deref(),
                pending_state.as_deref(),
            ) {
                (None, None, None, None, None, None) => {}
                (
                    Some(_),
                    Some(thread_id),
                    Some(turn_id),
                    Some(tool_call_id),
                    Some(payload),
                    Some(_),
                ) => {
                    if thread_id != request.thread_id {
                        return Err(StoreError::InvalidState(
                            PENDING_TOOL_CALL_THREAD_MISMATCH.to_string(),
                        ));
                    }
                    if turn_id != request.turn_id {
                        return Err(StoreError::InvalidState(
                            PENDING_TOOL_CALL_TURN_MISMATCH.to_string(),
                        ));
                    }
                    if request.tool_call_id.as_deref() != Some(tool_call_id) {
                        return Err(StoreError::InvalidState(
                            PENDING_TOOL_CALL_ID_MISMATCH.to_string(),
                        ));
                    }
                    if payload.trim().is_empty() {
                        return Err(StoreError::InvalidState(format!(
                            "approval {request_id} has an empty pending checkpoint payload"
                        )));
                    }
                }
                _ => {
                    return Err(StoreError::InvalidState(format!(
                        "approval {request_id} has incomplete pending checkpoint metadata"
                    )));
                }
            }

            if request.tool_call_id.is_none() && pending_rowid.is_some() {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} has inconsistent checkpoint state"
                )));
            }
            rows.push(RecoveryRow {
                approval_rowid,
                request,
                decision,
                pending_rowid,
                pending_state,
                turn_status: status,
                agent_loop_status,
            });
        }

        let mut rows_by_turn = BTreeMap::<String, Vec<usize>>::new();
        for (index, row) in rows.iter().enumerate() {
            rows_by_turn
                .entry(row.request.turn_id.clone())
                .or_default()
                .push(index);
        }
        let mut superseded_executions = BTreeSet::new();
        for indexes in rows_by_turn.values() {
            let pending = indexes
                .iter()
                .copied()
                .filter(|index| rows[*index].pending_state.as_deref() == Some("pending"))
                .collect::<Vec<_>>();
            let executing = indexes
                .iter()
                .copied()
                .filter(|index| rows[*index].pending_state.as_deref() == Some("executing"))
                .collect::<Vec<_>>();
            if pending.len() > 1 || executing.len() > 1 {
                return Err(StoreError::InvalidState(
                    "turn has ambiguous approval execution recovery state".to_string(),
                ));
            }
            if let (Some(executing_index), Some(pending_index)) =
                (executing.first(), pending.first())
            {
                if rows[*executing_index].approval_rowid >= rows[*pending_index].approval_rowid {
                    return Err(StoreError::InvalidState(
                        "pending approval does not follow executing approval".to_string(),
                    ));
                }
                superseded_executions.insert(rows[*executing_index].request.request_id.clone());
            }

            for index in indexes {
                let row = &rows[*index];
                if row.request.tool_call_id.is_none() {
                    continue;
                }
                let terminal = is_terminal_turn_status(&row.turn_status);
                let has_later_active_approval = indexes.iter().any(|candidate| {
                    rows[*candidate].approval_rowid > row.approval_rowid
                        && rows[*candidate].pending_rowid.is_some()
                });
                let valid = match (row.decision, row.pending_state.as_deref(), terminal) {
                    (None, Some("pending"), false) => {
                        row.turn_status == TurnStatus::Blocked && row.agent_loop_status == "blocked"
                    }
                    (Some(ApprovalOutcome::Allow), Some("executing"), _) => true,
                    (Some(ApprovalOutcome::Allow), None, true) => true,
                    (Some(ApprovalOutcome::Allow), None, false) => has_later_active_approval,
                    (Some(ApprovalOutcome::Deny), None, true) => true,
                    _ => false,
                };
                if !valid {
                    return Err(StoreError::InvalidState(format!(
                        "approval {} has inconsistent checkpoint state",
                        row.request.request_id
                    )));
                }
            }
        }

        let mut recovered = Vec::new();
        for row in rows
            .iter()
            .filter(|row| row.pending_state.as_deref() == Some("executing"))
        {
            let request_id = &row.request.request_id;
            let turn_id = &row.request.turn_id;
            let superseded = superseded_executions.contains(request_id);
            if !superseded && !is_terminal_turn_status(&row.turn_status) {
                transaction.execute(
                    "update turns set status = ?1, agent_loop_status = 'interrupted' where turn_id = ?2",
                    params![TurnStatus::Interrupted.to_db_text(), turn_id],
                )?;
            }
            let recovery_reason = if superseded {
                "approval_execution_superseded_by_pending_handoff"
            } else {
                "approval_execution_outcome_unknown"
            };
            let summary = if superseded {
                "stale approval execution reconciled during process recovery"
            } else {
                "approval execution interrupted during process recovery"
            };
            let trace = TraceEvent {
                task_id: Some(turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": request_id,
                    "recovery_reason": recovery_reason,
                    "tool_replayed": false,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{request_id}_recovered"),
                    row.request.thread_id.clone(),
                    turn_id.clone(),
                    "app_server",
                    summary,
                )
            };
            Self::insert_turn_trace(transaction, &trace, &row.request.thread_id, turn_id)?;
            transaction.execute(
                "delete from pending_tool_calls where request_id = ?1",
                params![request_id],
            )?;
            recovered.push(request_id.clone());
        }
        Ok(recovered)
    }

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
        Self::recover_incomplete_approval_executions_for_thread(&transaction, thread_id)?;
        Self::recover_abandoned_turns_for_thread(&transaction, thread_id)?;
        transaction.commit()?;
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
            let pending_count: i64 = transaction.query_row(
                "select count(*) from pending_tool_calls
                 where turn_id = ?1 and execution_state = 'pending'",
                params![&turn_id],
                |row| row.get(0),
            )?;
            let executing_count: i64 = transaction.query_row(
                "select count(*) from pending_tool_calls
                 where turn_id = ?1 and execution_state = 'executing'",
                params![&turn_id],
                |row| row.get(0),
            )?;
            if status == TurnStatus::Blocked
                && agent_loop_status == "blocked"
                && pending_count == 1
                && executing_count == 0
            {
                continue;
            }
            if pending_count != 0 || executing_count != 0 {
                return Err(StoreError::InvalidState(format!(
                    "turn {turn_id} has inconsistent pending execution state"
                )));
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
