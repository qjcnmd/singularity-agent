//! Approval request, checkpoint, and continuation handling.

use super::*;

impl AppServer {
    pub(super) fn approval_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let approvals = self.store.list_pending_approvals()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ApprovalListResult { approvals })?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn approval_center(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
            ApprovalCenterResult {
                pending_approvals: self.store.list_pending_approvals()?,
                decisions: self.store.list_approval_decisions()?,
            },
        )
    }

    pub(super) fn approval_request(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let _request: ApprovalRequest = parse_params(&message)?;
        invalid_state_response(message.required_id(), APPROVAL_REQUEST_INTERNAL_ONLY)
    }

    /// 记录 approval，并保留、失败处理或恢复已认领的检查点。
    pub(super) fn approval_decision(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let decision: ApprovalDecision = parse_params(&message)?;
        let pending_request = match self.store.get_pending_approval(&decision.request_id) {
            Ok(request) => request,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let is_tool_continuation = pending_request.tool_call_id.is_some();
        if is_tool_continuation
            && !self
                .store
                .has_pending_tool_call(&pending_request.request_id)?
        {
            return not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND);
        }
        let pending_thread = self.store.get_thread(&pending_request.thread_id)?;
        let continues_execution =
            is_tool_continuation && matches!(decision.outcome, ApprovalOutcome::Allow);
        let continuation_workspace = if continues_execution {
            if pending_thread.status != singularity_protocol::ThreadStatus::Active {
                return invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION);
            }
            match workspace_tools_for_thread(&pending_thread, Arc::clone(&self.sandbox_backend)) {
                Ok(tools) => Some(tools),
                Err(error) => return invalid_state_response(message.required_id(), error),
            }
        } else {
            None
        };
        let _execution_guard = if continues_execution {
            let Some(guard) = self
                .store
                .try_begin_workspace_execution(&pending_request.thread_id)?
            else {
                return invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE);
            };
            Some(guard)
        } else {
            None
        };
        let mut active_turn = if continues_execution {
            let active_turn = self.activate_turn(&pending_request.turn_id)?;
            if active_turn.0.is_cancelled() {
                return invalid_state_response(message.required_id(), EXECUTION_STOPPED);
            }
            Some(active_turn)
        } else {
            None
        };
        if active_turn
            .as_ref()
            .and_then(|(_, guard)| guard.monitor_outcome())
            == Some(CancellationMonitorOutcome::InfrastructureFailure)
        {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        let pending_payload = if is_tool_continuation {
            self.store
                .get_pending_tool_call(&pending_request.request_id)?
        } else {
            None
        };
        if decode_pending_approval(&pending_request, pending_payload.as_ref()).is_err() {
            return invalid_state_response(
                message.required_id(),
                "Approval checkpoint unavailable",
            );
        }
        let recorded = match self.store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            Ok(recorded) => recorded,
            Err(error) => {
                return match error {
                    StoreError::NotFound(_) => {
                        not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND)
                    }
                    StoreError::InvalidState(state_message)
                        if state_message == "pending approval allow requires an active thread" =>
                    {
                        invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION)
                    }
                    StoreError::WorkspaceHasNonterminalTurn { .. } => {
                        invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)
                    }
                    other => Err(other.into()),
                };
            }
        };
        let pending_approval =
            match decode_pending_approval(&recorded.request, recorded.pending_tool_call.as_ref()) {
                Ok(pending_approval) => pending_approval,
                Err(_) => {
                    return invalid_state_response(
                        message.required_id(),
                        "Approval checkpoint unavailable",
                    );
                }
            };
        if matches!(decision.outcome, ApprovalOutcome::Defer) {
            return Ok(vec![approval_decision_response(
                message.required_id(),
                &decision,
            )?]);
        }
        if matches!(decision.outcome, ApprovalOutcome::Deny) {
            let mut messages = Vec::new();
            if pending_approval.is_some()
                && let Some(event) =
                    self.event_notification(AppEvent::turn_completed(&recorded.turn))?
            {
                messages.push(event);
            }
            messages.push(approval_decision_response(
                message.required_id(),
                &decision,
            )?);
            return Ok(messages);
        }
        let mut messages = Vec::new();
        let cancellation = active_turn
            .as_ref()
            .map(|(cancellation, _guard)| cancellation.clone())
            .unwrap_or_default();
        let continuation = {
            let monitor_control = active_turn
                .as_ref()
                .and_then(|(_cancellation, guard)| guard.monitor_control());
            (|| -> AppServerResult<_> {
                let resumed = self.resume_agent_loop(
                    ApprovalResumeInput {
                        request: &recorded.request,
                        decision: &decision,
                        turn: &recorded.turn,
                        thread: &pending_thread,
                        pending_approval: pending_approval.clone(),
                    },
                    ApprovalResumeContext {
                        cancellation: &cancellation,
                        monitor_control,
                        prepared_workspace_tools: continuation_workspace.clone(),
                    },
                )?;
                let terminal = if let Some(resumed) = resumed {
                    Some(resumed)
                } else {
                    self.approval_no_resume_status(
                        &recorded.request,
                        &decision,
                        &recorded.turn,
                        &pending_thread,
                        pending_approval.as_ref(),
                    )?
                    .map(|(turn, run_status)| (turn, run_status, Vec::new()))
                };
                Ok(terminal)
            })()
        };
        let monitor_outcome = active_turn
            .as_mut()
            .and_then(|(_, guard)| guard.stabilize_monitor(&cancellation));
        if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
            let failure = TurnFailure {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            };
            let terminal = self
                .terminalize_claimed_approval_error(
                    &recorded.request,
                    &decision,
                    pending_approval.as_ref(),
                    ApprovalTerminalizationContext {
                        turn: &recorded.turn,
                        thread: &pending_thread,
                        prior_status: None,
                        cancellation: &cancellation,
                        monitor_outcome,
                        failure,
                    },
                )
                .map_err(|cleanup_failure| AppServerError::TurnTerminalization {
                    stage: failure.stage,
                    cause: failure.cause,
                    failure: cleanup_failure,
                })?;
            if let TurnTerminalizationResult::Committed(committed) = terminal {
                messages.extend(self.committed_turn_events(&committed)?);
            }
            messages.push(approval_decision_response(
                message.required_id(),
                &decision,
            )?);
            return Ok(messages);
        }
        let terminal = match continuation {
            Ok(terminal) => terminal,
            Err(error) => {
                let failure = monitor_failure_or(
                    monitor_outcome,
                    turn_failure_from_error(&error, TurnFailureStage::ApprovalCheckpoint),
                );
                let terminal = self.terminalize_claimed_approval_error(
                    &recorded.request,
                    &decision,
                    pending_approval.as_ref(),
                    ApprovalTerminalizationContext {
                        turn: &recorded.turn,
                        thread: &pending_thread,
                        prior_status: None,
                        cancellation: &cancellation,
                        monitor_outcome,
                        failure,
                    },
                );
                match terminal {
                    Ok(TurnTerminalizationResult::Committed(committed)) => {
                        messages.extend(self.committed_turn_events(&committed)?);
                    }
                    Ok(TurnTerminalizationResult::Preserved) => {}
                    Err(cleanup_failure) => {
                        return Err(AppServerError::TurnTerminalization {
                            stage: failure.stage,
                            cause: failure.cause,
                            failure: cleanup_failure,
                        });
                    }
                }
                None
            }
        };
        let has_next_approvals = terminal
            .as_ref()
            .is_some_and(|(_, _, next_approvals)| !next_approvals.is_empty());
        if let Some((turn, run_status, next_approvals)) = terminal {
            let mut effective_status = run_status.clone();
            if monitor_outcome == Some(CancellationMonitorOutcome::UserCancellation)
                || cancellation.is_cancelled()
            {
                mark_run_cancelled(&mut effective_status);
            }
            match self.commit_effective_turn_status_resolving_approval(
                &decision.request_id,
                &turn,
                &effective_status,
                &next_approvals,
                monitor_outcome,
            ) {
                Ok(committed) => messages.extend(self.committed_turn_events(&committed)?),
                Err(_) => {
                    let failure = TurnFailure {
                        stage: TurnFailureStage::TerminalOutcome,
                        cause: TurnFailureCause::Store,
                    };
                    let terminal = self
                        .terminalize_claimed_approval_error(
                            &recorded.request,
                            &decision,
                            pending_approval.as_ref(),
                            ApprovalTerminalizationContext {
                                turn: &turn,
                                thread: &pending_thread,
                                prior_status: Some(&effective_status),
                                cancellation: &cancellation,
                                monitor_outcome,
                                failure,
                            },
                        )
                        .map_err(|cleanup_failure| AppServerError::TurnTerminalization {
                            stage: failure.stage,
                            cause: failure.cause,
                            failure: cleanup_failure,
                        })?;
                    if let TurnTerminalizationResult::Committed(committed) = terminal {
                        messages.extend(self.committed_turn_events(&committed)?);
                    }
                }
            }
        }
        if has_next_approvals {
            messages.extend(self.pending_approval_events_for_turn(&recorded.turn.turn_id)?);
        }
        messages.push(approval_decision_response(
            message.required_id(),
            &decision,
        )?);
        Ok(messages)
    }

    pub(super) fn terminalize_claimed_approval_error(
        &self,
        _request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_approval: Option<&PendingApprovalOccurrence>,
        context: ApprovalTerminalizationContext<'_>,
    ) -> Result<TurnTerminalizationResult, TurnTerminalizationFailure> {
        let ApprovalTerminalizationContext {
            turn,
            thread,
            prior_status,
            cancellation,
            monitor_outcome,
            failure,
        } = context;
        if is_terminal_turn_status(&turn.status)
            || turn.agent_loop_status == AgentStatus::Cancelled.as_str()
        {
            return Ok(TurnTerminalizationResult::Preserved);
        }
        let failure_message = match failure.cause {
            TurnFailureCause::StoredInputUnavailable => format!(
                "approval continuation failed during {}; stored user input unavailable",
                failure.stage
            ),
            _ => format!("approval continuation failed during {}", failure.stage),
        };
        let fallback_status = approval_terminal_status(
            thread,
            decision,
            pending_approval,
            AgentStatus::Failed,
            "unavailable",
            failure_message.clone(),
        );
        let mut run_status = fallback_status;
        if let Some(prior_status) = prior_status {
            run_status.model_turns = prior_status.model_turns;
            run_status.tool_calls = prior_status.tool_calls;
            run_status.approval_count = prior_status.approval_count;
        }
        run_status.status = AgentStatus::Failed;
        run_status.completed = false;
        run_status.final_answer = None;
        run_status.error = Some(failure_message);
        if cancellation.is_cancelled()
            && monitor_outcome != Some(CancellationMonitorOutcome::InfrastructureFailure)
        {
            mark_run_cancelled(&mut run_status);
        }
        run_status.audit_events.push(project_audit_event(&json!({
            "component": "app_server",
            "failure_kind": "approval_continuation",
            "failure_stage": failure.stage.as_str(),
            "failure_cause": failure.cause.as_str(),
        })));
        match self.commit_effective_turn_status_resolving_approval(
            &decision.request_id,
            turn,
            &run_status,
            &[],
            monitor_outcome,
        ) {
            Ok(committed) => Ok(TurnTerminalizationResult::Committed(Box::new(committed))),
            Err(_) => {
                let latest = self
                    .store
                    .get_turn(&turn.turn_id)
                    .map_err(|_| TurnTerminalizationFailure::Store)?;
                if is_terminal_turn_status(&latest.status)
                    || latest.agent_loop_status == AgentStatus::Cancelled.as_str()
                {
                    return Ok(TurnTerminalizationResult::Preserved);
                }
                if latest.agent_loop_status == AgentStatus::CancelRequested.as_str()
                    && monitor_outcome != Some(CancellationMonitorOutcome::InfrastructureFailure)
                {
                    let mut interrupted =
                        AgentRunStatus::failed("turn interrupted by user request");
                    mark_run_cancelled(&mut interrupted);
                    if let Ok(committed) = self.commit_effective_turn_status_resolving_approval(
                        &decision.request_id,
                        &latest,
                        &interrupted,
                        &[],
                        monitor_outcome,
                    ) {
                        return Ok(TurnTerminalizationResult::Committed(Box::new(committed)));
                    }
                    let latest = self
                        .store
                        .get_turn(&turn.turn_id)
                        .map_err(|_| TurnTerminalizationFailure::Store)?;
                    if is_terminal_turn_status(&latest.status)
                        || latest.agent_loop_status == AgentStatus::Cancelled.as_str()
                    {
                        return Ok(TurnTerminalizationResult::Preserved);
                    }
                }
                Err(TurnTerminalizationFailure::Store)
            }
        }
    }

    pub(super) fn approval_no_resume_status(
        &self,
        _request: &ApprovalRequest,
        decision: &ApprovalDecision,
        turn: &Turn,
        thread: &Thread,
        pending_approval: Option<&PendingApprovalOccurrence>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus)>> {
        if pending_approval.is_none() {
            return Ok(None);
        }
        let (status, audit_decision, message) = if turn.agent_loop_status
            == AgentStatus::CancelRequested.as_str()
        {
            (
                AgentStatus::Cancelled,
                "unavailable",
                "approval continuation interrupted before resume",
            )
        } else if is_terminal_turn_status(&turn.status) {
            (
                AgentStatus::from(turn.agent_loop_status.as_str()),
                "unavailable",
                "approval continuation already reached a terminal turn",
            )
        } else if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            (
                AgentStatus::Failed,
                "unavailable",
                "approval allowed but turn state changed before agent loop resume",
            )
        } else {
            match decision.outcome {
                ApprovalOutcome::Allow if pending_approval.is_some() => (
                    AgentStatus::Failed,
                    "unavailable",
                    "approval allowed but agent loop turn could not resume",
                ),
                ApprovalOutcome::Allow => (
                    AgentStatus::Failed,
                    "unavailable",
                    "approval allowed but pending tool call is unavailable",
                ),
                ApprovalOutcome::Deny => (AgentStatus::Failed, "denied", "approval denied"),
                ApprovalOutcome::Defer => (AgentStatus::Blocked, "deferred", "approval deferred"),
            }
        };
        let run_status = approval_terminal_status(
            thread,
            decision,
            pending_approval,
            status,
            audit_decision,
            message,
        );
        Ok(Some((turn.clone(), run_status)))
    }
}
