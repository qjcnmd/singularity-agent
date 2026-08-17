//! AppServer construction, turn supervision, cancellation, and shutdown.

use super::*;
use std::cell::RefCell;

impl AppServer {
    pub(super) fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        self.handle_turn_start_streaming_values(message, |message| messages.push(message))?;
        Ok(messages)
    }

    /// 执行 `turn/start`：打开 JSONL rollout，运行 AgentLoop，更新会话索引元数据。
    pub fn handle_turn_start_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        self.handle_turn_start_streaming_values(message, &mut emit)
    }

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
        let record = match self.store.get_session(&params.thread_id) {
            Ok(record) => record,
            Err(StoreError::NotFound(_)) => {
                emit_messages(
                    &mut emit,
                    not_found_response(message.required_id(), THREAD_NOT_FOUND)?,
                );
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let thread = thread_from_record(&record);
        let payload = serde_json::to_value(&params.input)?;
        let input_text = match input_items_to_text(&payload) {
            Ok(text) => text,
            Err(_) => {
                emit_messages(&mut emit, invalid_params_response(message.required_id())?);
                return Ok(());
            }
        };
        let turn_id = turn_id();
        let (cancellation, _active_turn) = self.activate_turn(&turn_id, &record.session_id)?;
        let title = title_from_input(&input_text);
        let mut metadata = SessionMetadataUpdate {
            status: Some(SessionStatus::Active),
            ..SessionMetadataUpdate::default()
        };
        if record.title.is_none() && !title.is_empty() {
            metadata.title = Some(Some(&title));
        }
        // turn 真正开始后才把索引切到 active；resume 不提前制造 active。
        self.store.update_session(&record.session_id, metadata)?;
        let turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: record.session_id.clone(),
            status: TurnStatus::Running,
            agent_loop_status: AgentStatus::Running.as_str().to_string(),
            model_usage: None,
        };
        let mut assistant_events = AssistantItemEventState::new(format!("{turn_id}_assistant"));
        emit(self.event_notification(AppEvent::turn_started(&turn))?);
        let run_result = self.run_agent_core(
            &thread,
            &turn_id,
            &input_text,
            &cancellation,
            &mut assistant_events,
            &mut emit,
        );
        // AgentLoop 已停止后立即关闭实时注入窗口；窗口关闭前已经到达的
        // 输入由 close_turn_inputs 转移到 thread 队列，避免终态化期间丢失。
        // 状态和句柄在同一 turn_threads 锁下线性化，旧 turn 不会再暴露 running。
        let (closed_status, closed_agent_loop_status) = match &run_result {
            Ok(status) => (
                turn_status_for_agent(&status.status),
                status.status.as_str(),
            ),
            Err(_) => (TurnStatus::Failed, AgentStatus::Failed.as_str()),
        };
        self.close_turn_inputs(
            &turn_id,
            &record.session_id,
            closed_status,
            closed_agent_loop_status,
        );
        let status = match run_result {
            Ok(status)
                if matches!(
                    status.status,
                    AgentStatus::Completed | AgentStatus::Cancelled
                ) =>
            {
                status
            }
            Ok(status) => {
                let error =
                    AppServerError::Agent(AgentError::Loop(status.error.clone().unwrap_or_else(
                        || "agent loop did not reach a terminal result".to_string(),
                    )));
                return self.finish_agent_failure(
                    &record,
                    &turn_id,
                    &assistant_events,
                    &error,
                    &status.model_usage,
                    &mut emit,
                );
            }
            Err(error) => {
                return self.finish_agent_failure(
                    &record,
                    &turn_id,
                    &assistant_events,
                    &error,
                    &ModelUsage::default(),
                    &mut emit,
                );
            }
        };
        let terminal_turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: record.session_id.clone(),
            status: turn_status_for_agent(&status.status),
            agent_loop_status: status.status.as_str().to_string(),
            model_usage: None,
        };
        if let Err(error) = self.update_session_status_and_usage(
            &record.session_id,
            session_status_for_agent(&status.status),
            &status.model_usage,
        ) {
            let failure = TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: Some(error.to_string()),
            };
            // durable terminal metadata is authoritative. If the intended status
            // cannot be written, converge to failed/interrupted before exposing
            // any terminal event, then report the metadata failure to the client.
            let _ = self.persist_failure_state(&record.session_id, &status.model_usage);
            self.remember_turn_status(&turn_id, TurnStatus::Failed, AgentStatus::Failed.as_str());
            let _event_failure = self.emit_failure_terminal_events(
                &turn_id,
                &record.session_id,
                &assistant_events,
                &failure,
                &mut emit,
            );
            return Err(AppServerError::TurnTerminalization {
                stage: failure.stage,
                cause: failure.cause,
                failure: TurnTerminalizationFailure::Store,
                original: failure.original,
            });
        }
        // Publication order: durable metadata first, then the in-process usage
        // projection used by the terminal event and RPC response.
        self.remember_usage(&turn_id, &status.model_usage);
        self.remember_turn_status(
            &turn_id,
            terminal_turn.status,
            &terminal_turn.agent_loop_status,
        );
        let terminal_turn = self.turn_with_usage(terminal_turn);
        let completion = self.event_notification(AppEvent::turn_completed(&terminal_turn));
        match completion {
            Ok(event) => emit(event),
            Err(error) => {
                let failure = turn_failure_from_error(&error, TurnFailureStage::EventNotification);
                // A failed completion notification must not leave clients without a
                // terminal event; best-effort turn/error is emitted before the RPC
                // error response is produced by the transport.
                let _ = self.emit_failure_terminal_events(
                    &turn_id,
                    &record.session_id,
                    &assistant_events,
                    &failure,
                    &mut emit,
                );
                return Err(AppServerError::TurnTerminalization {
                    stage: failure.stage,
                    cause: failure.cause,
                    failure: TurnTerminalizationFailure::EventNotification,
                    original: failure.original,
                });
            }
        }
        emit(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnStartResult {
                    turn: terminal_turn,
                })?,
            )
            .to_wire_value(),
        );
        Ok(())
    }

    /// 将 AgentLoop 错误收敛为唯一终态：先写 durable failure，再发 item/failed
    /// 与 turn/error，最后由调用方/transport 产生 RPC error response。
    fn finish_agent_failure(
        &self,
        record: &SessionRecord,
        turn_id: &str,
        assistant_events: &AssistantItemEventState,
        error: &AppServerError,
        usage: &ModelUsage,
        emit: &mut impl FnMut(Value),
    ) -> AppServerResult<()> {
        let failure = turn_failure_from_error(error, TurnFailureStage::AgentLoop);
        let (metadata_error, _durable) = self.persist_failure_state(&record.session_id, usage);
        self.remember_turn_status(turn_id, TurnStatus::Failed, AgentStatus::Failed.as_str());
        let event_failure = self.emit_failure_terminal_events(
            turn_id,
            &record.session_id,
            assistant_events,
            &failure,
            emit,
        );
        if metadata_error.is_some() {
            return Err(AppServerError::TurnTerminalization {
                stage: failure.stage,
                cause: failure.cause,
                failure: TurnTerminalizationFailure::Store,
                original: failure.original,
            });
        }
        if let Some(event_failure) = event_failure {
            return Err(AppServerError::TurnTerminalization {
                stage: event_failure.stage,
                cause: event_failure.cause,
                failure: TurnTerminalizationFailure::EventNotification,
                original: event_failure.original,
            });
        }
        Err(AppServerError::TurnExecution {
            stage: failure.stage,
            cause: failure.cause,
            original: failure.original,
        })
    }

    /// 首次失败记录后最多重试一次，并在必要时降级为 interrupted；返回首次
    /// durable 写失败文本，供 typed terminalization error 保留真实原因。
    fn persist_failure_state(
        &self,
        session_id: &str,
        usage: &ModelUsage,
    ) -> (Option<String>, bool) {
        let first_error =
            match self.update_session_status_and_usage(session_id, SessionStatus::Failed, usage) {
                Ok(_) => return (None, true),
                Err(error) => error.to_string(),
            };
        if self
            .update_session_status_and_usage(session_id, SessionStatus::Failed, usage)
            .is_ok()
        {
            return (Some(first_error), true);
        }
        let token_usage = match serde_json::to_value(usage_to_wire(usage)) {
            Ok(value) => value,
            Err(_) => return (Some(first_error), false),
        };
        let fallback = self.store.update_session(
            session_id,
            SessionMetadataUpdate {
                status: Some(SessionStatus::Interrupted),
                token_usage: Some(&token_usage),
                ..SessionMetadataUpdate::default()
            },
        );
        (Some(first_error), fallback.is_ok())
    }

    /// 尽力发送失败 item 与 turn 级终态事件；一个事件失败不阻断另一个事件，
    /// 返回首个 notification failure 供 RPC 错误分类。
    fn emit_failure_terminal_events(
        &self,
        turn_id: &str,
        thread_id: &str,
        assistant_events: &AssistantItemEventState,
        failure: &TurnFailure,
        emit: &mut impl FnMut(Value),
    ) -> Option<TurnFailure> {
        let mut first_failure = None;
        if assistant_events.appeared() {
            match self.realtime_item_failed_event(assistant_events) {
                Ok(Some(event)) => emit(event),
                Ok(None) => {}
                Err(error) => {
                    first_failure = Some(turn_failure_from_error(
                        &error,
                        TurnFailureStage::EventNotification,
                    ));
                }
            }
        }
        let message = failure
            .original
            .clone()
            .unwrap_or_else(|| format!("turn failed during {} ({})", failure.stage, failure.cause));
        let message = if singularity_core::contains_sensitive_text(&message) {
            "Internal error".to_string()
        } else {
            message
        };
        match self.event_notification(AppEvent::turn_error(
            turn_id,
            thread_id,
            failure.stage.as_str(),
            failure.cause.as_str(),
            &message,
            false,
        )) {
            Ok(event) => emit(event),
            Err(error) if first_failure.is_none() => {
                first_failure = Some(turn_failure_from_error(
                    &error,
                    TurnFailureStage::EventNotification,
                ));
            }
            Err(_) => {}
        }
        first_failure
    }

    /// 用 headless core 执行一个 turn：会话文件 open/create 已在 thread/start 完成，
    /// 这里只打开既有 rollout、执行 AgentLoop 并实时映射事件。
    pub(super) fn run_agent_core(
        &self,
        thread: &Thread,
        turn_id: &str,
        input_text: &str,
        cancellation: &CancellationToken,
        assistant_events: &mut AssistantItemEventState,
        emit: &mut impl FnMut(Value),
    ) -> AppServerResult<RunStatus> {
        let mut session = self.open_session_for_thread(thread)?;
        session
            .repair_orphaned_tool_calls()
            .map_err(AppServerError::Session)?;
        let (provider, config) = self.provider_and_config_for_thread(thread)?;
        let mut agent = Agent::new(provider, ToolRegistry::new(), config, session)?;
        let steer_handle = agent.steer_handle();
        let follow_up_handle = agent.follow_up_handle();
        // M2：把上一 turn 终态后到达的 thread 级待办（steer/followUp）取走注入本次
        // turn；无待办时为空操作。
        let thread_id = thread.thread_id.clone();
        for (pending, handle) in [
            (&self.thread_steer_pending, &steer_handle),
            (&self.thread_follow_up_pending, &follow_up_handle),
        ] {
            let queued = pending
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                .remove(&thread_id)
                .unwrap_or_default();
            for text in queued {
                handle
                    .lock()
                    .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                    .push_back(text);
            }
        }
        self.steer_handles
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .insert(turn_id.to_string(), steer_handle);
        self.follow_up_handles
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .insert(turn_id.to_string(), follow_up_handle);
        let callback_error = RefCell::new(None);
        // 回调闭包共享 emit 的可变借用：RefCell 包装（单线程 turn 内串行使用）。
        let emit_cell = RefCell::new(emit);
        let mut on_message_update = |delta: &str| {
            if callback_error.borrow().is_some() {
                return;
            }
            match self.project_assistant_delta(assistant_events, delta) {
                Ok(messages) => emit_messages(&mut *emit_cell.borrow_mut(), messages),
                Err(error) => *callback_error.borrow_mut() = Some(error),
            }
        };
        let mut on_tool_execution_start = |tool_name: &str, tool_call_id: &str, args: &Value| {
            if callback_error.borrow().is_some() {
                return;
            }
            match self.event_notification(AppEvent::tool_execution_start(
                tool_call_id,
                tool_name,
                args.clone(),
            )) {
                Ok(event) => emit_cell.borrow_mut()(event),
                Err(error) => *callback_error.borrow_mut() = Some(error),
            }
        };
        let mut on_tool_execution_update =
            |tool_name: &str, tool_call_id: &str, args: &Value, partial_result: &str| {
                if callback_error.borrow().is_some() {
                    return;
                }
                match self.event_notification(AppEvent::tool_execution_update(
                    tool_call_id,
                    tool_name,
                    args.clone(),
                    partial_result,
                )) {
                    Ok(event) => emit_cell.borrow_mut()(event),
                    Err(error) => *callback_error.borrow_mut() = Some(error),
                }
            };
        let mut on_tool_execution_end =
            |tool_name: &str, tool_call_id: &str, execution: &ToolExecution| {
                if callback_error.borrow().is_some() {
                    return;
                }
                match self.event_notification(AppEvent::tool_execution_end(
                    tool_call_id,
                    tool_name,
                    &execution.content,
                    execution.is_error,
                )) {
                    Ok(event) => emit_cell.borrow_mut()(event),
                    Err(error) => *callback_error.borrow_mut() = Some(error),
                }
            };
        let mut events = AgentEvents::new();
        events.on_message_update = Some(&mut on_message_update);
        events.on_tool_execution_start = Some(&mut on_tool_execution_start);
        events.on_tool_execution_update = Some(&mut on_tool_execution_update);
        events.on_tool_execution_end = Some(&mut on_tool_execution_end);
        let outcome = match agent.run(input_text, &mut events, cancellation) {
            Ok(outcome) => outcome,
            // provider 调用内的取消：按 AgentOutcome 的 aborted 语义收敛。
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
        Ok(outcome_to_run_status(outcome))
    }

    /// 同一连接内中断运行中的 turn。
    pub(super) fn turn_interrupt(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: TurnIdParams = parse_params(&message)?;
        let cancellation = self
            .active_turns
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&params.turn_id)
            .cloned();
        let Some(cancellation) = cancellation else {
            return not_found_response(message.required_id(), TURN_NOT_FOUND);
        };
        cancellation.cancel();
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(TurnInterruptResult {
                    turn_id: params.turn_id,
                    status: AgentStatus::CancelRequested.as_str().to_string(),
                    agent_loop_status: Some(AgentStatus::CancelRequested.as_str().to_string()),
                })?,
            )
            .to_wire_value(),
        ])
    }

    /// 同一连接内 steer：把输入注入下一轮开始的 steer 队列。
    pub(super) fn turn_steer(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.inject_turn_input(message, false)
    }

    /// 同一连接内 follow-up：把输入注入“代理准备停止时继续一轮”的队列。
    pub(super) fn turn_follow_up(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        self.inject_turn_input(message, true)
    }

    fn inject_turn_input(
        &self,
        message: JsonRpcMessage,
        follow_up: bool,
    ) -> AppServerResult<Vec<Value>> {
        let params: TurnInjectionParams = parse_params(&message)?;
        let payload = serde_json::to_value(&params.input)?;
        let text = input_items_to_text(&payload)?;
        // turn_threads 是状态转换的同步锁：终态化先更新 reference 再摘除句柄，
        // 注入请求不会在终态窗口中重新观察到 running。
        let (reference, outcome) = {
            let references = self
                .turn_threads
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
            let Some(reference) = references.get(&params.turn_id).cloned() else {
                return not_found_response(message.required_id(), TURN_NOT_FOUND);
            };
            let active = if reference.status == TurnStatus::Running {
                let handles = if follow_up {
                    &self.follow_up_handles
                } else {
                    &self.steer_handles
                };
                let handles = handles
                    .lock()
                    .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
                if let Some(handle) = handles.get(&params.turn_id).cloned() {
                    handle
                        .lock()
                        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                        .push_back(text.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            };
            (
                reference,
                if active {
                    TurnInjectionOutcome::Active
                } else {
                    TurnInjectionOutcome::Queued
                },
            )
        };
        if outcome == TurnInjectionOutcome::Queued {
            let queue = if follow_up {
                &self.thread_follow_up_pending
            } else {
                &self.thread_steer_pending
            };
            queue
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                .entry(reference.thread_id.clone())
                .or_default()
                .push_back(text);
        }
        json_response(
            message.required_id(),
            TurnInjectionResult {
                turn: Turn {
                    turn_id: params.turn_id,
                    thread_id: reference.thread_id,
                    status: reference.status,
                    agent_loop_status: reference.agent_loop_status,
                    model_usage: None,
                },
                outcome,
            },
        )
    }

    pub(super) fn agent_capability(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
            AgentCapabilityResult {
                provider_configuration: provider_configuration(&self.provider_snapshot),
            },
        )
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
