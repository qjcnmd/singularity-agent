//! Approval request, decision, binding, and terminalization operations.

use super::support::*;
use super::*;
use crate::trace_artifact::find_trace_span_start_by_id;

/// approval 决定，以及 `AppServer` 所需的检查点和追踪数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RecordedApprovalDecision {
    /// 被决定的 approval 请求。
    pub request: ApprovalRequest,
    /// 已记录的 approval decision。
    pub decision: ApprovalDecision,
    /// claim 事务提交时绑定 turn 的一致性快照，供后续补偿路径复用。
    pub turn: Turn,
    /// 允许继续执行时保留的 pending tool call checkpoint。
    pub pending_tool_call: Option<Value>,
    /// 记录决定的 trace event。
    pub trace: TraceEvent,
}

impl SessionStore {
    /// 校验绑定后保存一个不带 checkpoint 的 approval 请求。
    pub fn create_approval(&self, request: &ApprovalRequest) -> StoreResult<()> {
        self.create_approval_with_trace(request, "approval", "approval requested")?;
        Ok(())
    }

    /// 原子保存 approval 请求及其 trace。
    pub fn create_approval_with_trace(
        &self,
        request: &ApprovalRequest,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        self.create_approval_with_pending_tool_call_and_trace(request, None, component, summary)
    }

    /// 原子保存一批 approval 请求、检查点和对应 trace。
    ///
    /// 所有请求会在同一个 `BEGIN IMMEDIATE` 事务中先完成绑定、checkpoint、
    /// resource、当前 turn 状态和唯一性校验；已有完全相同的 pending 请求按
    /// 幂等重试处理，新的请求则要么全部提交，要么不留下任何副作用。
    pub fn create_approval_batch_with_pending_tool_calls_and_trace(
        &self,
        approvals: &[(ApprovalRequest, Value)],
        component: &str,
        summary: &str,
    ) -> StoreResult<Vec<TraceEvent>> {
        if approvals.is_empty() {
            return Ok(Vec::new());
        }

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut request_ids = BTreeSet::new();
        let mut tool_call_keys = BTreeSet::new();
        let mut existing_request_ids = BTreeSet::new();
        let mut new_turn_ids = BTreeSet::new();

        // 先验证整个批次，不能让第一个合法项先改变 turn 或写入 approval。
        for (request, checkpoint) in approvals {
            if request.request_id.trim().is_empty()
                || !request_ids.insert(request.request_id.clone())
            {
                return Err(StoreError::InvalidState(
                    "approval batch request ids must be non-empty and unique".to_string(),
                ));
            }
            let tool_call_id = request.tool_call_id.clone().ok_or_else(|| {
                StoreError::InvalidState(
                    "pending approval checkpoint requires an explicit tool_call_id".to_string(),
                )
            })?;
            if !tool_call_keys.insert((request.turn_id.clone(), tool_call_id.clone())) {
                return Err(StoreError::InvalidState(
                    "approval batch tool call bindings must be unique".to_string(),
                ));
            }

            let (turn_thread_id, status_text, agent_loop_status): (String, String, String) =
                transaction
                    .query_row(
                        "select thread_id, status, agent_loop_status from turns where turn_id = ?1",
                        params![request.turn_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(|error| match error {
                        rusqlite::Error::QueryReturnedNoRows => {
                            StoreError::NotFound(format!("turn {}", request.turn_id))
                        }
                        other => StoreError::Sqlite(other),
                    })?;
            if turn_thread_id != request.thread_id {
                return Err(StoreError::InvalidState(
                    APPROVAL_TURN_THREAD_MISMATCH.to_string(),
                ));
            }
            let turn_status = TurnStatus::from_storage_text(&status_text)
                .ok_or_else(|| unknown_db_enum("turn status", &status_text))?;
            let valid_turn_state = matches!(turn_status, TurnStatus::Running | TurnStatus::Blocked)
                && matches!(agent_loop_status.as_str(), "running" | "blocked");
            if !valid_turn_state {
                return Err(StoreError::InvalidState(
                    "pending approval requires a running or blocked turn".to_string(),
                ));
            }

            let existing = transaction
                .query_row(
                    "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                     from approvals where request_id = ?1",
                    params![request.request_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((
                stored_request_id,
                stored_thread_id,
                stored_turn_id,
                stored_payload,
                stored_outcome,
                stored_reason,
            )) = existing
            {
                let stored_request = decode_stored_approval_request_row(
                    &transaction,
                    &stored_request_id,
                    &stored_thread_id,
                    &stored_turn_id,
                    &stored_payload,
                    stored_outcome.as_deref(),
                    stored_reason.as_deref(),
                )?;
                if stored_request != *request || stored_outcome.is_some() {
                    return Err(StoreError::InvalidState(
                        "approval batch request already exists with different state".to_string(),
                    ));
                }
                let stored_pending = transaction
                    .query_row(
                        "select thread_id, turn_id, tool_call_id, payload, execution_state
                         from pending_tool_calls where request_id = ?1",
                        params![request.request_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((
                    stored_pending_thread_id,
                    stored_pending_turn_id,
                    stored_tool_call_id,
                    stored_checkpoint,
                    execution_state,
                )) = stored_pending
                else {
                    return Err(StoreError::InvalidState(
                        "existing approval batch request is missing its checkpoint".to_string(),
                    ));
                };
                if execution_state != "pending"
                    || stored_pending_thread_id != request.thread_id
                    || stored_pending_turn_id != request.turn_id
                    || stored_tool_call_id != tool_call_id
                    || stored_checkpoint != serde_json::to_string(checkpoint)?
                {
                    return Err(StoreError::InvalidState(
                        "existing approval batch checkpoint does not match the request".to_string(),
                    ));
                }
                existing_request_ids.insert(request.request_id.clone());
            } else {
                let conflicting_request = transaction
                    .query_row(
                        "select request_id from pending_tool_calls
                         where turn_id = ?1 and tool_call_id = ?2 limit 1",
                        params![request.turn_id, tool_call_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if conflicting_request.is_some() {
                    return Err(StoreError::InvalidState(
                        "approval batch tool call is already bound to another request".to_string(),
                    ));
                }
                new_turn_ids.insert(request.turn_id.clone());
            }
        }

        let mut traces = Vec::new();
        for (request, checkpoint) in approvals {
            if existing_request_ids.contains(&request.request_id) {
                continue;
            }
            insert_approval(&transaction, request)?;
            let tool_call_id = request.tool_call_id.as_deref().ok_or_else(|| {
                StoreError::InvalidState(
                    "pending approval checkpoint requires an explicit tool_call_id".to_string(),
                )
            })?;
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
        }
        for turn_id in &new_turn_ids {
            let changed = transaction.execute(
                "update turns set status = ?1, agent_loop_status = 'blocked'
                 where turn_id = ?2 and status in (?3, ?1)
                   and agent_loop_status in ('running', 'blocked')",
                params![
                    TurnStatus::Blocked.to_db_text(),
                    turn_id,
                    TurnStatus::Running.to_db_text(),
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidState(
                    "pending approval requires a running or blocked turn".to_string(),
                ));
            }
        }
        for (request, _) in approvals {
            if existing_request_ids.contains(&request.request_id) {
                continue;
            }
            let mut trace =
                typed_approval_wait_start_trace(&transaction, request, component, summary)?;
            trace.payload = serde_json::json!({
                "request_id": &request.request_id,
                "action": &request.action,
                "tool_call_id": &request.tool_call_id,
            });
            traces.push(Self::insert_turn_trace(
                &transaction,
                &trace,
                &request.thread_id,
                &request.turn_id,
            )?);
        }
        transaction.commit()?;
        Ok(traces)
    }

    /// 保存 approval 请求和可选检查点，并将其绑定到阻塞的 turn。
    pub fn create_approval_with_pending_tool_call_and_trace(
        &self,
        request: &ApprovalRequest,
        pending_tool_call: Option<Value>,
        component: &str,
        summary: &str,
    ) -> StoreResult<TraceEvent> {
        if let Some(pending_tool_call) = pending_tool_call {
            let traces = self.create_approval_batch_with_pending_tool_calls_and_trace(
                &[(request.clone(), pending_tool_call)],
                component,
                summary,
            )?;
            return traces.into_iter().next().ok_or_else(|| {
                StoreError::AlreadyExists(format!("approval {}", request.request_id))
            });
        }
        let transaction = self.connection.unchecked_transaction()?;
        insert_approval(&transaction, request)?;
        let mut trace = typed_approval_wait_start_trace(&transaction, request, component, summary)?;
        trace.payload = serde_json::json!({
            "request_id": &request.request_id,
            "action": &request.action,
            "tool_call_id": &request.tool_call_id,
        });
        let trace =
            Self::insert_turn_trace(&transaction, &trace, &request.thread_id, &request.turn_id)?;
        transaction.commit()?;
        Ok(trace)
    }

    /// 列出尚未记录最终决定的 approval 请求。
    pub fn list_pending_approvals(&self) -> StoreResult<Vec<ApprovalRequest>> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select a.request_id, a.thread_id, a.turn_id, a.payload,
                    a.decision_outcome, a.decision_reason,
                    t.thread_id, d.request_id
             from approvals a
             left join turns t on t.turn_id = a.turn_id
             left join approval_decisions d on d.request_id = a.request_id
             where a.decision_outcome is null
             order by a.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        let raw_rows = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut approvals = Vec::with_capacity(raw_rows.len());
        for (
            request_id,
            thread_id,
            turn_id,
            payload,
            outcome,
            reason,
            bound_turn_thread_id,
            decision_request_id,
        ) in raw_rows
        {
            let request = decode_stored_approval_request_columns(
                &request_id,
                &thread_id,
                &turn_id,
                &payload,
                outcome.as_deref(),
                reason.as_deref(),
            )?;
            if bound_turn_thread_id.as_deref() != Some(request.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} turn binding is missing or inconsistent"
                )));
            }
            if decision_request_id.is_some() {
                return Err(StoreError::InvalidState(format!(
                    "approval {request_id} has decision history without final columns"
                )));
            }
            approvals.push(request);
        }
        transaction.commit()?;
        Ok(approvals)
    }

    /// 读取指定 request_id 的 pending approval。
    pub fn get_pending_approval(&self, request_id: &str) -> StoreResult<ApprovalRequest> {
        let (stored_request_id, stored_thread_id, stored_turn_id, payload, outcome, reason): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = self
            .connection
            .query_row(
                "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                 from approvals where request_id = ?1 and decision_outcome is null",
                params![request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {request_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        let request = decode_stored_approval_request_row(
            &self.connection,
            &stored_request_id,
            &stored_thread_id,
            &stored_turn_id,
            &payload,
            outcome.as_deref(),
            reason.as_deref(),
        )?;
        if Self::exists_in_transaction(
            &self.connection,
            "select 1 from approval_decisions where request_id = ?1",
            request_id,
        )? {
            return Err(StoreError::InvalidState(format!(
                "approval {request_id} has decision history without final columns"
            )));
        }
        Ok(request)
    }

    /// 判断 approval 是否仍绑定 pending tool call。
    pub fn has_pending_tool_call(&self, request_id: &str) -> StoreResult<bool> {
        Self::exists_in_transaction(
            &self.connection,
            "select 1 from pending_tool_calls where request_id = ?1",
            request_id,
        )
    }

    /// 读取 pending execution 的 opaque checkpoint payload；字段语义由 AppServer/AgentLoop
    /// 在 typed persistence seam 解码，Store 只返回持久化字节和值关系。
    pub fn get_pending_tool_call(&self, request_id: &str) -> StoreResult<Option<Value>> {
        let payload = self
            .connection
            .query_row(
                "select payload from pending_tool_calls where request_id = ?1",
                params![request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload
            .map(|payload| serde_json::from_str::<Value>(&payload).map_err(StoreError::from))
            .transpose()
    }

    /// 按持久化顺序列出已记录的 approval decisions。
    pub fn list_approval_decisions(&self) -> StoreResult<Vec<ApprovalDecision>> {
        type RawDecisionRow = (
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let mut statement = transaction.prepare(
            "select d.decision_id, d.request_id, d.outcome, d.reason, d.payload,
                    a.request_id, a.thread_id, a.turn_id, a.payload,
                    a.decision_outcome, a.decision_reason, t.thread_id
             from approval_decisions d
             left join approvals a on a.request_id = d.request_id
             left join turns t on t.turn_id = a.turn_id
             order by d.rowid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })?;
        let raw_rows = rows.collect::<Result<Vec<RawDecisionRow>, _>>()?;
        drop(statement);
        let mut decisions = Vec::with_capacity(raw_rows.len());
        for (
            decision_id,
            request_id,
            outcome,
            reason,
            decision_payload,
            approval_request_id,
            approval_thread_id,
            approval_turn_id,
            approval_payload,
            approval_outcome,
            approval_reason,
            bound_turn_thread_id,
        ) in raw_rows
        {
            let (
                Some(approval_request_id),
                Some(approval_thread_id),
                Some(approval_turn_id),
                Some(approval_payload),
            ) = (
                approval_request_id,
                approval_thread_id,
                approval_turn_id,
                approval_payload,
            )
            else {
                return Err(StoreError::InvalidState(format!(
                    "approval decision {decision_id} has no approval request"
                )));
            };
            let request = decode_stored_approval_request_columns(
                &approval_request_id,
                &approval_thread_id,
                &approval_turn_id,
                &approval_payload,
                approval_outcome.as_deref(),
                approval_reason.as_deref(),
            )?;
            if bound_turn_thread_id.as_deref() != Some(request.thread_id.as_str()) {
                return Err(StoreError::InvalidState(format!(
                    "approval {approval_request_id} turn binding is missing or inconsistent"
                )));
            }
            let decision = decode_stored_approval_decision_columns(
                &decision_id,
                &request_id,
                &outcome,
                &reason,
                &decision_payload,
            )?;
            if approval_request_id != request_id
                || approval_outcome.as_deref() != Some(outcome.as_str())
                || approval_reason.as_deref() != Some(reason.as_str())
            {
                return Err(StoreError::InvalidState(format!(
                    "approval decision {decision_id} does not match approval final columns"
                )));
            }
            decisions.push(decision);
        }
        transaction.commit()?;
        Ok(decisions)
    }

    /// 校验并记录 approval 结果，延后或认领其待执行操作。
    pub fn record_approval_decision(
        &self,
        decision: &ApprovalDecision,
        component: &str,
        summary: &str,
    ) -> StoreResult<RecordedApprovalDecision> {
        self.record_approval_decision_with_optional_turn_checkpoint(
            decision, component, summary, None,
        )
    }

    /// Atomically resolve an allowed approval into an ordinary turn checkpoint at a user boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn record_approval_decision_with_turn_checkpoint(
        &self,
        decision: &ApprovalDecision,
        component: &str,
        summary: &str,
        input_ids: &[String],
        checkpoint: &Value,
        checkpoint_version: u32,
        pause: bool,
    ) -> StoreResult<RecordedApprovalDecision> {
        self.record_approval_decision_with_optional_turn_checkpoint(
            decision,
            component,
            summary,
            Some((input_ids, checkpoint, checkpoint_version, pause)),
        )
    }

    fn record_approval_decision_with_optional_turn_checkpoint(
        &self,
        decision: &ApprovalDecision,
        component: &str,
        summary: &str,
        turn_checkpoint: Option<(&[String], &Value, u32, bool)>,
    ) -> StoreResult<RecordedApprovalDecision> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        // 读取尚未决定的 approval，并恢复其持久化绑定。
        let (
            stored_request_id,
            stored_thread_id,
            stored_turn_id,
            request_payload,
            request_outcome,
            request_reason,
        ): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = transaction
            .query_row(
                "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
                 from approvals where request_id = ?1 and decision_outcome is null",
                params![decision.request_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval {}", decision.request_id))
                }
                other => StoreError::Sqlite(other),
            })?;
        let request = decode_stored_approval_request_row(
            &transaction,
            &stored_request_id,
            &stored_thread_id,
            &stored_turn_id,
            &request_payload,
            request_outcome.as_deref(),
            request_reason.as_deref(),
        )?;
        if Self::exists_in_transaction(
            &transaction,
            "select 1 from approval_decisions where request_id = ?1",
            &decision.request_id,
        )? {
            return Err(StoreError::InvalidState(format!(
                "approval {} has decision history without final columns",
                decision.request_id
            )));
        }
        // 校验 pending tool call 与 approval 的 thread、turn 和 call id 一致。
        let pending_tool_call = match transaction
            .query_row(
                "select thread_id, turn_id, tool_call_id, payload from pending_tool_calls where request_id = ?1",
                params![decision.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?
        {
            Some((thread_id, turn_id, tool_call_id, payload)) => {
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
                if request.tool_call_id.as_deref() != Some(tool_call_id.as_str()) {
                    return Err(StoreError::InvalidState(
                        PENDING_TOOL_CALL_ID_MISMATCH.to_string(),
                    ));
                }
                Some(serde_json::from_str::<Value>(&payload)?)
            }
            None => {
                if request.tool_call_id.is_some() {
                    return Err(StoreError::NotFound(format!(
                        "pending tool call {}",
                        decision.request_id
                    )));
                }
                None
            }
        };
        if pending_tool_call.is_some() {
            let (turn_status, agent_loop_status): (String, String) = transaction.query_row(
                "select status, agent_loop_status from turns where turn_id = ?1",
                params![request.turn_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if turn_status != TurnStatus::Blocked.to_db_text() || agent_loop_status != "blocked" {
                return Err(StoreError::InvalidState(
                    "pending approval is not bound to a blocked turn".to_string(),
                ));
            }
        }
        let include_follow_up = decision.outcome == ApprovalOutcome::Deny;
        let (pause_requested, pending_input_count): (bool, u64) = transaction.query_row(
            "select pause_requested = 1,
                    (select count(*) from turn_inputs
                     where turn_id = ?1 and delivery_state = 'pending'
                       and (delivery = 'steer' or ?2))
             from turns where turn_id = ?1",
            params![request.turn_id, include_follow_up],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let boundary_pending = pause_requested || pending_input_count != 0;
        match turn_checkpoint {
            Some((input_ids, _, _, pause))
                if pause != pause_requested || input_ids.len() as u64 != pending_input_count =>
            {
                return Err(StoreError::TurnBoundaryPending {
                    turn_id: request.turn_id.clone(),
                });
            }
            None if boundary_pending && decision.outcome != ApprovalOutcome::Defer => {
                return Err(StoreError::TurnBoundaryPending {
                    turn_id: request.turn_id.clone(),
                });
            }
            _ => {}
        }
        if let Some((input_ids, checkpoint, _, pause)) = turn_checkpoint {
            if !matches!(
                decision.outcome,
                ApprovalOutcome::Allow | ApprovalOutcome::Deny
            ) || pending_tool_call.is_none()
                || (!pause && input_ids.is_empty())
                || !checkpoint.is_object()
            {
                return Err(StoreError::InvalidState(
                    "approval turn checkpoint handoff is invalid".to_string(),
                ));
            }
            for input_id in input_ids {
                let pending: bool = transaction.query_row(
                    "select exists(
                        select 1 from turn_inputs
                        where input_id = ?1 and turn_id = ?2 and delivery_state = 'pending'
                           and (delivery = 'steer' or ?3)
                     )",
                    params![input_id, request.turn_id, include_follow_up],
                    |row| row.get(0),
                )?;
                if !pending {
                    return Err(StoreError::InvalidState(format!(
                        "approval turn input {input_id} is not pending steer"
                    )));
                }
            }
        }
        if pending_tool_call.is_some()
            && (decision.outcome == ApprovalOutcome::Allow || turn_checkpoint.is_some())
        {
            let thread_status: String = transaction.query_row(
                "select status from threads where thread_id = ?1",
                params![request.thread_id],
                |row| row.get(0),
            )?;
            if thread_status != ThreadStatus::Active.to_db_text() {
                return Err(StoreError::InvalidState(
                    PENDING_APPROVAL_ALLOW_REQUIRES_ACTIVE_THREAD.to_string(),
                ));
            }
            Self::ensure_workspace_has_no_nonterminal_turn(
                &transaction,
                &request.thread_id,
                Some(&request.turn_id),
            )?;
        }
        // Defer 只记录 trace，保留 approval 与 checkpoint 以便后续恢复。
        if decision.outcome == ApprovalOutcome::Defer {
            let trace = TraceEvent {
                task_id: Some(request.turn_id.clone()),
                payload: serde_json::json!({
                    "request_id": decision.request_id,
                    "decision_id": decision.decision_id,
                    "outcome": decision.outcome,
                }),
                ..TraceEvent::for_turn(
                    format!("trace_{}_defer_{}", decision.request_id, Uuid::new_v4()),
                    request.thread_id.clone(),
                    request.turn_id.clone(),
                    component,
                    "approval deferred",
                )
            };
            let trace = Self::insert_turn_trace(
                &transaction,
                &trace,
                &request.thread_id,
                &request.turn_id,
            )?;
            let turn = self.turn_in_transaction(&transaction, &request.turn_id)?;
            transaction.commit()?;
            return Ok(RecordedApprovalDecision {
                request,
                decision: decision.clone(),
                turn,
                pending_tool_call,
                trace,
            });
        }
        // 其他 outcome 写入 approval decision history，并推进或清理执行状态。
        let changed = transaction.execute(
            "update approvals set decision_outcome = ?1, decision_reason = ?2 where request_id = ?3 and decision_outcome is null",
            params![
                final_approval_outcome_to_db_text(decision.outcome)?,
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
                final_approval_outcome_to_db_text(decision.outcome)?,
                decision.reason,
                serde_json::to_string(decision)?
            ],
        )?;
        if let Some((input_ids, checkpoint, checkpoint_version, pause)) = turn_checkpoint {
            let deleted = transaction.execute(
                "delete from pending_tool_calls
                     where request_id = ?1 and execution_state = 'pending'",
                params![decision.request_id],
            )?;
            if deleted != 1 {
                return Err(StoreError::InvalidState(format!(
                    "pending execution {} is not in pending state",
                    decision.request_id
                )));
            }
            for input_id in input_ids {
                let changed = transaction.execute(
                    "update turn_inputs
                         set delivery_state = 'consumed', consumed_at = current_timestamp
                         where input_id = ?1 and turn_id = ?2 and delivery_state = 'pending'
                            and (delivery = 'steer' or ?3)",
                    params![input_id, request.turn_id, include_follow_up],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidState(format!(
                        "approval turn input {input_id} is not pending steer"
                    )));
                }
            }
            Self::upsert_turn_checkpoint(
                &transaction,
                &request.turn_id,
                &request.thread_id,
                checkpoint,
                checkpoint_version,
            )?;
            let (status, agent_loop_status) = if pause {
                (TurnStatus::Paused.to_db_text(), "paused")
            } else {
                (TurnStatus::Running.to_db_text(), "running")
            };
            let changed = transaction.execute(
                "update turns
                     set status = ?1, agent_loop_status = ?2, pause_requested = 0
                     where turn_id = ?3 and status = ?4 and agent_loop_status = 'blocked'",
                params![
                    status,
                    agent_loop_status,
                    request.turn_id,
                    TurnStatus::Blocked.to_db_text()
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidState(
                    "pending approval is not bound to a blocked turn".to_string(),
                ));
            }
        } else if decision.outcome == ApprovalOutcome::Allow {
            let changed = transaction.execute(
                    "update pending_tool_calls set execution_state = 'executing' where request_id = ?1 and execution_state = 'pending'",
                    params![decision.request_id],
                )?;
            if pending_tool_call.is_some() && changed != 1 {
                return Err(StoreError::InvalidState(format!(
                    "pending execution {} is not in pending state",
                    decision.request_id
                )));
            }
        } else {
            transaction.execute(
                "delete from pending_tool_calls where request_id = ?1",
                params![decision.request_id],
            )?;
            if pending_tool_call.is_some() {
                let mut terminal_trace = TraceEvent {
                    task_id: Some(request.turn_id.clone()),
                    payload: serde_json::json!({
                        "request_id": decision.request_id,
                        "decision_id": decision.decision_id,
                        "outcome": decision.outcome,
                    }),
                    ..TraceEvent::for_turn(
                        format!("trace_{}_denied", decision.decision_id),
                        request.thread_id.clone(),
                        request.turn_id.clone(),
                        "agent_loop",
                        "approval denied",
                    )
                };
                let current_turn = self.turn_in_transaction(&transaction, &request.turn_id)?;
                terminal_trace = typed_turn_end_trace(
                    &transaction,
                    &terminal_trace,
                    &current_turn,
                    &TurnStatus::Failed,
                )?;
                let changed = transaction.execute(
                    "update turns set status = ?1, agent_loop_status = 'failed' where turn_id = ?2 and status = ?3 and agent_loop_status = 'blocked'",
                    params![
                        TurnStatus::Failed.to_db_text(),
                        request.turn_id,
                        TurnStatus::Blocked.to_db_text(),
                    ],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidState(
                        "pending approval is not bound to a blocked turn".to_string(),
                    ));
                }
                Self::insert_turn_trace(
                    &transaction,
                    &terminal_trace,
                    &request.thread_id,
                    &request.turn_id,
                )?;
            }
        }
        // 将最终决定与其绑定的 turn trace 一并提交。
        let mut trace =
            typed_approval_wait_end_trace(&transaction, &request, decision, component, summary)?;
        trace.payload = serde_json::json!({
            "request_id": decision.request_id,
            "decision_id": decision.decision_id,
            "outcome": decision.outcome,
        });
        let trace =
            Self::insert_turn_trace(&transaction, &trace, &request.thread_id, &request.turn_id)?;
        let turn = self.turn_in_transaction(&transaction, &request.turn_id)?;
        transaction.commit()?;
        Ok(RecordedApprovalDecision {
            request,
            decision: decision.clone(),
            turn,
            pending_tool_call,
            trace,
        })
    }

    /// 读取指定 decision_id 的 approval decision。
    pub fn get_approval_decision(&self, decision_id: &str) -> StoreResult<ApprovalDecision> {
        let (stored_decision_id, request_id, outcome, reason, payload): (
            String,
            String,
            String,
            String,
            String,
        ) = self
            .connection
            .query_row(
                "select decision_id, request_id, outcome, reason, payload
                 from approval_decisions where decision_id = ?1",
                params![decision_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .map_err(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => {
                    StoreError::NotFound(format!("approval decision {decision_id}"))
                }
                other => StoreError::Sqlite(other),
            })?;
        decode_stored_approval_decision_row(
            &self.connection,
            &stored_decision_id,
            &request_id,
            &outcome,
            &reason,
            &payload,
        )
    }
}

pub(crate) fn typed_approval_wait_start_trace(
    transaction: &Transaction<'_>,
    request: &ApprovalRequest,
    component: &str,
    summary: &str,
) -> StoreResult<TraceEvent> {
    let turn_start = find_trace_span_start(
        transaction,
        &request.thread_id,
        &request.turn_id,
        TraceSpanKind::Turn,
    )?
    .ok_or_else(|| {
        StoreError::InvalidState(format!(
            "approval {} is missing its bound typed turn start",
            request.request_id
        ))
    })?;
    let parent_span_id = turn_start.span_id.ok_or_else(|| {
        StoreError::InvalidState(format!(
            "approval {} has no typed turn parent identity",
            request.request_id
        ))
    })?;
    let mut event = TraceEvent::for_turn(
        format!("trace_{}", request.request_id),
        request.thread_id.clone(),
        request.turn_id.clone(),
        component,
        summary,
    );
    event.timestamp = Some(Timestamp::now_utc().to_string());
    event.span_id = Some(format!("approval_wait_span_{}", request.request_id));
    event.parent_span_id = Some(parent_span_id);
    event.span_kind = Some(TraceSpanKind::ApprovalWait);
    event.span_phase = Some(TraceSpanPhase::Start);
    Ok(event)
}

fn typed_approval_wait_end_trace(
    transaction: &Transaction<'_>,
    request: &ApprovalRequest,
    decision: &ApprovalDecision,
    component: &str,
    summary: &str,
) -> StoreResult<TraceEvent> {
    let expected_span_id = format!("approval_wait_span_{}", request.request_id);
    let start = find_trace_span_start_by_id(
        transaction,
        &request.thread_id,
        &request.turn_id,
        TraceSpanKind::ApprovalWait,
        &expected_span_id,
    )?
    .ok_or_else(|| {
        StoreError::InvalidState(format!(
            "approval {} is missing its persisted typed wait start",
            request.request_id
        ))
    })?;
    let start_timestamp = start
        .timestamp
        .as_deref()
        .and_then(|timestamp| Timestamp::parse(timestamp).ok())
        .ok_or_else(|| {
            StoreError::InvalidState(format!(
                "approval {} typed wait start has no valid timestamp",
                request.request_id
            ))
        })?;
    let end_timestamp = Timestamp::now_utc();
    let duration_ms = end_timestamp
        .unix_ms()
        .checked_sub(start_timestamp.unix_ms())
        .ok_or_else(|| {
            StoreError::InvalidState(format!(
                "approval {} typed end timestamp precedes its persisted start",
                request.request_id
            ))
        })?;
    let span_status = match decision.outcome {
        ApprovalOutcome::Allow => TraceSpanStatus::Ok,
        ApprovalOutcome::Deny => TraceSpanStatus::Error,
        ApprovalOutcome::Defer => {
            return Err(StoreError::InvalidState(
                "deferred approval must keep its wait span open".to_string(),
            ));
        }
    };
    let mut event = TraceEvent::for_turn(
        format!("trace_{}", decision.decision_id),
        request.thread_id.clone(),
        request.turn_id.clone(),
        component,
        summary,
    );
    event.timestamp = Some(end_timestamp.to_string());
    event.span_id = start.span_id;
    event.parent_span_id = start.parent_span_id;
    event.span_kind = Some(TraceSpanKind::ApprovalWait);
    event.span_phase = Some(TraceSpanPhase::End);
    event.span_status = Some(span_status);
    event.duration_ms = Some(duration_ms);
    Ok(event)
}

pub(crate) fn decode_final_approval_outcome(value: &str) -> StoreResult<ApprovalOutcome> {
    match migration::decode_legacy_db_enum::<ApprovalOutcome>(value)? {
        ApprovalOutcome::Allow => Ok(ApprovalOutcome::Allow),
        ApprovalOutcome::Deny => Ok(ApprovalOutcome::Deny),
        ApprovalOutcome::Defer => Err(StoreError::InvalidState(
            "defer approval outcome must remain pending".to_string(),
        )),
    }
}

pub(crate) fn final_approval_outcome_to_db_text(
    outcome: ApprovalOutcome,
) -> StoreResult<&'static str> {
    match outcome {
        ApprovalOutcome::Allow => Ok(ApprovalOutcome::Allow.to_db_text()),
        ApprovalOutcome::Deny => Ok(ApprovalOutcome::Deny.to_db_text()),
        ApprovalOutcome::Defer => Err(StoreError::InvalidState(
            "defer approval outcome must remain pending".to_string(),
        )),
    }
}

// 读取 approval 行时同时验证列、payload 和 request 的 turn 绑定。
pub(crate) fn decode_stored_approval_request_row(
    connection: &Connection,
    request_id: &str,
    stored_thread_id: &str,
    stored_turn_id: &str,
    payload: &str,
    outcome: Option<&str>,
    reason: Option<&str>,
) -> StoreResult<ApprovalRequest> {
    let request = decode_stored_approval_request_columns(
        request_id,
        stored_thread_id,
        stored_turn_id,
        payload,
        outcome,
        reason,
    )?;
    ensure_request_turn_binding(connection, &request)?;
    Ok(request)
}

pub(crate) fn decode_stored_approval_request_columns(
    request_id: &str,
    stored_thread_id: &str,
    stored_turn_id: &str,
    payload: &str,
    outcome: Option<&str>,
    reason: Option<&str>,
) -> StoreResult<ApprovalRequest> {
    let request: ApprovalRequest = serde_json::from_str(payload).map_err(|error| {
        StoreError::InvalidState(format!("approval {request_id} payload is invalid: {error}"))
    })?;
    if request.request_id != request_id
        || request.thread_id != stored_thread_id
        || request.turn_id != stored_turn_id
        || request.thread_id.trim().is_empty()
        || request.turn_id.trim().is_empty()
    {
        return Err(StoreError::InvalidState(format!(
            "approval {request_id} payload binding is invalid"
        )));
    }
    let outcome = outcome.map(decode_final_approval_outcome).transpose()?;
    if outcome.is_none() && reason.is_some() {
        return Err(StoreError::InvalidState(format!(
            "approval {request_id} has a decision reason without a decision"
        )));
    }
    Ok(request)
}

// 读取 decision 行时同时验证 decision 表、approval 最终列和 payload。
pub(crate) fn decode_stored_approval_decision_row(
    connection: &Connection,
    decision_id: &str,
    request_id: &str,
    outcome: &str,
    reason: &str,
    payload: &str,
) -> StoreResult<ApprovalDecision> {
    let decision =
        decode_stored_approval_decision_columns(decision_id, request_id, outcome, reason, payload)?;
    let (
        approval_request_id,
        approval_thread_id,
        approval_turn_id,
        approval_payload,
        approval_outcome,
        approval_reason,
    ): (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    ) = connection
        .query_row(
            "select request_id, thread_id, turn_id, payload, decision_outcome, decision_reason
             from approvals where request_id = ?1",
            params![request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => StoreError::InvalidState(format!(
                "approval decision {decision_id} has no approval request"
            )),
            other => StoreError::Sqlite(other),
        })?;
    let _request = decode_stored_approval_request_row(
        connection,
        &approval_request_id,
        &approval_thread_id,
        &approval_turn_id,
        &approval_payload,
        approval_outcome.as_deref(),
        approval_reason.as_deref(),
    )?;
    if approval_request_id != request_id
        || approval_outcome.as_deref() != Some(outcome)
        || approval_reason.as_deref() != Some(reason)
    {
        return Err(StoreError::InvalidState(format!(
            "approval decision {decision_id} does not match approval final columns"
        )));
    }
    Ok(decision)
}

pub(crate) fn decode_stored_approval_decision_columns(
    decision_id: &str,
    request_id: &str,
    outcome: &str,
    reason: &str,
    payload: &str,
) -> StoreResult<ApprovalDecision> {
    let decision: ApprovalDecision = serde_json::from_str(payload).map_err(|error| {
        StoreError::InvalidState(format!(
            "approval decision {decision_id} payload is invalid: {error}"
        ))
    })?;
    let expected_outcome = decode_final_approval_outcome(outcome)?;
    if decision.decision_id != decision_id
        || decision.request_id != request_id
        || decision.outcome != expected_outcome
        || decision.reason != reason
    {
        return Err(StoreError::InvalidState(format!(
            "approval decision {decision_id} columns do not match payload"
        )));
    }
    Ok(decision)
}
