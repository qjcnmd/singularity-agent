//! AppServer construction, turn supervision, cancellation, and shutdown.

use super::*;
#[cfg(test)]
use singularity_agent::AgentTextDeltaCallback;

fn turn_tool_execution_id(turn_id: &str, tool_call_id: &str) -> String {
    format!("turn:{turn_id}:tool:{tool_call_id}")
}

enum TurnBoundaryAction {
    Continue,
    Restart(TurnCheckpoint),
    Paused,
}

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
            pending_transport_trace_binding: None,
            test_provider_override: None,
        }
    }

    /// 仅测试：注入动态 provider 覆盖，使 JSON-RPC E2E 测试无需真实 HTTP 端点。
    #[doc(hidden)]
    pub fn with_test_provider(
        mut self,
        provider: std::sync::Arc<dyn singularity_model::Provider + Send + Sync>,
    ) -> Self {
        self.test_provider_override = Some(provider);
        self
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

    /// 为唯一 stdout transport 打开独立、身份校验的 SQLite trace 连接。
    pub fn transport_trace_store(&self) -> AppServerResult<SessionStore> {
        self.store.trusted_reopen().map_err(Into::into)
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
            pending_transport_trace_binding: None,
            test_provider_override: self.test_provider_override.clone(),
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
        let trace_binding = RefCell::new(None);
        let result = self.handle_turn_start_streaming_values(
            message,
            |binding| *trace_binding.borrow_mut() = Some(binding),
            |message| messages.push(message),
        );
        self.pending_transport_trace_binding = trace_binding.into_inner();
        result?;
        Ok(messages)
    }

    /// Persist interactive user input independently of workspace execution ownership.
    pub(super) fn turn_input(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnInputParams = parse_params(&message)?;
        let input = serde_json::to_value(&params.input)?;
        let turn = match self.store.append_turn_input(
            &params.turn_id,
            &params.input_id,
            params.delivery,
            &input,
        ) {
            Ok(turn) => turn,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            }
            Err(StoreError::InvalidState(error)) => {
                return invalid_state_response(message.required_id(), error);
            }
            Err(error) => return Err(error.into()),
        };
        json_response(message.required_id(), TurnResult { turn })
    }

    /// Request a durable pause without conflating it with cancellation.
    pub(super) fn turn_pause(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        let turn = match self.store.request_turn_pause(&params.turn_id) {
            Ok(turn) => turn,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            }
            Err(StoreError::InvalidState(error)) => {
                return invalid_state_response(message.required_id(), error);
            }
            Err(error) => return Err(error.into()),
        };
        json_response(message.required_id(), TurnResult { turn })
    }

    /// Explicitly claim and resume a non-approval suspended turn. The store CAS prevents two
    /// callers from issuing a duplicate first `ModelTurnRequest`.
    pub(super) fn turn_resume(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        let current = match self.store.get_turn(&params.turn_id) {
            Ok(turn) => turn,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if !matches!(current.status, TurnStatus::Paused | TurnStatus::Suspended) {
            return invalid_state_response(
                message.required_id(),
                "turn is not paused or suspended",
            );
        }
        let thread = self.store.get_thread(&current.thread_id)?;
        if thread.status != singularity_protocol::ThreadStatus::Active {
            return invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION);
        }
        let Some(_execution_guard) = self
            .store
            .try_begin_workspace_execution(&thread.thread_id)?
        else {
            return invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE);
        };
        let (turn, checkpoint_payload) = match self.store.claim_suspended_turn(&params.turn_id) {
            Ok(claimed) => claimed,
            Err(StoreError::InvalidState(error)) => {
                return invalid_state_response(message.required_id(), error);
            }
            Err(error) => return Err(error.into()),
        };
        let checkpoint = match TurnCheckpoint::decode(&checkpoint_payload) {
            Ok(checkpoint) => checkpoint,
            Err(_) => {
                let _ = self.store.update_turn_state(
                    &turn.turn_id,
                    TurnStatus::Failed,
                    AgentStatus::Failed.as_str(),
                );
                return invalid_state_response(
                    message.required_id(),
                    "turn checkpoint unavailable",
                );
            }
        };
        let resumed = (|| -> AppServerResult<Vec<Value>> {
            let resume_attempt = match checkpoint.resume_attempt().checked_add(1) {
                Some(resume_attempt) => resume_attempt,
                None => {
                    return Err(AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Serialization,
                    });
                }
            };
            let checkpoint = checkpoint.with_resume_attempt(resume_attempt);
            let encoded_checkpoint =
                checkpoint
                    .encode()
                    .map_err(|_| AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Serialization,
                    })?;
            self.store
                .save_turn_checkpoint(
                    &turn.turn_id,
                    &thread.thread_id,
                    &encoded_checkpoint,
                    checkpoint.checkpoint_version(),
                )
                .map_err(AppServerError::Store)?;
            let user_input = self.store.get_turn_user_input(&turn.turn_id)?;
            let params_for_agent = TurnStartParams {
                thread_id: thread.thread_id.clone(),
                input: serde_json::from_value(user_input)?,
            };
            let history = self.store.read_thread_history_before_turn(
                &thread.thread_id,
                &turn.turn_id,
                DEFAULT_THREAD_HISTORY_TURN_LIMIT,
            )?;
            let workspace_tools =
                workspace_tools_for_thread(&thread, Arc::clone(&self.sandbox_backend))
                    .map_err(AppServerError::Workspace)?;
            let (cancellation, mut active_turn) = self.activate_turn(&turn.turn_id)?;
            let mut assistant_events =
                AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
            let mut emit = Vec::new();
            let mut status = {
                let mut on_event = |event: AgentLoopEvent| match event {
                    AgentLoopEvent::FinalTextDelta { delta } => {
                        emit.extend(self.project_assistant_delta(&mut assistant_events, &delta)?);
                        Ok(())
                    }
                    AgentLoopEvent::Observation(_) => Ok(()),
                };
                let invocation = AgentLoopInvocation {
                    thread: &thread,
                    params: &params_for_agent,
                    turn_id: &turn.turn_id,
                    history: &history.messages,
                    cancellation: &cancellation,
                    monitor_control: active_turn.monitor_control(),
                };
                let result = if let Some(provider) = &self.test_provider_override {
                    self.run_resumed_agent_loop_with_provider_and_tools(
                        Arc::clone(provider),
                        invocation,
                        workspace_tools,
                        &checkpoint,
                        &mut on_event,
                        true,
                    )
                } else {
                    let provider = self.provider_snapshot.provider().map_err(|_| {
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
                result?
            };
            let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
            let committed = loop {
                match self.commit_turn_run_status(
                    turn.clone(),
                    &status,
                    Some(&assistant_events.item_id),
                    &cancellation,
                    monitor_outcome,
                ) {
                    Ok(committed) => break committed,
                    Err(AppServerError::Store(StoreError::TurnBoundaryPending { .. })) => {
                        status =
                            self.resume_pending_terminal_boundary(
                                AgentLoopInvocation {
                                    thread: &thread,
                                    params: &params_for_agent,
                                    turn_id: &turn.turn_id,
                                    history: &history.messages,
                                    cancellation: &cancellation,
                                    monitor_control: active_turn.monitor_control(),
                                },
                                &mut |event| match event {
                                    AgentLoopEvent::FinalTextDelta { delta } => {
                                        emit.extend(self.project_assistant_delta(
                                            &mut assistant_events,
                                            &delta,
                                        )?);
                                        Ok(())
                                    }
                                    AgentLoopEvent::Observation(_) => Ok(()),
                                },
                                true,
                            )?;
                    }
                    Err(error) => return Err(error),
                }
            };
            let mut outputs = emit;
            outputs.extend(self.committed_turn_events(&committed, Some(&assistant_events))?);
            outputs.push(
                JsonRpcMessage::response(
                    message.required_id(),
                    serde_json::to_value(TurnResult {
                        turn: committed.turn,
                    })?,
                )
                .to_wire_value(),
            );
            Ok(outputs)
        })();
        match resumed {
            Ok(outputs) => Ok(outputs),
            Err(error) => {
                let failure = turn_failure_from_error(&error, TurnFailureStage::AgentLoop);
                match self.store.suspend_claimed_turn_after_failure(&turn.turn_id) {
                    Ok(_) => Err(error),
                    Err(_) => Err(AppServerError::TurnTerminalization {
                        stage: failure.stage,
                        cause: failure.cause,
                        failure: TurnTerminalizationFailure::Store,
                    }),
                }
            }
        }
    }

    /// Persist one typed AgentLoop boundary and, for tool calls, its execution owner.
    fn persist_turn_checkpoint_event(
        &self,
        thread_id: &str,
        turn_id: &str,
        event: TurnCheckpointEvent,
    ) -> AppServerResult<TurnBoundaryAction> {
        let checkpoint_version = event.checkpoint.checkpoint_version();
        let checkpoint = event
            .checkpoint
            .encode()
            .map_err(|_| AppServerError::TurnExecution {
                stage: TurnFailureStage::ApprovalCheckpoint,
                cause: TurnFailureCause::Serialization,
            })?;
        let tool_boundary_claimed = match &event.phase {
            TurnCheckpointPhase::ToolCallsReady { .. } => {
                let executions = event
                    .checkpoint
                    .pending_tool_calls()
                    .iter()
                    .map(|pending| ToolExecution {
                        execution_id: turn_tool_execution_id(turn_id, &pending.tool_call_id),
                        thread_id: thread_id.to_string(),
                        turn_id: turn_id.to_string(),
                        tool_call_id: pending.tool_call_id.clone(),
                        state: ToolExecutionState::Running,
                        payload: json!({
                            "kind": "tool_call",
                            "tool_name": pending.tool_name.as_str(),
                        }),
                    })
                    .collect::<Vec<_>>();
                Some(
                    self.store
                        .begin_tool_executions_at_checkpoint(
                            &executions,
                            &checkpoint,
                            checkpoint_version,
                        )
                        .map_err(AppServerError::Store)?,
                )
            }
            _ => None,
        };
        match &event.phase {
            TurnCheckpointPhase::Initial
            | TurnCheckpointPhase::BeforeModelRequest { .. }
            | TurnCheckpointPhase::ModelResponseCommitted => self
                .store
                .save_turn_checkpoint(turn_id, thread_id, &checkpoint, checkpoint_version)
                .map_err(AppServerError::Store)?,
            TurnCheckpointPhase::ToolCallsReady { .. } => {}
            TurnCheckpointPhase::ToolResultsCommitted { tool_call_ids } => {
                let execution_ids = tool_call_ids
                    .iter()
                    .map(|tool_call_id| turn_tool_execution_id(turn_id, tool_call_id))
                    .collect::<Vec<_>>();
                self.store
                    .commit_tool_results_checkpoint(
                        &execution_ids,
                        turn_id,
                        thread_id,
                        &checkpoint,
                        checkpoint_version,
                    )
                    .map_err(AppServerError::Store)?;
            }
        }

        let include_follow_up = matches!(
            &event.phase,
            TurnCheckpointPhase::BeforeModelRequest {
                finalization_only: true
            } | TurnCheckpointPhase::ModelResponseCommitted
        );
        let boundary = self
            .store
            .turn_boundary_state(turn_id, include_follow_up)
            .map_err(AppServerError::Store)?;
        if boundary.inputs.is_empty() && !boundary.pause_requested {
            return Ok(TurnBoundaryAction::Continue);
        }
        if matches!(tool_boundary_claimed, Some(true)) {
            // Input accepted after the tool execution transaction linearized is handled only
            // after the tool reaches a durable result or Unknown.
            return Ok(TurnBoundaryAction::Continue);
        }

        let mut messages = Vec::with_capacity(boundary.inputs.len());
        let mut input_ids = Vec::with_capacity(boundary.inputs.len());
        for pending in &boundary.inputs {
            let input: Vec<singularity_protocol::InputItem> =
                serde_json::from_value(pending.input.clone())?;
            let message = input
                .into_iter()
                .map(|item| match item {
                    singularity_protocol::InputItem::Text { text } => text,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if message.trim().is_empty() {
                return Err(AppServerError::Workspace(
                    "persisted turn input is empty".to_string(),
                ));
            }
            messages.push(message);
            input_ids.push(pending.input_id.clone());
        }
        let cancel_pending_tools =
            matches!(&event.phase, TurnCheckpointPhase::ToolCallsReady { .. })
                && !messages.is_empty();
        let updated =
            if messages.is_empty() {
                event.checkpoint
            } else {
                let updated = event
                    .checkpoint
                    .with_user_inputs(&messages, cancel_pending_tools)
                    .map_err(|_| AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Serialization,
                    })?;
                let resume_attempt = updated.resume_attempt().checked_add(1).ok_or(
                    AppServerError::TurnExecution {
                        stage: TurnFailureStage::ApprovalCheckpoint,
                        cause: TurnFailureCause::Serialization,
                    },
                )?;
                updated.with_resume_attempt(resume_attempt)
            };
        let updated_payload = updated
            .encode()
            .map_err(|_| AppServerError::TurnExecution {
                stage: TurnFailureStage::ApprovalCheckpoint,
                cause: TurnFailureCause::Serialization,
            })?;
        if input_ids.is_empty() {
            self.store
                .pause_turn_with_checkpoint(
                    turn_id,
                    thread_id,
                    &updated_payload,
                    updated.checkpoint_version(),
                )
                .map_err(AppServerError::Store)?;
        } else {
            self.store
                .consume_turn_inputs_with_checkpoint(
                    turn_id,
                    thread_id,
                    &input_ids,
                    &updated_payload,
                    updated.checkpoint_version(),
                    boundary.pause_requested,
                )
                .map_err(AppServerError::Store)?;
        }
        if boundary.pause_requested {
            Ok(TurnBoundaryAction::Paused)
        } else {
            Ok(TurnBoundaryAction::Restart(updated))
        }
    }

    /// 执行 `turn/start`，并在每个阶段完成时返回已预留顺序的输出。
    pub fn handle_turn_start_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let trace_binding = RefCell::new(None);
        let result = self.handle_turn_start_streaming_values(
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

    /// 执行 `turn/start` 并返回未携带 transport 顺序的兼容消息。
    pub fn handle_turn_start_streaming(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let result = self.handle_turn_start_streaming_values(
            message,
            |_| {},
            |message| {
                if sequencing_error.is_some() {
                    return;
                }
                match sequence_output(&coordinator, message, None) {
                    Ok(output) => {
                        coordinator.complete(output.reservation.order);
                        emit(output.message);
                    }
                    Err(error) => sequencing_error = Some(error),
                }
            },
        );
        if let Some(error) = sequencing_error {
            return Err(error);
        }
        result
    }

    /// 执行 `turn/start`，并在每个持久化阶段完成时发出生命周期事件。
    pub(super) fn handle_turn_start_streaming_values(
        &mut self,
        message: JsonRpcMessage,
        mut bind_trace: impl FnMut(TransportTraceBinding),
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
        bind_trace(TransportTraceBinding::for_turn(
            turn.thread_id.clone(),
            turn.turn_id.clone(),
        ));
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        active_turn.start_monitor();
        match self.event_notification(AppEvent::turn_started(&turn)) {
            Ok(Some(event)) => emit(event),
            Ok(None) => {}
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &turn,
                    Some(&assistant_events),
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(
                        monitor_outcome,
                        turn_failure_from_error(&error, TurnFailureStage::EventNotification),
                    ),
                );
            }
        }
        let mut status = match self.run_agent_loop(
            AgentLoopInvocation {
                thread: &thread,
                params: &params,
                turn_id: &turn.turn_id,
                history: &started.history.messages,
                cancellation: &cancellation,
                monitor_control: active_turn.monitor_control(),
            },
            workspace_tools,
            &mut |event| match event {
                AgentLoopEvent::FinalTextDelta { delta } => {
                    let messages = self.project_assistant_delta(&mut assistant_events, &delta)?;
                    emit_messages(&mut emit, messages);
                    Ok(())
                }
                AgentLoopEvent::Observation(_) => Ok(()),
            },
        ) {
            Ok(status) => status,
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &turn,
                    Some(&assistant_events),
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
                    Some(&assistant_events),
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
                Some(&assistant_events),
                &cancellation,
                monitor_outcome,
                TurnFailure {
                    stage: TurnFailureStage::CancellationMonitor,
                    cause: TurnFailureCause::CancellationMonitor,
                },
            );
        }
        let committed = loop {
            match self.commit_turn_run_status(
                turn.clone(),
                &status,
                Some(&assistant_events.item_id),
                &cancellation,
                monitor_outcome,
            ) {
                Ok(committed) => break committed,
                Err(AppServerError::Store(StoreError::TurnBoundaryPending { .. })) => {
                    status = self.resume_pending_terminal_boundary(
                        AgentLoopInvocation {
                            thread: &thread,
                            params: &params,
                            turn_id: &turn.turn_id,
                            history: &started.history.messages,
                            cancellation: &cancellation,
                            monitor_control: active_turn.monitor_control(),
                        },
                        &mut |event| match event {
                            AgentLoopEvent::FinalTextDelta { delta } => {
                                let messages =
                                    self.project_assistant_delta(&mut assistant_events, &delta)?;
                                emit_messages(&mut emit, messages);
                                Ok(())
                            }
                            AgentLoopEvent::Observation(_) => Ok(()),
                        },
                        true,
                    )?;
                }
                Err(error) => {
                    return self.finish_turn_failure(
                        &mut emit,
                        &turn,
                        Some(&assistant_events),
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
            }
        };
        let terminal_events = self.committed_turn_events(&committed, Some(&assistant_events))?;
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
        assistant_events: Option<&AssistantItemEventState>,
        cancellation: &CancellationToken,
        monitor_outcome: Option<CancellationMonitorOutcome>,
        failure: impl Into<TurnFailure>,
    ) -> AppServerResult<()> {
        let failure = failure.into();
        match self.terminalize_turn_failure(turn, cancellation, monitor_outcome, failure) {
            Ok(TurnTerminalizationResult::Committed(committed)) => {
                match self.committed_turn_events(&committed, assistant_events) {
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
            Ok(TurnTerminalizationResult::Preserved) => {
                self.emit_realtime_item_failure(emit, assistant_events)?;
                Err(AppServerError::TurnExecution {
                    stage: failure.stage,
                    cause: failure.cause,
                })
            }
            Err(cleanup_failure) => {
                self.emit_realtime_item_failure(emit, assistant_events)?;
                Err(AppServerError::TurnTerminalization {
                    stage: failure.stage,
                    cause: failure.cause,
                    failure: cleanup_failure,
                })
            }
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
        match self.commit_effective_turn_status_with_authority(&current, &status, None, authority) {
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
                    match self.commit_effective_turn_status(&latest, &interrupted, None) {
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
            &self.store,
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
        on_event: &mut dyn FnMut(AgentLoopEvent) -> AppServerResult<()>,
    ) -> AppServerResult<AgentRunStatus> {
        if let Some(test_provider) = &self.test_provider_override {
            return match self.run_agent_loop_with_provider_and_tools(
                std::sync::Arc::clone(test_provider),
                invocation,
                workspace_tools,
                on_event,
                true,
            ) {
                Err(AppServerError::ProjectInstructions(_)) => Ok(safe_failed_agent_status(
                    SAFE_PROJECT_INSTRUCTIONS_FAILURE,
                    "project_instructions",
                )),
                Err(AppServerError::Workspace(_)) => Ok(safe_failed_agent_status(
                    SAFE_WORKSPACE_FAILURE,
                    "workspace",
                )),
                result => result,
            };
        }
        let provider = match self.provider_snapshot.provider() {
            Ok(provider) => provider,
            Err(error) => {
                let category = error.error.category();
                let mut status = safe_failed_agent_status(SAFE_PROVIDER_FAILURE, "provider");
                status.error_category = Some(category);
                return Ok(status);
            }
        };
        match self.run_agent_loop_with_provider_and_tools(
            provider,
            invocation,
            workspace_tools,
            on_event,
            true,
        ) {
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
        on_event: &mut dyn FnMut(AgentLoopEvent) -> AppServerResult<()>,
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
        if let Some(test_provider) = &self.test_provider_override {
            return self.resume_agent_loop_after_gate_with_monitor(
                ApprovalResumeInput {
                    request,
                    decision,
                    turn,
                    thread,
                    pending_approval,
                },
                std::sync::Arc::clone(test_provider),
                context,
                on_event,
                true,
            );
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
            on_event,
            true,
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
        P: Provider + Clone,
    {
        self.resume_agent_loop_after_gate_with_text_deltas(
            request,
            decision,
            pending_tool_call,
            provider,
            cancellation,
            prepared_workspace_tools,
            &mut |_| {},
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resume_agent_loop_after_gate_with_text_deltas<P>(
        &self,
        request: &ApprovalRequest,
        decision: &ApprovalDecision,
        pending_tool_call: Option<Value>,
        provider: P,
        cancellation: &CancellationToken,
        prepared_workspace_tools: Option<WorkspaceTools>,
        on_text_delta: &mut AgentTextDeltaCallback<'_>,
    ) -> AppServerResult<Option<(Turn, AgentRunStatus, Vec<PendingApprovalOccurrence>)>>
    where
        P: Provider + Clone,
    {
        let turn = self.store.get_turn(&request.turn_id)?;
        let thread = self.store.get_thread(&turn.thread_id)?;
        let pending_approval = match pending_tool_call.as_ref() {
            Some(payload) => decode_pending_approval(request, Some(payload))?,
            None => None,
        };
        let mut on_event = |event| match event {
            AgentLoopEvent::FinalTextDelta { delta } => {
                on_text_delta(&delta);
                Ok(())
            }
            AgentLoopEvent::Observation(_) => Ok(()),
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
            &mut on_event,
            false,
        )
    }

    pub(super) fn resume_agent_loop_after_gate_with_monitor<P>(
        &self,
        input: ApprovalResumeInput<'_>,
        provider: P,
        context: ApprovalResumeContext<'_>,
        on_event: &mut dyn FnMut(AgentLoopEvent) -> AppServerResult<()>,
        project_observability: bool,
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
        let mut projector = if project_observability {
            Some(observability::TraceProjector::new(
                &self.store,
                &thread.thread_id,
                &turn.turn_id,
            )?)
        } else {
            None
        };
        let mut callback_error = None;
        let result = {
            let mut callback = |event: AgentLoopEvent| -> Result<(), AgentLoopEventSinkError> {
                if callback_error.is_some() {
                    return Err(AgentLoopEventSinkError);
                }
                if let Some(projector) = projector.as_mut()
                    && let Err(error) = projector.project_event(event.clone())
                {
                    callback_error = Some(AppServerError::Store(error));
                    return Err(AgentLoopEventSinkError);
                }
                if let Err(error) = on_event(event) {
                    callback_error = Some(error);
                    return Err(AgentLoopEventSinkError);
                }
                Ok(())
            };
            AgentLoop::new(provider, ToolBroker::new(registry), policy)
                .with_workspace_tools(workspace_tools)
                .with_cancellation_token(context.cancellation.clone())
                .resume_pending_approval_with_events(&loop_input, &pending_approval, &mut callback)
        };
        if let Some(error) = callback_error {
            return Err(error);
        }
        if monitor_infrastructure_failure(context.monitor_control) {
            return Err(AppServerError::TurnExecution {
                stage: TurnFailureStage::CancellationMonitor,
                cause: TurnFailureCause::CancellationMonitor,
            });
        }
        let mut run_status = result.to_run_status();
        sanitize_agent_run_status_error(&mut run_status);
        if let Some(projector) = projector.as_mut() {
            projector
                .project_result(&run_status)
                .map_err(AppServerError::Store)?;
        }
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
        P: Provider + Clone,
    {
        self.run_agent_loop_with_provider_and_text_deltas(
            provider,
            thread,
            params,
            turn_id,
            history,
            cancellation,
            &mut |_| {},
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_agent_loop_with_provider_and_text_deltas<P>(
        &self,
        provider: P,
        thread: &Thread,
        params: &TurnStartParams,
        turn_id: &str,
        history: &[ConversationMessage],
        cancellation: &CancellationToken,
        on_text_delta: &mut AgentTextDeltaCallback<'_>,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider + Clone,
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
        let mut on_event = |event| match event {
            AgentLoopEvent::FinalTextDelta { delta } => {
                on_text_delta(&delta);
                Ok(())
            }
            AgentLoopEvent::Observation(_) => Ok(()),
        };
        self.run_agent_loop_with_provider_and_tools(
            provider,
            invocation,
            workspace_tools,
            &mut on_event,
            false,
        )
    }

    pub(super) fn run_agent_loop_with_provider_and_tools<P>(
        &self,
        provider: P,
        invocation: AgentLoopInvocation<'_>,
        workspace_tools: WorkspaceTools,
        on_event: &mut dyn FnMut(AgentLoopEvent) -> AppServerResult<()>,
        project_observability: bool,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider + Clone,
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
        let mut projector = if project_observability {
            Some(observability::TraceProjector::new(
                &self.store,
                &invocation.thread.thread_id,
                invocation.turn_id,
            )?)
        } else {
            None
        };
        let callback_error = RefCell::new(None);
        let boundary_action = RefCell::new(None);
        let agent_loop = AgentLoop::new(provider.clone(), ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools)
            .with_cancellation_token(invocation.cancellation.clone());
        let result = {
            let mut callback = |event: AgentLoopEvent| -> Result<(), AgentLoopEventSinkError> {
                if callback_error.borrow().is_some() {
                    return Err(AgentLoopEventSinkError);
                }
                if let Some(projector) = projector.as_mut()
                    && let Err(error) = projector.project_event(event.clone())
                {
                    *callback_error.borrow_mut() = Some(AppServerError::Store(error));
                    return Err(AgentLoopEventSinkError);
                }
                if let Err(error) = on_event(event) {
                    *callback_error.borrow_mut() = Some(error);
                    return Err(AgentLoopEventSinkError);
                }
                Ok(())
            };
            let mut on_checkpoint = |event: TurnCheckpointEvent| {
                if callback_error.borrow().is_some() {
                    return Err(AgentLoopEventSinkError);
                }
                match self.persist_turn_checkpoint_event(
                    &invocation.thread.thread_id,
                    invocation.turn_id,
                    event,
                ) {
                    Ok(TurnBoundaryAction::Continue) => Ok(()),
                    Ok(action) => {
                        *boundary_action.borrow_mut() = Some(action);
                        Err(AgentLoopEventSinkError)
                    }
                    Err(error) => {
                        *callback_error.borrow_mut() = Some(error);
                        Err(AgentLoopEventSinkError)
                    }
                }
            };
            agent_loop.run_with_events_and_checkpoints(
                &loop_input,
                &mut callback,
                &mut on_checkpoint,
            )
        };
        if let Some(error) = callback_error.into_inner() {
            return Err(error);
        }
        match boundary_action.into_inner() {
            Some(TurnBoundaryAction::Restart(checkpoint)) => {
                let workspace_tools = workspace_tools_for_thread(
                    invocation.thread,
                    Arc::clone(&self.sandbox_backend),
                )
                .map_err(AppServerError::Workspace)?;
                return self.run_resumed_agent_loop_with_provider_and_tools(
                    provider,
                    invocation,
                    workspace_tools,
                    &checkpoint,
                    on_event,
                    project_observability,
                );
            }
            Some(TurnBoundaryAction::Paused) => return Ok(AgentRunStatus::paused()),
            Some(TurnBoundaryAction::Continue) | None => {}
        }
        let mut run_status = result.to_run_status();
        sanitize_agent_run_status_error(&mut run_status);
        if let Some(projector) = projector.as_mut() {
            projector
                .project_result(&run_status)
                .map_err(AppServerError::Store)?;
        }
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

    /// Continue one suspended turn from its validated checkpoint without replaying a tool call.
    pub(super) fn run_resumed_agent_loop_with_provider_and_tools<P>(
        &self,
        provider: P,
        invocation: AgentLoopInvocation<'_>,
        workspace_tools: WorkspaceTools,
        checkpoint: &TurnCheckpoint,
        on_event: &mut dyn FnMut(AgentLoopEvent) -> AppServerResult<()>,
        project_observability: bool,
    ) -> AppServerResult<AgentRunStatus>
    where
        P: Provider + Clone,
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
        )?
        .with_resume_attempt(checkpoint.resume_attempt());
        let mut projector = if project_observability {
            Some(observability::TraceProjector::new(
                &self.store,
                &invocation.thread.thread_id,
                invocation.turn_id,
            )?)
        } else {
            None
        };
        let callback_error = RefCell::new(None);
        let boundary_action = RefCell::new(None);
        let agent_loop = AgentLoop::new(provider.clone(), ToolBroker::new(registry), policy)
            .with_workspace_tools(workspace_tools)
            .with_cancellation_token(invocation.cancellation.clone());
        let result = {
            let mut callback = |event: AgentLoopEvent| -> Result<(), AgentLoopEventSinkError> {
                if callback_error.borrow().is_some() {
                    return Err(AgentLoopEventSinkError);
                }
                if let Some(projector) = projector.as_mut()
                    && let Err(error) = projector.project_event(event.clone())
                {
                    *callback_error.borrow_mut() = Some(AppServerError::Store(error));
                    return Err(AgentLoopEventSinkError);
                }
                if let Err(error) = on_event(event) {
                    *callback_error.borrow_mut() = Some(error);
                    return Err(AgentLoopEventSinkError);
                }
                Ok(())
            };
            let mut on_checkpoint = |event: TurnCheckpointEvent| {
                if callback_error.borrow().is_some() {
                    return Err(AgentLoopEventSinkError);
                }
                match self.persist_turn_checkpoint_event(
                    &invocation.thread.thread_id,
                    invocation.turn_id,
                    event,
                ) {
                    Ok(TurnBoundaryAction::Continue) => Ok(()),
                    Ok(action) => {
                        *boundary_action.borrow_mut() = Some(action);
                        Err(AgentLoopEventSinkError)
                    }
                    Err(error) => {
                        *callback_error.borrow_mut() = Some(error);
                        Err(AgentLoopEventSinkError)
                    }
                }
            };
            agent_loop.resume_turn_with_events_and_checkpoints(
                &loop_input,
                checkpoint,
                &mut callback,
                &mut on_checkpoint,
            )
        };
        if let Some(error) = callback_error.into_inner() {
            return Err(error);
        }
        match boundary_action.into_inner() {
            Some(TurnBoundaryAction::Restart(checkpoint)) => {
                let workspace_tools = workspace_tools_for_thread(
                    invocation.thread,
                    Arc::clone(&self.sandbox_backend),
                )
                .map_err(AppServerError::Workspace)?;
                return self.run_resumed_agent_loop_with_provider_and_tools(
                    provider,
                    invocation,
                    workspace_tools,
                    &checkpoint,
                    on_event,
                    project_observability,
                );
            }
            Some(TurnBoundaryAction::Paused) => return Ok(AgentRunStatus::paused()),
            Some(TurnBoundaryAction::Continue) | None => {}
        }
        let mut run_status = result.to_run_status();
        sanitize_agent_run_status_error(&mut run_status);
        if let Some(projector) = projector.as_mut() {
            projector
                .project_result(&run_status)
                .map_err(AppServerError::Store)?;
        }
        if let Err(error) =
            self.persist_agent_approval_requests(&result, invocation.monitor_control)
        {
            return Err(error);
        }
        Ok(run_status)
    }

    /// Resolve a follow-up that linearized after the last boundary check but before terminal commit.
    pub(super) fn resume_pending_terminal_boundary(
        &self,
        invocation: AgentLoopInvocation<'_>,
        on_event: &mut dyn FnMut(AgentLoopEvent) -> AppServerResult<()>,
        project_observability: bool,
    ) -> AppServerResult<AgentRunStatus> {
        let payload = self
            .store
            .get_turn_checkpoint(invocation.turn_id)?
            .ok_or_else(|| {
                StoreError::InvalidState(
                    "turn boundary is pending without a durable checkpoint".to_string(),
                )
            })?;
        let checkpoint =
            TurnCheckpoint::decode(&payload).map_err(|_| AppServerError::TurnExecution {
                stage: TurnFailureStage::ApprovalCheckpoint,
                cause: TurnFailureCause::Serialization,
            })?;
        let checkpoint = match self.persist_turn_checkpoint_event(
            &invocation.thread.thread_id,
            invocation.turn_id,
            TurnCheckpointEvent {
                phase: TurnCheckpointPhase::ModelResponseCommitted,
                checkpoint,
            },
        )? {
            TurnBoundaryAction::Restart(checkpoint) => checkpoint,
            TurnBoundaryAction::Paused => return Ok(AgentRunStatus::paused()),
            TurnBoundaryAction::Continue => {
                return Err(StoreError::InvalidState(
                    "turn boundary disappeared before it could be consumed".to_string(),
                )
                .into());
            }
        };
        let workspace_tools =
            workspace_tools_for_thread(invocation.thread, Arc::clone(&self.sandbox_backend))
                .map_err(AppServerError::Workspace)?;
        if let Some(provider) = &self.test_provider_override {
            self.run_resumed_agent_loop_with_provider_and_tools(
                Arc::clone(provider),
                invocation,
                workspace_tools,
                &checkpoint,
                on_event,
                project_observability,
            )
        } else {
            let provider =
                self.provider_snapshot
                    .provider()
                    .map_err(|_| AppServerError::TurnExecution {
                        stage: TurnFailureStage::AgentLoop,
                        cause: TurnFailureCause::Internal,
                    })?;
            self.run_resumed_agent_loop_with_provider_and_tools(
                provider,
                invocation,
                workspace_tools,
                &checkpoint,
                on_event,
                project_observability,
            )
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
        assistant_item_id: Option<&AllocatedAssistantItemId>,
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
        self.commit_effective_turn_status(&turn, &effective_status, assistant_item_id)
            .map_err(Into::into)
    }

    pub(super) fn commit_effective_turn_status(
        &self,
        turn: &Turn,
        run_status: &AgentRunStatus,
        assistant_item_id: Option<&AllocatedAssistantItemId>,
    ) -> Result<CommittedTurnOutcome, StoreError> {
        self.commit_effective_turn_status_with_authority(
            turn,
            run_status,
            assistant_item_id,
            TurnOutcomeAuthority::AgentLoop,
        )
    }

    pub(super) fn commit_effective_turn_status_with_authority(
        &self,
        turn: &Turn,
        run_status: &AgentRunStatus,
        assistant_item_id: Option<&AllocatedAssistantItemId>,
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
                assistant_item_id: assistant_delta.as_ref().and(assistant_item_id),
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
        assistant_item_id: Option<&AllocatedAssistantItemId>,
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
                        assistant_item_id: assistant_delta.as_ref().and(assistant_item_id),
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
