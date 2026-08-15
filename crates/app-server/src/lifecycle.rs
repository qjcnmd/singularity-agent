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
        // 信任门控：ask 未决且有交互 UI 时返回 -32010 trust_required（带 cwd）。
        if let TrustResolution::AskNeeded = self.resolve_thread_trust(&thread)? {
            let cwd = thread.cwd.clone().unwrap_or_default();
            emit_messages(
                &mut emit,
                trust_required_response(message.required_id(), &cwd)?,
            );
            return Ok(());
        }
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
        if record.title.is_none() && !title.is_empty() {
            self.store.update_session(
                &record.session_id,
                SessionMetadataUpdate {
                    title: Some(Some(&title)),
                    ..SessionMetadataUpdate::default()
                },
            )?;
        }
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
        let session = self.open_session_for_thread(thread)?;
        let (provider, config) = self.provider_and_config_for_thread(thread)?;
        let mut agent = Agent::new(provider, ToolRegistry::new(), config, session)?;
        let steer_handle = agent.steer_handle();
        let follow_up_handle = agent.follow_up_handle();
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
        }
        .ok_or_else(|| {
            AppServerError::Store(StoreError::NotFound(format!("turn {}", params.turn_id)))
        })?;
        handle
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .push_back(text);
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
                agent_loop: AgentLoopCapabilityStatus {
                    available: true,
                    status: AgentStatus::Completed.as_str().to_string(),
                    reason:
                        "AgentLoop uses the headless core; sandbox backend gating is not applied"
                            .to_string(),
                    blockers: Vec::new(),
                },
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
