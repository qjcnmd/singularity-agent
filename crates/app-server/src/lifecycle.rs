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
            sandbox_backend: Arc::new(WindowsSandboxBackend::new()),
            provider_snapshot,
            active_turns: Arc::new(Mutex::new(HashMap::new())),
            steer_handles: Arc::new(Mutex::new(HashMap::new())),
            execution_stopped: Arc::new(AtomicBool::new(false)),
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
            steer_handles: Arc::clone(&self.steer_handles),
            execution_stopped: Arc::clone(&self.execution_stopped),
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
            steer_handles: Arc::clone(&self.steer_handles),
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

    /// Persist interactive user input and, when the turn is running in-process, inject it
    /// into the steer queue so the next agent loop round consumes it.
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
        if let Ok(handle) = self
            .steer_handles
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))
            .and_then(|handles| {
                handles
                    .get(&params.turn_id)
                    .cloned()
                    .ok_or_else(|| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))
            })
            && let Ok(text) = input_items_to_text(&input)
        {
            // 运行中注入：进入共享 steer 队列，下一轮开头被 drain（对齐 Pi steer 语义）。
            handle
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                .push_back(text);
        }
        json_response(message.required_id(), TurnResult { turn })
    }

    /// Request a durable pause without conflating it with cancellation.
    ///
    /// Phase 3a 起无 checkpoint 挂钩：pause 语义为「请求取消」——store 侧置
    /// cancel_requested，CancellationMonitor 轮询到后取消进程内 Agent。
    pub(super) fn turn_pause(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        let turn = match self.store.get_turn(&params.turn_id) {
            Ok(turn) => turn,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if is_terminal_turn_status(&turn.status) {
            return json_response(message.required_id(), TurnResult { turn });
        }
        let thread_id = turn.thread_id.clone();
        if let Some(cancellation) = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&params.turn_id)
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
                format!("trace_{}_pause_requested", turn.turn_id),
                thread_id,
                turn.turn_id.clone(),
                "app_server",
                "turn pause requested",
            )
        };
        let turn = self
            .store
            .request_turn_cancellation(&params.turn_id, &trace)?;
        json_response(message.required_id(), TurnResult { turn })
    }

    /// Explicitly claim and resume a non-approval suspended turn. The store CAS prevents two
    /// callers from issuing a duplicate first `ModelTurnRequest`.
    pub(super) fn turn_resume(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        let trace_binding = RefCell::new(None);
        let result = self.handle_turn_resume_streaming_values(
            message,
            |binding| *trace_binding.borrow_mut() = Some(binding),
            |message| messages.push(message),
        );
        self.pending_transport_trace_binding = trace_binding.into_inner();
        result?;
        Ok(messages)
    }

    /// 执行 `turn/resume`，在恢复期间实时发出生命周期事件并保留唯一最终响应。
    pub fn handle_turn_resume_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let coordinator = self.output_order.clone();
        let mut sequencing_error = None;
        let trace_binding = RefCell::new(None);
        let result = self.handle_turn_resume_streaming_values(
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

    fn handle_turn_resume_streaming_values(
        &mut self,
        message: JsonRpcMessage,
        mut bind_trace: impl FnMut(TransportTraceBinding),
        mut emit: impl FnMut(Value),
    ) -> AppServerResult<()> {
        let params: TurnIdParams = parse_params(&message)?;
        let current = match self.store.get_turn(&params.turn_id) {
            Ok(turn) => turn,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), TURN_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        if !matches!(current.status, TurnStatus::Paused | TurnStatus::Suspended) {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), "turn is not paused or suspended")?,
            );
            return Ok(());
        }
        let thread = self.store.get_thread(&current.thread_id)?;
        if thread.status != singularity_protocol::ThreadStatus::Active {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), THREAD_ARCHIVED_CONTINUATION)?,
            );
            return Ok(());
        }
        let Some(_execution_guard) = self
            .store
            .try_begin_workspace_execution(&thread.thread_id)?
        else {
            emit_messages(
                &mut emit,
                invalid_state_response(message.required_id(), WORKSPACE_EXECUTION_ACTIVE)?,
            );
            return Ok(());
        };
        let (cancellation, mut active_turn) = self.activate_turn(&current.turn_id)?;
        bind_trace(TransportTraceBinding::for_turn(
            thread.thread_id.clone(),
            current.turn_id.clone(),
        ));
        let mut assistant_events =
            AssistantItemEventState::new(SessionStore::allocate_assistant_item_id());
        active_turn.start_monitor();
        // 恢复输入：turn 行的持久化用户输入（无 checkpoint 语义；会话文件承载历史）。
        let user_input = match self.store.get_turn_user_input(&current.turn_id) {
            Ok(input) => input,
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &current,
                    Some(&assistant_events),
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(
                        monitor_outcome,
                        turn_failure_from_error(&AppServerError::Store(error), TurnFailureStage::AgentLoop),
                    ),
                );
            }
        };
        let input_text = match input_items_to_text(&user_input) {
            Ok(text) => text,
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &current,
                    Some(&assistant_events),
                    &cancellation,
                    monitor_outcome,
                    monitor_failure_or(monitor_outcome, turn_failure_from_error(&error, TurnFailureStage::AgentLoop)),
                );
            }
        };
        let status = match self.run_agent_core(
            &thread,
            &current,
            &input_text,
            &cancellation,
            &mut assistant_events,
            &mut emit,
        ) {
            Ok(status) => status,
            Err(error) => {
                let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
                return self.finish_turn_failure(
                    &mut emit,
                    &current,
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
        let monitor_outcome = active_turn.stabilize_monitor(&cancellation);
        if monitor_outcome == Some(CancellationMonitorOutcome::InfrastructureFailure) {
            return self.finish_turn_failure(
                &mut emit,
                &current,
                Some(&assistant_events),
                &cancellation,
                monitor_outcome,
                TurnFailure {
                    stage: TurnFailureStage::CancellationMonitor,
                    cause: TurnFailureCause::CancellationMonitor,
                },
            );
        }
        let committed = match self.commit_turn_run_status(
            current.clone(),
            &status,
            Some(&assistant_events.item_id),
            &cancellation,
            monitor_outcome,
        ) {
            Ok(committed) => committed,
            Err(error) => {
                return self.finish_turn_failure(
                    &mut emit,
                    &current,
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
        };
        emit_messages(
            &mut emit,
            self.committed_turn_events(&committed, Some(&assistant_events))?,
        );
        emit(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnResult {
                    turn: committed.turn,
                })?,
            )
            .to_wire_value(),
        );
        Ok(())
    }

    /// 用新 headless core 执行一个 turn：会话文件 open/create → `Agent::new` → `run`。
    ///
    /// - 跨轮历史由 `<thread_id>.jsonl` 会话文件承载（不再读 store checkpoint/history seed）。
    /// - run 前把 store 中尚未消费的 turn/input 按 delivery 注入 steer/follow-up 队列。
    /// - 事件映射：`on_message_update` → `item/agentMessage/delta`（`project_assistant_delta`）；
    ///   `on_tool_execution_start` → 只进 trace；工具执行持久化只落 session 文件。
    /// - 注册该 turn 的 steer handle（`turn/input` 运行中注入通道），guard drop 时注销。
    pub(super) fn run_agent_core(
        &self,
        thread: &Thread,
        turn: &Turn,
        input_text: &str,
        cancellation: &CancellationToken,
        assistant_events: &mut AssistantItemEventState,
        emit: &mut impl FnMut(Value),
    ) -> AppServerResult<AgentRunStatus> {
        let session = open_or_create_thread_session(thread)?;
        let (provider, config) = self.provider_and_config_for_thread(thread)?;
        let mut agent = Agent::new(provider, ToolRegistry::new(), config, session)?;
        let boundary = self.store.turn_boundary_state(&turn.turn_id, true)?;
        for pending in boundary.inputs {
            let text = input_items_to_text(&pending.input)?;
            match pending.delivery {
                TurnInputDelivery::Steer => agent.steer(&text),
                TurnInputDelivery::FollowUp => agent.follow_up(&text),
            }
        }
        let steer_handle = agent.steer_handle();
        self.steer_handles
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .insert(turn.turn_id.clone(), steer_handle);
        let callback_error = RefCell::new(None);
        let projector =
            observability::TraceProjector::new(&self.store, &thread.thread_id, &turn.turn_id)?;
        let mut on_message_update = |delta: &str| {
            if callback_error.borrow().is_some() {
                return;
            }
            match self.project_assistant_delta(assistant_events, delta) {
                Ok(messages) => emit_messages(emit, messages),
                Err(error) => *callback_error.borrow_mut() = Some(error),
            }
        };
        let mut on_tool_execution_start = |name: &str, args: &str| {
            if callback_error.borrow().is_some() {
                return;
            }
            if let Err(error) = projector.project_tool_execution(name, args) {
                *callback_error.borrow_mut() = Some(AppServerError::Store(error));
            }
        };
        let mut events = AgentEvents::new();
        events.on_message_update = Some(&mut on_message_update);
        events.on_tool_execution_start = Some(&mut on_tool_execution_start);
        let outcome = match agent.run(input_text, &mut events, cancellation) {
            Ok(outcome) => outcome,
            // provider 调用内的取消（interrupt/pause/shutdown）：外部已请求停止，
            // 按 AgentOutcome 的 aborted 语义收敛，不视为失败（与旧链取消响应一致）。
            Err(_) if cancellation.is_cancelled() => AgentOutcome {
                final_text: String::new(),
                turns: 0,
                usage: singularity_model::ModelUsage::default(),
                compacted: false,
                aborted: true,
            },
            Err(error) => return Err(error.into()),
        };
        if let Some(error) = callback_error.into_inner() {
            return Err(error);
        }
        projector.project_outcome(&outcome)?;
        Ok(outcome_to_run_status(outcome))
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
        // 在 turn 行创建前提取 run 输入文本（参数无效时不创建 turn）。
        let input_text = match input_items_to_text(&payload) {
            Ok(text) => text,
            Err(_) => {
                emit_messages(
                    &mut emit,
                    invalid_params_response(message.required_id())?,
                );
                return Ok(());
            }
        };
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
            Err(StoreError::WorkspaceHasNonterminalTurn { turn_id, .. }) => {
                emit_messages(
                    &mut emit,
                    invalid_state_response(
                        message.required_id(),
                        format!(
                            "workspace already has an active or pending turn {turn_id}; use sg turn resume/pause/input {turn_id}"
                        ),
                    )?,
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
        let status = match self.run_agent_core(
            &thread,
            &turn,
            &input_text,
            &cancellation,
            &mut assistant_events,
            &mut emit,
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
        let committed = match self.commit_turn_run_status(
            turn.clone(),
            &status,
            Some(&assistant_events.item_id),
            &cancellation,
            monitor_outcome,
        ) {
            Ok(committed) => committed,
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
        let event = agent_loop_trace(turn, run_status);
        self.store.commit_turn_outcome_with_authority(
            &turn.turn_id,
            CommitTurnOutcomeParams {
                status: turn_status_for_agent(&run_status.status),
                agent_loop_status: run_status.status.as_str(),
                assistant_item_id: assistant_delta.as_ref().and(assistant_item_id),
                assistant_delta: assistant_delta.as_deref(),
                trace: &event,
            },
            authority,
        )
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
