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
        let mut messages = Vec::new();
        self.handle_approval_decision_streaming_values(message, |message| messages.push(message))?;
        Ok(messages)
    }

    /// 执行 approval/decision，并在 continuation delta 生成时立即预留输出顺序。
    pub fn handle_approval_decision_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let result = self.handle_approval_decision_streaming_values(message, |message| {
            if sequencing_error.is_some() {
                return;
            }
            match sequence_output(&coordinator, message) {
                Ok(output) => emit(output),
                Err(error) => sequencing_error = Some(error),
            }
        });
        if let Some(error) = sequencing_error {
            return Err(error);
        }
        result
    }

    fn handle_approval_decision_streaming_values(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        let decision: ApprovalDecision = parse_params(&message)?;
        let pending_request = match self.store.get_pending_approval(&decision.request_id) {
            Ok(request) => request,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let is_tool_continuation = pending_request.tool_call_id.is_some();
        if is_tool_continuation
            && !self
                .store
                .has_pending_tool_call(&pending_request.request_id)?
        {
            emit_messages(
                &mut emit,
                not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND)?,
            );
            return Ok(());
        }
        let pending_thread = self.store.get_thread(&pending_request.thread_id)?;
        let continues_execution =
            is_tool_continuation && matches!(decision.outcome, ApprovalOutcome::Allow);
        let continuation_workspace = if continues_execution {
            if pending_thread.status != singularity_protocol::ThreadStatus::Active {
                emit_messages(
                    &mut emit,
                    invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION)?,
                );
                return Ok(());
            }
            match workspace_tools_for_thread(&pending_thread, Arc::clone(&self.sandbox_backend)) {
                Ok(tools) => Some(tools),
                Err(error) => {
                    emit_messages(
                        &mut emit,
                        invalid_state_response(message.required_id(), error)?,
                    );
                    return Ok(());
                }
            }
        } else {
            None
        };
        let _execution_guard = if continues_execution {
            let Some(guard) = self
                .store
                .try_begin_workspace_execution(&pending_request.thread_id)?
            else {
                emit_messages(
                    &mut emit,
                    invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?,
                );
                return Ok(());
            };
            Some(guard)
        } else {
            None
        };
        let mut active_turn = if continues_execution {
            let active_turn = self.activate_turn(&pending_request.turn_id)?;
            if active_turn.0.is_cancelled() {
                emit_messages(
                    &mut emit,
                    invalid_state_response(message.required_id(), EXECUTION_STOPPED)?,
                );
                return Ok(());
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
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), "Approval checkpoint unavailable")?,
            );
            return Ok(());
        }
        let recorded = match self.store.record_approval_decision(
            &decision,
            "approval",
            "approval decision recorded",
        ) {
            Ok(recorded) => recorded,
            Err(error) => {
                let response = match error {
                    StoreError::NotFound(_) => {
                        not_found_response(message.required_id(), PENDING_APPROVAL_NOT_FOUND)?
                    }
                    StoreError::InvalidState(state_message)
                        if state_message == "pending approval allow requires an active thread" =>
                    {
                        invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION)?
                    }
                    StoreError::WorkspaceHasNonterminalTurn { .. } => {
                        invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?
                    }
                    other => return Err(other.into()),
                };
                emit_messages(&mut emit, response);
                return Ok(());
            }
        };
        let pending_approval =
            match decode_pending_approval(&recorded.request, recorded.pending_tool_call.as_ref()) {
                Ok(pending_approval) => pending_approval,
                Err(_) => {
                    emit_messages(
                        &mut emit,
                        invalid_state_response(
                            message.required_id(),
                            "Approval checkpoint unavailable",
                        )?,
                    );
                    return Ok(());
                }
            };
        if matches!(decision.outcome, ApprovalOutcome::Defer) {
            emit(approval_decision_response(
                message.required_id(),
                &decision,
            )?);
            return Ok(());
        }
        if matches!(decision.outcome, ApprovalOutcome::Deny) {
            if pending_approval.is_some()
                && let Some(event) =
                    self.event_notification(AppEvent::turn_completed(&recorded.turn))?
            {
                emit(event);
            }
            emit(approval_decision_response(
                message.required_id(),
                &decision,
            )?);
            return Ok(());
        }
        let mut assistant_events = continues_execution
            .then(|| AssistantItemEventState::new(SessionStore::allocate_assistant_item_id()));
        let mut delta_error = None;
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
                    &mut |delta| {
                        if delta_error.is_some() {
                            return;
                        }
                        let Some(assistant_events) = assistant_events.as_mut() else {
                            return;
                        };
                        match self.project_assistant_delta(assistant_events, delta) {
                            Ok(messages) => emit_messages(&mut emit, messages),
                            Err(error) => delta_error = Some(error),
                        }
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
        let continuation_event_failed = delta_error.is_some();
        let continuation = match delta_error {
            Some(error) => Err(error),
            None => continuation,
        };
        let monitor_outcome = active_turn
            .as_mut()
            .and_then(|(_, guard)| guard.stabilize_monitor(&cancellation));
        if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
            let failure = TurnFailure {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            };
            let terminal = match self.terminalize_claimed_approval_error(
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
            ) {
                Ok(terminal) => terminal,
                Err(cleanup_failure) => {
                    self.emit_realtime_item_failure(&mut emit, assistant_events.as_ref())?;
                    return Err(AppServerError::TurnTerminalization {
                        stage: failure.stage,
                        cause: failure.cause,
                        failure: cleanup_failure,
                    });
                }
            };
            match terminal {
                TurnTerminalizationResult::Committed(committed) => emit_messages(
                    &mut emit,
                    self.committed_turn_events(&committed, assistant_events.as_ref())?,
                ),
                TurnTerminalizationResult::Preserved => {
                    self.emit_realtime_item_failure(&mut emit, assistant_events.as_ref())?;
                }
            }
            emit(approval_decision_response(
                message.required_id(),
                &decision,
            )?);
            return Ok(());
        }
        let terminal = match continuation {
            Ok(terminal) => terminal,
            Err(error) => {
                let failure = monitor_failure_or(
                    monitor_outcome,
                    turn_failure_from_error(
                        &error,
                        if continuation_event_failed {
                            TurnFailureStage::EventNotification
                        } else {
                            TurnFailureStage::ApprovalCheckpoint
                        },
                    ),
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
                        emit_messages(
                            &mut emit,
                            self.committed_turn_events(&committed, assistant_events.as_ref())?,
                        );
                    }
                    Ok(TurnTerminalizationResult::Preserved) => {
                        self.emit_realtime_item_failure(&mut emit, assistant_events.as_ref())?;
                    }
                    Err(cleanup_failure) => {
                        self.emit_realtime_item_failure(&mut emit, assistant_events.as_ref())?;
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
                assistant_events.as_ref().map(|events| &events.item_id),
                monitor_outcome,
            ) {
                Ok(committed) => emit_messages(
                    &mut emit,
                    self.committed_turn_events(&committed, assistant_events.as_ref())?,
                ),
                Err(_) => {
                    let failure = TurnFailure {
                        stage: TurnFailureStage::TerminalOutcome,
                        cause: TurnFailureCause::Store,
                    };
                    let terminal = match self.terminalize_claimed_approval_error(
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
                    ) {
                        Ok(terminal) => terminal,
                        Err(cleanup_failure) => {
                            self.emit_realtime_item_failure(&mut emit, assistant_events.as_ref())?;
                            return Err(AppServerError::TurnTerminalization {
                                stage: failure.stage,
                                cause: failure.cause,
                                failure: cleanup_failure,
                            });
                        }
                    };
                    match terminal {
                        TurnTerminalizationResult::Committed(committed) => emit_messages(
                            &mut emit,
                            self.committed_turn_events(&committed, assistant_events.as_ref())?,
                        ),
                        TurnTerminalizationResult::Preserved => {
                            self.emit_realtime_item_failure(&mut emit, assistant_events.as_ref())?;
                        }
                    }
                }
            }
        }
        if has_next_approvals {
            emit_messages(
                &mut emit,
                self.pending_approval_events_for_turn(&recorded.turn.turn_id)?,
            );
        }
        emit(approval_decision_response(
            message.required_id(),
            &decision,
        )?);
        Ok(())
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
            None,
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
                        None,
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
