//! AppServer construction, turn supervision, cancellation, and shutdown.

use super::*;

impl AppServer {
    /// 使用平台沙箱和已捕获的模型提供方配置快照创建未初始化的服务。
    pub fn new(store: SessionStore, provider_snapshot: ProviderConfigSnapshot) -> Self {
        Self {
            store,
            initialized: false,
            initialized_acknowledged: false,
            event_filter: Arc::new(Mutex::new(EventSubscriptionState::default())),
            shutdown_requested: false,
            sandbox_backend: Arc::new(PlatformSandboxBackend::new()),
            provider_snapshot,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
            evaluation_cancellation: CancellationToken::new(),
            output_order: OutputOrderCoordinator::new(),
        }
    }

    /// 替换服务使用的 sandbox backend。
    pub fn with_sandbox_backend(
        mut self,
        sandbox_backend: impl SandboxBackend + Send + Sync + 'static,
    ) -> Self {
        self.sandbox_backend = Arc::new(sandbox_backend);
        self
    }

    /// 判断服务是否已收到 shutdown 请求。
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// 判断初始化握手是否允许启动 turn worker。
    pub fn ready_for_turn_worker(&self) -> bool {
        self.initialized_acknowledged
    }

    /// 请求当前进程所有执行停止。
    pub fn request_execution_stop(&self) -> AppServerResult<()> {
        self.cancellation_handle().request_execution_stop()
    }

    /// 返回共享的执行取消句柄。
    pub fn cancellation_handle(&self) -> AppServerCancellationHandle {
        AppServerCancellationHandle {
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
            evaluation_cancellation: self.evaluation_cancellation.clone(),
        }
    }

    /// 返回当前 app-server 生命周期共享的 stdout reservation 协调器。
    pub fn output_order_coordinator(&self) -> OutputOrderCoordinator {
        self.output_order.clone()
    }

    /// 为请求工作线程打开独立的存储连接，同时共享停止和事件订阅状态。
    pub fn turn_worker(&self) -> AppServerResult<Self> {
        Ok(Self {
            store: self.store.trusted_reopen()?,
            initialized: true,
            initialized_acknowledged: true,
            event_filter: Arc::clone(&self.event_filter),
            shutdown_requested: false,
            sandbox_backend: Arc::clone(&self.sandbox_backend),
            provider_snapshot: self.provider_snapshot.clone(),
            active_turns: Arc::clone(&self.active_turns),
            execution_stopped: Arc::clone(&self.execution_stopped),
            evaluation_cancellation: self.evaluation_cancellation.clone(),
            output_order: self.output_order.clone(),
        })
    }

    /// 注册一个活动 turn，并为其附加持久化取消监视器。
    pub(super) fn activate_turn(
        &self,
        turn_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let (cancellation, guard) = self.prepare_turn_activation(turn_id)?;
        guard.start_monitor();
        Ok((cancellation, guard))
    }

    // Establish every fallible runtime resource before a new running Turn is
    // committed. The monitor remains paused until the caller starts it after commit.
    pub(super) fn prepare_turn_activation(
        &self,
        turn_id: &str,
    ) -> AppServerResult<(CancellationToken, ActiveTurnGuard)> {
        let cancellation = CancellationToken::new();
        // Open the fallible monitor connection before publishing the registry entry.
        let monitor_store = if self.store.descriptor().path == ":memory:" {
            None
        } else {
            Some(self.store.trusted_reopen()?)
        };
        let mut active_turns = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        if active_turns.contains_key(turn_id) {
            return Err(AppServerError::Workspace(format!(
                "turn {turn_id} is already active"
            )));
        }
        let monitor = cancellation_monitor(monitor_store, turn_id, cancellation.clone())?;
        if self.execution_stopped.load(Ordering::SeqCst) {
            cancellation.cancel();
        }
        active_turns.insert(turn_id.to_string(), cancellation.clone());
        drop(active_turns);
        let guard = ActiveTurnGuard {
            turn_id: turn_id.to_string(),
            active_turns: Arc::clone(&self.active_turns),
            cancellation: cancellation.clone(),
            monitor,
            stabilized_monitor_outcome: None,
        };
        Ok((cancellation, guard))
    }

    pub(super) fn refresh_turn_if_unowned(&self, turn: Turn) -> AppServerResult<Turn> {
        if is_terminal_turn_status(&turn.status) {
            return Ok(turn);
        }
        let Some(_execution_guard) = self.store.try_begin_workspace_execution(&turn.thread_id)?
        else {
            return Ok(turn);
        };
        self.store.get_turn(&turn.turn_id).map_err(Into::into)
    }

    pub(super) fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        self.handle_turn_start_streaming_values(message, |message| messages.push(message))?;
        Ok(messages)
    }

    /// 执行 `turn/start`，并在每个阶段完成时返回已预留顺序的输出。
    pub fn handle_turn_start_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let result = self.handle_turn_start_streaming_values(message, |message| {
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

    /// 执行 `turn/start` 并返回未携带 transport 顺序的兼容消息。
    pub fn handle_turn_start_streaming(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let result = self.handle_turn_start_streaming_values(message, |message| {
            if sequencing_error.is_some() {
                return;
            }
            match sequence_output(&coordinator, message) {
                Ok(output) => {
                    coordinator.complete(output.reservation.order);
                    emit(output.message);
                }
                Err(error) => sequencing_error = Some(error),
            }
        });
        if let Some(error) = sequencing_error {
            return Err(error);
        }
        result
    }

    /// 执行 `turn/start`，并在每个持久化阶段完成时发出生命周期事件。
    pub(super) fn handle_turn_start_streaming_values(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        if message.method_name() != Some(Method::TurnStart.as_str()) {
            return Err(AppServerError::InvalidParams(
                "streaming handler requires turn/start".to_string(),
            ));
        }
        let params: TurnStartParams = parse_params(&message)?;
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), THREAD_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if thread.status != singularity_protocol::ThreadStatus::Active {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), THREAD_ARCHIVED)?,
            );
            return Ok(());
        }
        let workspace_tools =
            match workspace_tools_for_thread(&thread, Arc::clone(&self.sandbox_backend)) {
                Ok(tools) => tools,
                Err(error) => {
                    emit_messages(
                        &mut emit,
                        invalid_state_response(message.required_id(), error)?,
                    );
                    return Ok(());
                }
            };
        let capability = agent_loop_capability(self.sandbox_backend.as_ref());
        if !agent_loop_capability_ready(&capability) {
            emit_messages(
                &mut emit,
                invalid_state_response(
                    message.required_id(),
                    agent_loop_unavailable_message(&capability),
                )?,
            );
            return Ok(());
        }
        let Some(_execution_guard) = self
            .store
            .try_begin_workspace_execution(&params.thread_id)?
        else {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?,
            );
            return Ok(());
        };
        let payload = serde_json::to_value(&params.input)?;
        let allocated_turn_id = SessionStore::allocate_turn_id();
        let (cancellation, mut active_turn) =
            self.prepare_turn_activation(allocated_turn_id.as_str())?;
        let started = match self
            .store
            .create_allocated_turn_with_input_trace_and_history(
                allocated_turn_id,
                CreateStartedTurnParams {
                    thread_id: &params.thread_id,
                    agent_loop_status: AgentStatus::Running.as_str(),
                    input: payload,
                    component: "app_server",
                    summary: "turn started",
                    history_turn_limit: DEFAULT_THREAD_HISTORY_TURN_LIMIT,
                },
            ) {
            Ok(result) => result,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), THREAD_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(StoreError::WorkspaceHasNonterminalTurn { .. }) => {
                emit_messages(
                    &mut emit,
                    invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let turn = started.turn;
        active_turn.start_monitor();
        match self.event_notification(AppEvent::turn_started(&turn)) {
            Ok(Some(event)) => emit(event),
            Ok(None) => {}
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &turn,
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(
                        monitor_outcome,
                        turn_failure_from_error(&error, TurnFailureStage::EventNotification),
                    ),
                );
            }
        }
        let status = match self.run_agent_loop(
            AgentLoopInvocation {
                thread: &thread,
                params: &params,
                turn_id: &turn.turn_id,
                history: &started.history.messages,
                cancellation: &cancellation,
                monitor_control: active_turn.monitor_control(),
            },
            workspace_tools,
        ) {
            Ok(status) => status,
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &turn,
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(
                        monitor_outcome,
                        turn_failure_from_error(&error, TurnFailureStage::AgentLoop),
                    ),
                );
            }
        };
        let approval_events = match self.pending_approval_events_for_turn(&turn.turn_id) {
            Ok(events) => events,
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &turn,
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(
                        monitor_outcome,
                        turn_failure_from_error(&error, TurnFailureStage::EventNotification),
                    ),
                );
            }
        };
        emit_messages(&mut emit, approval_events);
        let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
        if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
            return self.finish_turn_failure(
                &mut emit,
                &turn,
                &cancellation,
                monitor_outcome,
                TurnFailure {
                    stage: TurnFailureStage::CancellationMonitor,
                    cause: TurnFailureCause::CancellationMonitor,
                },
            );
        }
        let committed = match self.commit_turn_run_status(
            turn.clone(),
            &status,
            &cancellation,
            monitor_outcome,
        ) {
            Ok(committed) => committed,
            Err(error) => {
                return self.finish_turn_failure(
                    &mut emit,
                    &turn,
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(
                        monitor_outcome,
                        TurnFailure {
                            stage: TurnFailureStage::TerminalOutcome,
                            cause: turn_failure_cause(&error),
                        },
                    ),
                );
            }
        };
        let terminal_events = self.committed_turn_events(&committed)?;
        let turn = committed.turn;
        emit_messages(&mut emit, terminal_events);
        emit(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnStartResult { turn })?,
            )
            .to_wire_value(),
        );
        Ok(())
    }

    pub(super) fn finish_turn_failure(
        &self,
        emit: &mut impl FnMut(Value),
        turn: &Turn,
        cancellation: &CancellationToken,
        monitor_outcome: Option<CancellationMonitorOutcome>,
        failure: impl Into<TurnFailure>,
    ) -> AppServerResult<()> {
        let failure = failure.into();
        match self.terminalize_turn_failure(turn, cancellation, monitor_outcome, failure) {
            Ok(TurnTerminalizationResult::Committed(committed)) => {
                match self.committed_turn_events(&committed) {
                    Ok(events) => emit_messages(emit, events),
                    Err(_) => {
                        return Err(AppServerError::TurnTerminalization {
                            stage: failure.stage,
                            cause: failure.cause,
                            failure: TurnTerminalizationFailure::EventNotification,
                        });
                    }
                }
                Err(AppServerError::TurnExecution {
                    stage: failure.stage,
                    cause: failure.cause,
                })
            }
            Ok(TurnTerminalizationResult::Preserved) => Err(AppServerError::TurnExecution {
                stage: failure.stage,
                cause: failure.cause,
            }),
            Err(cleanup_failure) => Err(AppServerError::TurnTerminalization {
                stage: failure.stage,
                cause: failure.cause,
                failure: cleanup_failure,
            }),
        }
    }

    /// 将已进入 Running 的执行错误收敛为安全终态，保留并发提交的 Blocked/终态。
    pub(super) fn terminalize_turn_failure(
        &self,
        turn: &Turn,
        cancellation: &CancellationToken,
        monitor_outcome: Option<CancellationMonitorOutcome>,
        failure: impl Into<TurnFailure>,
    ) -> Result<TurnTerminalizationResult, TurnTerminalizationFailure> {
        let failure = failure.into();
        let current = self
            .store
            .get_turn(&turn.turn_id)
            .map_err(|_| TurnTerminalizationFailure::Store)?;
        if is_safe_turn_state(&current) {
            return Ok(TurnTerminalizationResult::Preserved);
        }

        let user_cancelled = monitor_outcome
            != Some(CancellationMonitorOutcome::InfrastructureFailure)
            && (current.agent_loop_status == AgentStatus::CancelRequested.as_str()
                || cancellation.is_cancelled());
        let status = if user_cancelled {
            let mut status = AgentRunStatus::failed("turn interrupted by user request");
            mark_run_cancelled(&mut status);
            status
        } else {
            failed_turn_status(failure)
        };
        let authority =
            if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
                TurnOutcomeAuthority::InfrastructureFailure
            } else {
                TurnOutcomeAuthority::AgentLoop
            };
        match self.commit_effective_turn_status_with_authority(&current, &status, authority) {
            Ok(committed) => Ok(TurnTerminalizationResult::Committed(Box::new(committed))),
            Err(_) => {
                let latest = self
                    .store
                    .get_turn(&turn.turn_id)
                    .map_err(|_| TurnTerminalizationFailure::Store)?;
                if is_safe_turn_state(&latest) {
                    Ok(TurnTerminalizationResult::Preserved)
                } else if latest.agent_loop_status == AgentStatus::CancelRequested.as_str()
                    && monitor_outcome != Some(CancellationMonitorOutcome::InfrastructureFailure)
                {
                    let mut interrupted =
                        AgentRunStatus::failed("turn interrupted by user request");
                    mark_run_cancelled(&mut interrupted);
                    match self.commit_effective_turn_status(&latest, &interrupted) {
                        Ok(committed) => {
                            Ok(TurnTerminalizationResult::Committed(Box::new(committed)))
                        }
                        Err(_) => {
                            let latest = self
                                .store
                                .get_turn(&turn.turn_id)
                                .map_err(|_| TurnTerminalizationFailure::Store)?;
                            if is_safe_turn_state(&latest) {
                                Ok(TurnTerminalizationResult::Preserved)
                            } else {
                                Err(TurnTerminalizationFailure::Store)
                            }
                        }
                    }
                } else if latest.status != current.status
                    || latest.agent_loop_status != current.agent_loop_status
                {
                    Err(TurnTerminalizationFailure::StateChanged)
                } else {
                    Err(TurnTerminalizationFailure::Store)
                }
            }
        }
    }

    pub(super) fn agent_capability(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let capability = agent_loop_capability(self.sandbox_backend.as_ref());
        json_response(
            message.required_id(),
            AgentCapabilityResult {
                agent_loop: AgentLoopCapabilityStatus {
                    available: capability.available,
                    status: capability.status.as_str().to_string(),
                    reason: capability.reason,
                    blockers: capability.blockers,
                },
                provider_configuration: provider_configuration(&self.provider_snapshot),
            },
        )
    }

    pub(super) fn eval_run(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: EvalRunParams = parse_params(&message)?;
        match evaluation::run_evaluation(
            &params,
            Arc::clone(&self.sandbox_backend),
            &self.provider_snapshot,
            &self.evaluation_cancellation,
        ) {
            Ok(result) => json_response(message.required_id(), result),
            Err(error) => match error.kind() {
                evaluation::EvaluationRunErrorKind::Input => json_error(
                    Some(message.required_id()),
                    ErrorCode::invalid_params("Invalid params"),
                ),
                evaluation::EvaluationRunErrorKind::Publication
                | evaluation::EvaluationRunErrorKind::Infrastructure => json_error(
                    Some(message.required_id()),
                    ErrorCode::new(JSON_RPC_INTERNAL_ERROR, "Internal error"),
                ),
                evaluation::EvaluationRunErrorKind::Cancelled => {
                    if let Some(partial) = error.partial_result() {
                        json_response(message.required_id(), partial.clone())
                    } else {
                        json_error(
                            Some(message.required_id()),
                            ErrorCode::new(JSON_RPC_INTERNAL_ERROR, "Internal error"),
                        )
                    }
                }
            },
        }
    }

    /// 根据已捕获的模型提供方、工作区策略和持久化历史构建 `AgentLoop`。
    pub(super) fn run_agent_loop(
        &self,
        invocation: AgentLoopInvocation<'_>,
        workspace_tools: WorkspaceTools,
    ) -> AppServerResult<AgentRunStatus> {
        let provider = match self.provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                let category = error.error.category();
                let mut status = safe_failed_agent_status(SAFE_PROVIDER_FAILURE, "provider");
                status.error_category = Some(category);
                return Ok(status);
            }
        };
        match self.run_agent_loop_with_provider_and_tools(provider, invocation, workspace_tools) {
            Err(AppServerError::ProjectInstructions(_)) => Ok(safe_failed_agent_status(
                SAFE_PROJECT_INSTRUCTIONS_FAILURE,
                "project_instructions",
            )),
            Err(AppServerError::Workspace(_)) => Ok(safe_failed_agent_status(
                SAFE_WORKSPACE_FAILURE,
                "workspace",
            )),
            result => result,
        }
    }

    /// 仅当存储与 turn 仍满足其契约时恢复已批准的检查点。
    pub(super) fn resume_agent_loop(
        &self,
        input: ApprovalResumeInput<'_>,
        context: ApprovalResumeContext<'_>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus, Vec<PendingApprovalOccurrence>)>> {
        let ApprovalResumeInput {
            request,
            decision,
            turn,
            thread,
            pending_approval,
        } = input;
        if monitor_infrastructure_failure(context.monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        if !agent_loop_ready(self.sandbox_backend.as_ref()) {
            return Ok(None);
        }
        if !matches!(decision.outcome, ApprovalOutcome::Allow) {
            return Ok(None);
        }
        if pending_approval.is_none() {
            return Ok(None);
        }
        let provider = match self.provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                let category = error.error.category();
                let mut run_status = approval_terminal_status(
                    thread,
                    decision,
                    pending_approval.as_ref(),
                    AgentStatus::Failed,
                    "unavailable",
                    SAFE_PROVIDER_FAILURE,
                );
                run_status.error_category = Some(category);
                return Ok(Some((turn.clone(), run_status, Vec::new())));
            }
        };
        self.resume_agent_loop_after_gate_with_monitor(
            ApprovalResumeInput {
                request,
                decision,
                turn,
                thread,
                pending_approval,
            },
            provider,
            context,
        )
    }

    /// 重建规范化的 loop 输入，并执行一个已批准的待执行调用。
    #[cfg(test)]
    pub(super) fn resume_agent_loop_after_gate<P>(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        provider: P,
        cancellation: &CancellationToken,
        prepared_workspace_tools: Option<WorkspaceTools>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus, Vec<PendingApprovalOccurrence>)>>
    where
        P: Provider,
    {
        let turn = self.store.get_turn(&request.turn_id)?;
        let thread = self.store.get_thread(&turn.thread_id)?;
        let pending_approval = match pending_tool_call.as_ref() {
            Some(payload) => decode_pending_approval(request, Some(payload))?,
            None => None,
        };
        self.resume_agent_loop_after_gate_with_monitor(
            ApprovalResumeInput {
                request,
                decision,
                turn: &turn,
                thread: &thread,
                pending_approval,
            },
            provider,
            ApprovalResumeContext {
                cancellation,
                monitor_control: None,
                prepared_workspace_tools,
            },
        )
    }

    pub(super) fn resume_agent_loop_after_gate_with_monitor<P>(
        &self,
        input: ApprovalResumeInput<'_>,
        provider: P,
        context: ApprovalResumeContext<'_>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus, Vec<PendingApprovalOccurrence>)>>
    where
        P: Provider,
    {
        let ApprovalResumeInput {
            request,
            decision,
            turn,
            thread,
            pending_approval,
        } = input;
        if monitor_infrastructure_failure(context.monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        if !matches!(decision.outcome, ApprovalOutcome::Allow) {
            return Ok(None);
        }
        if turn.status != TurnStatus::Blocked
            || turn.agent_loop_status != AgentStatus::Blocked.as_str()
        {
            return Ok(None);
        }
        let Some(pending_approval) = pending_approval else {
            return Ok(None);
        };
        if pending_approval.request().request_id != request.request_id {
            let run_status = approval_terminal_status(
                thread,
                decision,
                Some(&pending_approval),
                AgentStatus::Failed,
                "unavailable",
                "pending approval request mismatch",
            );
            return Ok(Some((turn.clone(), run_status, Vec::new())));
        }
        if thread.status != singularity_protocol::ThreadStatus::Active {
            return Ok(None);
        }
        let workspace_tools = match context.prepared_workspace_tools {
            Some(workspace_tools) => workspace_tools,
            None => {
                let run_status = approval_terminal_status(
                    thread,
                    decision,
                    Some(&pending_approval),
                    AgentStatus::Failed,
                    "unavailable",
                    "workspace capability was not prepared",
                );
                return Ok(Some((turn.clone(), run_status, Vec::new())));
            }
        };
        let workspace_root = workspace_tools.workspace_root().to_path_buf();
        let user_input = self.store.get_turn_user_input(&turn.turn_id).map_err(|_| {
            AppServerError::TurnExecution {
                stage: TurnFailureStage::ApprovalCheckpoint,
                cause: TurnFailureCause::StoredInputUnavailable,
            }
        })?;
        let params = TurnStartParams {
            thread_id: turn.thread_id.clone(),
            input: serde_json::from_value(user_input)?,
        };
        let grant = ApprovalGrant::allow(
            request.request_id.clone(),
            request.action.clone(),
            request.resources.clone(),
        );
        let history = self.store.read_thread_history_before_turn(
            &thread.thread_id,
            &turn.turn_id,
            DEFAULT_THREAD_HISTORY_TURN_LIMIT,
        )?;
        let registry = workspace_tool_registry();
        let policy = workspace_policy(thread.sandbox_mode, thread.approval_policy);
        let loop_input = match agent_loop_input(
            thread,
            &params,
            &turn.turn_id,
            &workspace_root,
            &history.messages,
        ) {
            Ok(input) => input.with_approval_grant(grant),
            Err(_error) => {
                let run_status = approval_terminal_status(
                    thread,
                    decision,
                    Some(&pending_approval),
                    AgentStatus::Failed,
                    "unavailable",
                    SAFE_PROJECT_INSTRUCTIONS_FAILURE,
                );
                return Ok(Some((turn.clone(), run_status, Vec::new())));
            }
        };
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools)
            .with_cancellation_token(context.cancellation.clone())
            .resume_pending_approval(&loop_input, &pending_approval);
        if monitor_infrastructure_failure(context.monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        let mut run_status = result.to_run_status();
        sanitize_agent_run_status_error(&mut run_status);
        let next_approvals = result.pending_approvals.clone();
        if run_status.audit_events.is_empty()
            && pending_approval.pending_tool_call().tool_name.as_str() == TOOL_COMMAND
        {
            let audit_status = approval_terminal_status(
                thread,
                decision,
                Some(&pending_approval),
                run_status.status.clone(),
                "unavailable",
                run_status
                    .error
                    .clone()
                    .unwrap_or_else(|| "approval resume did not execute command".to_string()),
            );
            run_status.audit_events = audit_status.audit_events;
        }
        if monitor_infrastructure_failure(context.monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        Ok(Some((turn.clone(), run_status, next_approvals)))
    }

    #[cfg(test)]
    pub(super) fn run_agent_loop_with_provider<P>(
        &self,
        provider: P,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
        history: &[ConversationMessage],
        cancellation: &CancellationToken,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider,
    {
        let workspace_tools = workspace_tools_for_thread(thread, Arc::clone(&self.sandbox_backend))
            .map_err(AppServerError::Workspace)?;
        let invocation = AgentLoopInvocation {
            thread,
            params,
            turn_id,
            history,
            cancellation,
            monitor_control: None,
        };
        self.run_agent_loop_with_provider_and_tools(provider, invocation, workspace_tools)
    }

    pub(super) fn run_agent_loop_with_provider_and_tools<P>(
        &self,
        provider: P,
        invocation: AgentLoopInvocation<'_>,
        workspace_tools: WorkspaceTools,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider,
    {
        let registry = workspace_tool_registry();
        let workspace_root = workspace_tools.workspace_root().to_path_buf();
        let policy = workspace_policy(
            invocation.thread.sandbox_mode,
            invocation.thread.approval_policy,
        );
        let loop_input = agent_loop_input(
            invocation.thread,
            invocation.params,
            invocation.turn_id,
            &workspace_root,
            invocation.history,
        )?;
        let result = AgentLoop::new(provider, ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools)
            .with_cancellation_token(invocation.cancellation.clone())
            .run(&loop_input);
        let mut run_status = result.to_run_status();
        sanitize_agent_run_status_error(&mut run_status);
        if monitor_infrastructure_failure(invocation.monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        if invocation.cancellation.is_cancelled() {
            mark_run_cancelled(&mut run_status);
            return Ok(run_status);
        }
        match self.persist_agent_approval_requests(&result, invocation.monitor_control) {
            Ok(()) => {
                if monitor_infrastructure_failure(invocation.monitor_control) {
                    Err(AppServerError::TurnExecution {
                        stage: TurnFailureStage::CancellationMonitor,
                        cause: TurnFailureCause::CancellationMonitor,
                    })
                } else {
                    Ok(run_status)
                }
            }
            Err(AppServerError::Store(_)) => {
                let turn = self.store.get_turn(invocation.turn_id).map_err(|_| {
                    AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Store,
                    }
                })?;
                if turn.agent_loop_status == AgentStatus::CancelRequested.as_str()
                    || turn.status == TurnStatus::Interrupted
                {
                    mark_run_cancelled(&mut run_status);
                    Ok(run_status)
                } else {
                    Err(AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Store,
                    })
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 在向客户端暴露阻塞 turn 前持久化每个 `AgentLoop` 检查点。
    pub(super) fn persist_agent_approval_requests(
        &self,
        result: &AgentLoopResult,
        monitor_control: Option<&CancellationMonitorControl>,
    ) -> AppServerResult<()> {
        if monitor_infrastructure_failure(monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        let encoded_checkpoints = encode_pending_approvals(&result.pending_approvals)?;
        if monitor_infrastructure_failure(monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        self.store
            .create_approval_batch_with_pending_tool_calls_and_trace(
                &encoded_checkpoints,
                "approval",
                "approval requested",
            )?;
        if monitor_infrastructure_failure(monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        Ok(())
    }

    /// 将运行状态映射为持久化 turn 状态，并在提交时让取消优先。
    pub(super) fn commit_turn_run_status(
        &self,
        turn: Turn,
        run_status: &AgentRunStatus,
        cancellation: &CancellationToken,
        monitor_outcome: Option<CancellationMonitorOutcome>,
    ) -> AppServerResult<CommittedTurnOutcome> {
        let current = self.store.get_turn(&turn.turn_id)?;
        if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        let mut effective_status = run_status.clone();
        if monitor_outcome == Some(CancellationMonitorOutcome::UserCancellation)
            || cancellation.is_cancelled()
            || current.agent_loop_status == AgentStatus::CancelRequested.as_str()
            || (current.status == TurnStatus::Interrupted
                && current.agent_loop_status == AgentStatus::Cancelled.as_str())
        {
            mark_run_cancelled(&mut effective_status);
        }
        if current.status == TurnStatus::Blocked
            && current.agent_loop_status == AgentStatus::Blocked.as_str()
            && effective_status.status != AgentStatus::Blocked
        {
            return Err(StoreError::InvalidState(
                "turn state changed to blocked before terminal commit".to_string(),
            )
            .into());
        }
        self.commit_effective_turn_status(&turn, &effective_status)
            .map_err(Into::into)
    }

    pub(super) fn commit_effective_turn_status(
        &self,
        turn: &Turn,
        run_status: &AgentRunStatus,
    ) -> Result<CommittedTurnOutcome, StoreError> {
        self.commit_effective_turn_status_with_authority(
            turn,
            run_status,
            TurnOutcomeAuthority::AgentLoop,
        )
    }

    pub(super) fn commit_effective_turn_status_with_authority(
        &self,
        turn: &Turn,
        run_status: &AgentRunStatus,
        authority: TurnOutcomeAuthority,
    ) -> Result<CommittedTurnOutcome, StoreError> {
        let assistant_delta = agent_completed_delta(run_status);
        let plan = run_status
            .plan
            .as_ref()
            .map(serde_json::to_value)
            .transpose()?;
        let event = agent_loop_trace(turn, run_status);
        self.store.commit_turn_outcome_with_authority(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: turn_status_for_agent(&run_status.status),
                agent_loop_status: run_status.status.as_str(),
                assistant_delta: assistant_delta.as_deref(),
                plan: plan.as_ref(),
                trace: &event,
            },
            authority,
        )
    }

    /// 在一个存储事务中提交 approval 续行状态及后续检查点（如有）。
    pub(super) fn commit_effective_turn_status_resolving_approval(
        &self,
        request_id: &str,
        turn: &Turn,
        run_status: &AgentRunStatus,
        next_approvals: &[PendingApprovalOccurrence],
        monitor_outcome: Option<CancellationMonitorOutcome>,
    ) -> Result<CommittedTurnOutcome, StoreError> {
        let mut effective_status = run_status.clone();
        let commit = |status: &AgentRunStatus| {
            let assistant_delta = agent_completed_delta(status);
            let plan = status.plan.as_ref().map(serde_json::to_value).transpose()?;
            let event = agent_loop_trace(turn, status);
            let effective_next_approvals = if status.status == AgentStatus::Blocked {
                encode_pending_approvals(next_approvals)?
            } else {
                Vec::new()
            };
            let authority =
                if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
                    TurnOutcomeAuthority::InfrastructureFailure
                } else {
                    TurnOutcomeAuthority::AgentLoop
                };
            self.store
                .commit_turn_outcome_and_resolve_pending_execution_with_authority(
                    request_id,
                    CommitTurnOutcomeParams {
                        status: turn_status_for_agent(&status.status),
                        agent_loop_status: status.status.as_str(),
                        assistant_delta: assistant_delta.as_deref(),
                        plan: plan.as_ref(),
                        trace: &event,
                    },
                    &effective_next_approvals,
                    authority,
                )
        };
        match commit(&effective_status) {
            Ok(committed) => Ok(committed),
            Err(error) => {
                let current = self.store.get_turn(&turn.turn_id)?;
                if monitor_outcome != Some(CancellationMonitorOutcome::InfrastructureFailure)
                    && current.agent_loop_status == AgentStatus::CancelRequested.as_str()
                {
                    mark_run_cancelled(&mut effective_status);
                    commit(&effective_status)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(super) fn turn_interrupt(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        let turn = match self.store.get_turn(&params.turn_id) {
            Ok(turn) => self.refresh_turn_if_unowned(turn)?,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if is_terminal_turn_status(&turn.status) {
            return Ok(vec![
                JsonRpcMessage::response(
                    message.required_id(),
                    serde_json::to_value(TurnInterruptResult {
                        status: turn.status.as_storage_text().to_string(),
                        turn_id: turn.turn_id,
                        agent_loop_status: Some(turn.agent_loop_status),
                    })?,
                )
                .to_wire_value(),
            ]);
        }
        let thread_id = turn.thread_id.clone();
        if let Some(cancellation) = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&turn.turn_id)
            .cloned()
        {
            cancellation.cancel();
        }
        let trace = TraceEvent {
            payload: json!({
                "turn_id": turn.turn_id,
                "agent_loop_status": AgentStatus::CancelRequested.as_str(),
            }),
            ..TraceEvent::for_turn(
                format!("trace_{}_interrupt_requested", turn.turn_id),
                thread_id,
                turn.turn_id.clone(),
                "app_server",
                "turn interrupt requested",
            )
        };
        let turn = self
            .store
            .request_turn_cancellation(&turn.turn_id, &trace)?;
        let status = if turn.agent_loop_status == AgentStatus::CancelRequested.as_str() {
            AgentStatus::CancelRequested.as_str()
        } else {
            turn.status.as_storage_text()
        };
        let mut messages = Vec::new();
        if is_terminal_turn_status(&turn.status)
            && let Some(event) = self.event_notification(AppEvent::turn_completed(&turn))?
        {
            messages.push(event);
        }
        messages.push(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnInterruptResult {
                    turn_id: turn.turn_id,
                    status: status.to_string(),
                    agent_loop_status: Some(turn.agent_loop_status),
                })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    pub(super) fn turn_status(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        match self.store.get_turn(&params.turn_id) {
            Ok(turn) => json_response(
                message.required_id(),
                TurnResult {
                    turn: self.refresh_turn_if_unowned(turn)?,
                },
            ),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TURN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn server_shutdown(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        self.shutdown_requested = true;
        self.request_execution_stop()?;
        json_response(
            message.required_id(),
            ServerShutdownResult { shutdown: true },
        )
    }
}
