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
        let trace_binding = RefCell::new(None);
        let result = self.handle_approval_decision_streaming_values(
            message,
            |binding| *trace_binding.borrow_mut() = Some(binding),
            |message| messages.push(message),
        );
        self.pending_transport_trace_binding = trace_binding.into_inner();
        result?;
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
        let trace_binding = RefCell::new(None);
        let result = self.handle_approval_decision_streaming_values(
            message,
            |binding| *trace_binding.borrow_mut() = Some(binding),
            |message| {
                if sequencing_error.is_some() {
                    return;
                }
                match sequence_output(&coordinator, message, trace_binding.borrow().clone()) {
                    Ok(output) => emit(output),
                    Err(error) => sequencing_error = Some(error),
                }
            },
        );
        if let Some(error) = sequencing_error {
            return Err(error);
        }
        result
    }

    fn handle_approval_decision_streaming_values(
        &mut self,
        message: JsonRpcMessage,
        mut bind_trace: impl FnMut(TransportTraceBinding),
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
        bind_trace(TransportTraceBinding::for_turn(
            pending_request.thread_id.clone(),
            pending_request.turn_id.clone(),
        ));
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
        'decision_attempt: loop {
            let pending_thread = self.store.get_thread(&pending_request.thread_id)?;
            let deny_boundary = if is_tool_continuation && decision.outcome == ApprovalOutcome::Deny
            {
                Some(
                    self.store
                        .turn_boundary_state(&pending_request.turn_id, true)?,
                )
            } else {
                None
            };
            let continues_execution = is_tool_continuation
                && (decision.outcome == ApprovalOutcome::Allow
                    || deny_boundary.as_ref().is_some_and(|boundary| {
                        !boundary.inputs.is_empty() || boundary.pause_requested
                    }));
            let continuation_workspace = if continues_execution {
                if pending_thread.status != singularity_protocol::ThreadStatus::Active {
                    emit_messages(
                        &mut emit,
                        invalid_state_response(
                            message.required_id(),
                            THREAD_ARCHIVED_CONTINUATION,
                        )?,
                    );
                    return Ok(());
                }
                match workspace_tools_for_thread(&pending_thread, Arc::clone(&self.sandbox_backend))
                {
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
            let pending_before_decision =
                match decode_pending_approval(&pending_request, pending_payload.as_ref()) {
                    Ok(pending) => pending,
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
            let build_handoff =
                || -> AppServerResult<(Option<TurnCheckpoint>, Vec<String>, bool)> {
                    if !continues_execution {
                        return Ok((None, Vec::new(), false));
                    }
                    let boundary = self.store.turn_boundary_state(
                        &pending_request.turn_id,
                        decision.outcome == ApprovalOutcome::Deny,
                    )?;
                    if boundary.inputs.is_empty() && !boundary.pause_requested {
                        return Ok((None, Vec::new(), false));
                    }
                    let Some(pending) = pending_before_decision.as_ref() else {
                        return Err(AppServerError::TurnExecution {
                            stage: TurnFailureStage::ApprovalCheckpoint,
                            cause: TurnFailureCause::Serialization,
                        });
                    };
                    let mut messages = Vec::with_capacity(boundary.inputs.len());
                    let mut input_ids = Vec::with_capacity(boundary.inputs.len());
                    for input in &boundary.inputs {
                        let items: Vec<singularity_protocol::InputItem> =
                            serde_json::from_value(input.input.clone())?;
                        messages.push(
                            items
                                .into_iter()
                                .map(|item| match item {
                                    singularity_protocol::InputItem::Text { text } => text,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                        input_ids.push(input.input_id.clone());
                    }
                    let checkpoint = (if decision.outcome == ApprovalOutcome::Deny {
                        pending
                            .checkpoint()
                            .into_turn_checkpoint_after_denial(&messages)
                    } else {
                        pending
                            .checkpoint()
                            .into_turn_checkpoint(&messages, !messages.is_empty())
                    })
                    .and_then(|checkpoint| {
                        checkpoint
                            .resume_attempt()
                            .checked_add(1)
                            .map(|attempt| checkpoint.with_resume_attempt(attempt))
                            .ok_or_else(|| {
                                "turn checkpoint resume attempt is exhausted".to_string()
                            })
                    })
                    .map_err(|_| AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Serialization,
                    })?;
                    Ok((Some(checkpoint), input_ids, boundary.pause_requested))
                };
            let (recorded, handoff_checkpoint) = loop {
                let (handoff_checkpoint, handoff_input_ids, handoff_pause) = build_handoff()?;
                let encoded_handoff = handoff_checkpoint
                    .as_ref()
                    .map(TurnCheckpoint::encode)
                    .transpose()
                    .map_err(|_| AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Serialization,
                    })?;
                let handoff_checkpoint_version = handoff_checkpoint
                    .as_ref()
                    .map(TurnCheckpoint::checkpoint_version);
                let recorded_result = if let (Some(checkpoint), Some(checkpoint_version)) =
                    (encoded_handoff.as_ref(), handoff_checkpoint_version)
                {
                    self.store.record_approval_decision_with_turn_checkpoint(
                        &decision,
                        "approval",
                        "approval decision recorded",
                        &handoff_input_ids,
                        checkpoint,
                        checkpoint_version,
                        handoff_pause,
                    )
                } else {
                    self.store.record_approval_decision(
                        &decision,
                        "approval",
                        "approval decision recorded",
                    )
                };
                match recorded_result {
                    Ok(recorded) => break (recorded, handoff_checkpoint),
                    Err(StoreError::TurnBoundaryPending { .. }) if continues_execution => continue,
                    Err(StoreError::TurnBoundaryPending { .. })
                        if decision.outcome == ApprovalOutcome::Deny =>
                    {
                        continue 'decision_attempt;
                    }
                    Err(error) => {
                        let response = match error {
                            StoreError::NotFound(_) => not_found_response(
                                message.required_id(),
                                PENDING_APPROVAL_NOT_FOUND,
                            )?,
                            StoreError::InvalidState(state_message)
                                if state_message
                                    == "pending approval allow requires an active thread" =>
                            {
                                invalid_state_response(
                                    message.required_id(),
                                    THREAD_ARCHIVED_CONTINUATION,
                                )?
                            }
                            StoreError::WorkspaceHasNonterminalTurn { turn_id, .. } => {
                                invalid_state_response(
                                    message.required_id(),
                                    format!(
                                        "workspace already has an active or pending turn {turn_id}; use sg turn resume/pause/input {turn_id}"
                                    ),
                                )?
                            }
                            StoreError::TurnBoundaryPending { .. } => invalid_state_response(
                                message.required_id(),
                                "turn has pending interactive input at the approval boundary",
                            )?,
                            other => return Err(other.into()),
                        };
                        emit_messages(&mut emit, response);
                        return Ok(());
                    }
                }
            };
            let pending_approval = match decode_pending_approval(
                &recorded.request,
                recorded.pending_tool_call.as_ref(),
            ) {
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
            if matches!(decision.outcome, ApprovalOutcome::Deny) && handoff_checkpoint.is_none() {
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
            if let Some(checkpoint) = handoff_checkpoint {
                if recorded.turn.status == TurnStatus::Paused {
                    emit(approval_decision_response(
                        message.required_id(),
                        &decision,
                    )?);
                    return Ok(());
                }
                let mut assistant_events =
                    AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
                let assistant_item_id = assistant_events.item_id.clone();
                let cancellation = active_turn
                    .as_ref()
                    .map(|(cancellation, _guard)| cancellation.clone())
                    .unwrap_or_default();
                let user_input = self.store.get_turn_user_input(&recorded.turn.turn_id)?;
                let params = TurnStartParams {
                    thread_id: pending_thread.thread_id.clone(),
                    input: serde_json::from_value(user_input)?,
                };
                let history = self.store.read_thread_history_before_turn(
                    &pending_thread.thread_id,
                    &recorded.turn.turn_id,
                    DEFAULT_THREAD_HISTORY_TURN_LIMIT,
                )?;
                let invocation = AgentLoopInvocation {
                    thread: &pending_thread,
                    params: &params,
                    turn_id: &recorded.turn.turn_id,
                    history: &history.messages,
                    cancellation: &cancellation,
                    monitor_control: active_turn
                        .as_ref()
                        .and_then(|(_, guard)| guard.monitor_control()),
                };
                let workspace_tools =
                    continuation_workspace.ok_or_else(|| AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Internal,
                    })?;
                let mut continuation_event_failed = false;
                let mut on_event = |event: AgentLoopEvent| match event {
                    AgentLoopEvent::FinalTextDelta { delta } => {
                        match self.project_assistant_delta(&mut assistant_events, &delta) {
                            Ok(messages) => emit_messages(&mut emit, messages),
                            Err(error) => {
                                continuation_event_failed = true;
                                return Err(error);
                            }
                        }
                        Ok(())
                    }
                    AgentLoopEvent::Observation(_) => Ok(()),
                };
                let status = if let Some(provider) = &self.test_provider_override {
                    self.run_resumed_agent_loop_with_provider_and_tools(
                        Arc::clone(provider),
                        invocation,
                        workspace_tools,
                        &checkpoint,
                        &mut on_event,
                        true,
                    )
                } else {
                    let provider = self.provider_for_thread(invocation.thread).map_err(|_| {
                        AppServerError::TurnExecution {
                            stage: TurnFailureStage::AgentLoop,
                            cause: TurnFailureCause::Internal,
                        }
                    })?;
                    self.run_resumed_agent_loop_with_provider_and_tools(
                        provider,
                        invocation,
                        workspace_tools,
                        &checkpoint,
                        &mut on_event,
                        true,
                    )
                };
                let monitor_outcome = active_turn
                    .as_mut()
                    .and_then(|(_, guard)| guard.stabilize_monitor(&cancellation));
                let mut status = match status {
                    Ok(status) => status,
                    Err(error) => {
                        self.finish_turn_failure(
                            &mut emit,
                            &recorded.turn,
                            Some(&assistant_events),
                            &cancellation,
                            monitor_outcome,
                            monitor_failure_or(
                                monitor_outcome,
                                turn_failure_from_error(
                                    &error,
                                    if continuation_event_failed {
                                        TurnFailureStage::EventNotification
                                    } else {
                                        TurnFailureStage::ApprovalCheckpoint
                                    },
                                ),
                            ),
                        )?;
                        emit(approval_decision_response(
                            message.required_id(),
                            &decision,
                        )?);
                        return Ok(());
                    }
                };
                let committed = loop {
                    match self.commit_turn_run_status(
                        recorded.turn.clone(),
                        &status,
                        Some(&assistant_item_id),
                        &cancellation,
                        monitor_outcome,
                    ) {
                        Ok(committed) => break committed,
                        Err(AppServerError::Store(StoreError::TurnBoundaryPending { .. })) => {
                            status = self.resume_pending_terminal_boundary(
                                AgentLoopInvocation {
                                    thread: &pending_thread,
                                    params: &params,
                                    turn_id: &recorded.turn.turn_id,
                                    history: &history.messages,
                                    cancellation: &cancellation,
                                    monitor_control: active_turn
                                        .as_ref()
                                        .and_then(|(_, guard)| guard.monitor_control()),
                                },
                                &mut on_event,
                                true,
                            )?;
                        }
                        Err(error) => return Err(error),
                    }
                };
                emit_messages(
                    &mut emit,
                    self.committed_turn_events(&committed, Some(&assistant_events))?,
                );
                if status.status == AgentStatus::Blocked {
                    emit_messages(
                        &mut emit,
                        self.pending_approval_events_for_turn(&recorded.turn.turn_id)?,
                    );
                }
                emit(approval_decision_response(
                    message.required_id(),
                    &decision,
                )?);
                return Ok(());
            }
            let mut assistant_events = continues_execution
                .then(|| AssistantItemEventState::new(SessionStore::allocate_assistant_item_id()));
            let mut continuation_event_failed = false;
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
                        &mut |event| match event {
                            AgentLoopEvent::FinalTextDelta { delta } => {
                                let Some(assistant_events) = assistant_events.as_mut() else {
                                    return Ok(());
                                };
                                let messages =
                                    match self.project_assistant_delta(assistant_events, &delta) {
                                        Ok(messages) => messages,
                                        Err(error) => {
                                            continuation_event_failed = true;
                                            return Err(error);
                                        }
                                    };
                                emit_messages(&mut emit, messages);
                                Ok(())
                            }
                            AgentLoopEvent::Observation(_) => Ok(()),
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
                                self.emit_realtime_item_failure(
                                    &mut emit,
                                    assistant_events.as_ref(),
                                )?;
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
                                self.emit_realtime_item_failure(
                                    &mut emit,
                                    assistant_events.as_ref(),
                                )?;
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
            return Ok(());
        }
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
