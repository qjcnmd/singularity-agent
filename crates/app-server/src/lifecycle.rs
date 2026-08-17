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
        let status = match self.run_agent_core(
            &thread,
            &turn_id,
            &input_text,
            &cancellation,
            &mut assistant_events,
            &mut emit,
        ) {
            Ok(status) => status,
            Err(error) => {
                self.emit_realtime_item_failure(&mut emit, Some(&assistant_events))?;
                let failure = turn_failure_from_error(&error, TurnFailureStage::AgentLoop);
                let _ = self.store.update_session(
                    &record.session_id,
                    SessionMetadataUpdate {
                        status: Some(SessionStatus::Failed),
                        ..SessionMetadataUpdate::default()
                    },
                );
                // M1/H4：失败 turn 发出 turn 级终态错误事件（typed cause + 重试
                // 状态，对齐 Codex ErrorNotification）；消息按传输边界同样脱敏。
                let message = failure.original.clone().unwrap_or_default();
                let message = if singularity_core::contains_sensitive_text(&message) {
                    "Internal error".to_string()
                } else {
                    message
                };
                emit(self.event_notification(AppEvent::turn_error(
                    &turn_id,
                    &record.session_id,
                    failure.stage.as_str(),
                    failure.cause.as_str(),
                    &message,
                    false,
                ))?);
                return Err(AppServerError::TurnExecution {
                    stage: failure.stage,
                    cause: failure.cause,
                    original: failure.original,
                });
            }
        };
        let terminal_turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: record.session_id.clone(),
            status: turn_status_for_agent(&status.status),
            agent_loop_status: status.status.as_str().to_string(),
            model_usage: None,
        };
        self.remember_usage(&turn_id, &status.model_usage);
        let terminal_turn = self.turn_with_usage(terminal_turn);
        self.update_session_status_and_usage(
            &record.session_id,
            session_status_for_agent(&status.status),
            &status.model_usage,
        )?;
        emit(self.event_notification(AppEvent::turn_completed(&terminal_turn))?);
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
        let mut on_message_update = |delta: &str| {
            if callback_error.borrow().is_some() {
                return;
            }
            match self.project_assistant_delta(assistant_events, delta) {
                Ok(messages) => emit_messages(emit, messages),
                Err(error) => *callback_error.borrow_mut() = Some(error),
            }
        };
        let mut events = AgentEvents::new();
        events.on_message_update = Some(&mut on_message_update);
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
        let handle = if follow_up {
            self.follow_up_handles
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                .get(&params.turn_id)
                .cloned()
        } else {
            self.steer_handles
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                .get(&params.turn_id)
                .cloned()
        };
        match handle {
            // 有 turn 在跑：实时注入。
            Some(handle) => {
                handle
                    .lock()
                    .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                    .push_back(text);
            }
            // turn 已终态（句柄已摘除）：按 turn→thread 历史映射入 thread 级待办
            // 队列，下一次 turn/start 取走；不再返回 not found（Pi 式 thread 队列）。
            None => {
                let thread_id = self
                    .turn_threads
                    .lock()
                    .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                    .get(&params.turn_id)
                    .cloned();
                let Some(thread_id) = thread_id else {
                    return not_found_response(message.required_id(), TURN_NOT_FOUND);
                };
                let queue = if follow_up {
                    &self.thread_follow_up_pending
                } else {
                    &self.thread_steer_pending
                };
                queue
                    .lock()
                    .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
                    .entry(thread_id)
                    .or_default()
                    .push_back(text);
            }
        }
        let thread_id = self
            .turn_threads
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&params.turn_id)
            .cloned()
            .unwrap_or_default();
        json_response(
            message.required_id(),
            TurnResult {
                turn: Turn {
                    turn_id: params.turn_id,
                    thread_id,
                    status: TurnStatus::Running,
                    agent_loop_status: AgentStatus::Running.as_str().to_string(),
                    model_usage: None,
                },
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
