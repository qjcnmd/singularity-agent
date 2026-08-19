//! AppServer construction, turn supervision, cancellation, and shutdown.

use super::dispatch::{
    input_items_to_text, invalid_params_response, json_response, not_found_response, parse_params,
    title_from_input,
};
use super::*;

fn emit_messages(emit: &mut impl FnMut(Value), messages: Vec<Value>) {
    for message in messages {
        emit(message);
    }
}
use std::cell::RefCell;

pub(super) fn agent_config_for_thread(
    thread: &Thread,
    provider: &dyn Provider,
    snapshot: &ProviderConfigSnapshot,
) -> AppServerResult<AgentConfig> {
    let cwd = workspace_path(thread)
        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))?;
    let system_prompt = match load_project_instructions_from_cwd(&cwd) {
        Ok(Some(instructions)) => instructions.content().to_string(),
        Ok(None) => String::new(),
        Err(error) => return Err(AppServerError::ProjectInstructions(error)),
    };
    let context_window = provider
        .protocol_contract()
        .max_context_tokens
        .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS) as u64;
    let max_output_tokens = provider.protocol_contract().max_output_tokens as u64;
    Ok(AgentConfig {
        model: thread
            .model
            .clone()
            .or_else(|| snapshot.resolved_default_selector())
            .unwrap_or_default(),
        system_prompt,
        context_window,
        max_output_tokens,
        ..AgentConfig::default()
    })
}

pub(super) fn outcome_to_run_status(outcome: AgentOutcome) -> RunStatus {
    let mut status = RunStatus::failed("agent loop did not reach a final assistant message");
    if outcome.aborted {
        mark_run_cancelled(&mut status);
    } else if outcome.final_text.trim().is_empty() {
        status.status = AgentStatus::Failed;
        status.error =
            Some("agent loop exhausted its turn budget without a final message".to_string());
    } else {
        status.status = AgentStatus::Completed;
        status.error = None;
        status.final_answer = Some(outcome.final_text.clone());
    }
    status.model_turns = outcome.turns;
    status.model_usage = outcome.usage;
    status
}

pub(super) fn provider_configuration(
    snapshot: &ProviderConfigSnapshot,
) -> ProviderConfigurationStatus {
    let config = snapshot.redacted_config();
    let configuration = snapshot.configuration();
    ProviderConfigurationStatus {
        source: snapshot.source().map(|source| source.as_str().to_string()),
        snapshot_id: snapshot.snapshot_id().to_string(),
        configured: configuration.configured,
        configuration_blocker: configuration
            .blocker
            .as_ref()
            .map(|blocker| blocker.code().to_string()),
        api_key_present: config.api_key_present,
        base_url_present: config.base_url_present,
        model_present: config.model_name.is_some(),
    }
}

pub(super) fn turn_failure_cause(error: &AppServerError) -> TurnFailureCause {
    match error {
        AppServerError::Store(_) => TurnFailureCause::Store,
        AppServerError::ProjectInstructions(_) => TurnFailureCause::ProjectInstructions,
        AppServerError::Workspace(_) => TurnFailureCause::Workspace,
        AppServerError::Agent(AgentError::Provider(provider_error)) => {
            TurnFailureCause::Provider(provider_failure_kind(&provider_error.error.kind))
        }
        AppServerError::Agent(_) => TurnFailureCause::Internal,
        AppServerError::InvalidJson(_) => TurnFailureCause::Serialization,
        AppServerError::InvalidParams(_) => TurnFailureCause::Internal,
        AppServerError::Session(_) => TurnFailureCause::Store,
        AppServerError::TurnExecution { cause, .. }
        | AppServerError::TurnTerminalization { cause, .. } => *cause,
    }
}

pub(super) fn provider_failure_kind(
    kind: &singularity_model::ModelErrorKind,
) -> ProviderFailureKind {
    use singularity_model::ModelErrorKind::*;
    match kind {
        RateLimited => ProviderFailureKind::RateLimited,
        BudgetExceeded => ProviderFailureKind::QuotaExceeded,
        NetworkError => ProviderFailureKind::Network,
        Timeout => ProviderFailureKind::Timeout,
        AuthError => ProviderFailureKind::Auth,
        InvalidRequest | ToolCallParseError | JsonSchemaViolation | ContentFilter => {
            ProviderFailureKind::Validation
        }
        ProviderOverloaded => ProviderFailureKind::Overloaded,
        Cancelled => ProviderFailureKind::Cancelled,
        ContextLengthExceeded => ProviderFailureKind::ContextOverflow,
        UnknownProviderError | UnsupportedCapability => ProviderFailureKind::Unknown,
    }
}

pub(super) fn turn_failure_from_error(
    error: &AppServerError,
    fallback_stage: TurnFailureStage,
) -> TurnFailure {
    match error {
        AppServerError::TurnExecution {
            stage,
            cause,
            original,
        }
        | AppServerError::TurnTerminalization {
            stage,
            cause,
            original,
            ..
        } => TurnFailure {
            stage: *stage,
            cause: *cause,
            original: original.clone().or_else(|| Some(error.to_string())),
        },
        _ => TurnFailure {
            stage: fallback_stage,
            cause: turn_failure_cause(error),
            original: Some(error.to_string()),
        },
    }
}

pub(super) fn turn_status_for_agent(status: &AgentStatus) -> TurnStatus {
    match status {
        AgentStatus::Completed => TurnStatus::Completed,
        AgentStatus::CancelRequested | AgentStatus::Cancelled => TurnStatus::Interrupted,
        AgentStatus::Running => TurnStatus::Running,
        AgentStatus::Failed => TurnStatus::Failed,
    }
}

pub(super) fn session_status_for_agent(status: &AgentStatus) -> SessionStatus {
    match status {
        AgentStatus::Completed => SessionStatus::Completed,
        AgentStatus::CancelRequested | AgentStatus::Cancelled => SessionStatus::Interrupted,
        AgentStatus::Running => SessionStatus::Active,
        AgentStatus::Failed => SessionStatus::Failed,
    }
}

pub(super) fn terminal_metadata_for_status(
    turn_id: &str,
    status: SessionStatus,
) -> Option<singularity_agent::session::SessionMetadata> {
    match status {
        SessionStatus::Completed => Some(
            singularity_agent::session::SessionMetadata::turn_completed(turn_id),
        ),
        SessionStatus::Failed => Some(singularity_agent::session::SessionMetadata::turn_failed(
            turn_id,
            "turn failed",
        )),
        SessionStatus::Interrupted => Some(
            singularity_agent::session::SessionMetadata::turn_interrupted(
                turn_id,
                "turn interrupted",
                false,
            ),
        ),
        SessionStatus::Active => None,
    }
}

pub(super) fn mark_run_cancelled(status: &mut RunStatus) {
    status.status = AgentStatus::Cancelled;
    status.final_answer = None;
    status.error = None;
}
pub(super) fn turn_id() -> String {
    Uuid::new_v4().to_string()
}

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
        // Repair only the pre-existing session state. The current turn has not
        // been appended yet, so it cannot be mistaken for a crashed turn.
        self.open_and_repair_session_for_thread(&thread)?;
        let title = title_from_input(&input_text);
        // JSONL is the authoritative lifecycle source: commit turn_started before
        // projecting Active into SQLite or publishing turn/started.
        self.append_turn_started_metadata(&record.session_id, &turn_id)?;
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
        // AgentLoop 已停止后立即关闭实时注入窗口；终态后的输入必须由客户端
        // 通过新的 turn/start 发送，不能在内存中静默排队。
        self.close_turn_inputs(&turn_id);
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
                    &mut assistant_events,
                    &error,
                    &status.model_usage,
                    &mut emit,
                );
            }
            Err(error) => {
                return self.finish_agent_failure(
                    &record,
                    &turn_id,
                    &mut assistant_events,
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
            Some(&turn_id),
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
            let _ = self.persist_failure_state(&record.session_id, &turn_id, &status.model_usage);
            let _event_failure = self.emit_failure_terminal_events(
                &turn_id,
                &record.session_id,
                &mut assistant_events,
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
        // Cancellation can interrupt a side-effecting tool after its item has
        // started but before the tool callback emits an execution-end event.
        // Close every such item before the turn terminal event; never leave a
        // client with an unpaired item/started notification.
        for tool_call_id in assistant_events.open_tool_items() {
            match self.realtime_tool_terminal_event(&mut assistant_events, &tool_call_id, true) {
                Ok(Some(event)) => emit(event),
                Ok(None) => {}
                Err(error) => {
                    let failure =
                        turn_failure_from_error(&error, TurnFailureStage::EventNotification);
                    let _ = self.emit_failure_terminal_events(
                        &turn_id,
                        &record.session_id,
                        &mut assistant_events,
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
        }
        match self.realtime_item_completed_event(&mut assistant_events) {
            Ok(Some(event)) => emit(event),
            Ok(None) => {}
            Err(error) => {
                let failure = turn_failure_from_error(&error, TurnFailureStage::EventNotification);
                let _ = self.emit_failure_terminal_events(
                    &turn_id,
                    &record.session_id,
                    &mut assistant_events,
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
                    &mut assistant_events,
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
        assistant_events: &mut AssistantItemEventState,
        error: &AppServerError,
        usage: &ModelUsage,
        emit: &mut impl FnMut(Value),
    ) -> AppServerResult<()> {
        let failure = turn_failure_from_error(error, TurnFailureStage::AgentLoop);
        let (metadata_error, _durable) =
            self.persist_failure_state(&record.session_id, turn_id, usage);
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
        turn_id: &str,
        usage: &ModelUsage,
    ) -> (Option<String>, bool) {
        let first_error = match self.update_session_status_and_usage(
            session_id,
            Some(turn_id),
            SessionStatus::Failed,
            usage,
        ) {
            Ok(_) => return (None, true),
            Err(error) => error.to_string(),
        };
        if self
            .update_session_status_and_usage(
                session_id,
                Some(turn_id),
                SessionStatus::Failed,
                usage,
            )
            .is_ok()
        {
            return (Some(first_error), true);
        }
        // Do not write a terminal SQLite projection without its JSONL fact. The
        // next reopen will repair an active turn from turn_started, while an
        // index-only fallback would violate the JSONL-first ordering contract.
        let _ = usage;
        (Some(first_error), false)
    }

    /// 尽力发送失败 item 与 turn 级终态事件；一个事件失败不阻断另一个事件，
    /// 返回首个 notification failure 供 RPC 错误分类。
    fn emit_failure_terminal_events(
        &self,
        turn_id: &str,
        thread_id: &str,
        assistant_events: &mut AssistantItemEventState,
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
        for tool_call_id in assistant_events.open_tool_items() {
            match self.realtime_tool_terminal_event(assistant_events, &tool_call_id, true) {
                Ok(Some(event)) => emit(event),
                Ok(None) => {}
                Err(error) if first_failure.is_none() => {
                    first_failure = Some(turn_failure_from_error(
                        &error,
                        TurnFailureStage::EventNotification,
                    ));
                }
                Err(_) => {}
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
        let assistant_events_cell = RefCell::new(assistant_events);
        // 回调闭包共享 emit 的可变借用：RefCell 包装（单线程 turn 内串行使用）。
        let emit_cell = RefCell::new(emit);
        let mut on_message_update = |delta: &str| {
            if callback_error.borrow().is_some() {
                return;
            }
            match self.project_assistant_delta(&mut assistant_events_cell.borrow_mut(), delta) {
                Ok(messages) => emit_messages(&mut *emit_cell.borrow_mut(), messages),
                Err(error) => *callback_error.borrow_mut() = Some(error),
            }
        };
        let mut on_tool_execution_start = |tool_name: &str, tool_call_id: &str, args: &Value| {
            if callback_error.borrow().is_some() {
                return;
            }
            if assistant_events_cell
                .borrow_mut()
                .start_tool_item(tool_call_id)
            {
                match self.event_notification(AppEvent::item_started(tool_call_id)) {
                    Ok(event) => emit_cell.borrow_mut()(event),
                    Err(error) => {
                        *callback_error.borrow_mut() = Some(error);
                        return;
                    }
                }
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
                if callback_error.borrow().is_some() {
                    return;
                }
                match self.realtime_tool_terminal_event(
                    &mut assistant_events_cell.borrow_mut(),
                    tool_call_id,
                    execution.is_error,
                ) {
                    Ok(Some(event)) => emit_cell.borrow_mut()(event),
                    Ok(None) => {}
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
        // turn_threads 只保留活动 turn；终态化先移除映射，再摘除句柄，
        // 因此注入请求不会在终态窗口中被确认。
        let references = self
            .turn_threads
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?;
        let Some(reference) = references.get(&params.turn_id).cloned() else {
            return not_found_response(message.required_id(), TURN_NOT_FOUND);
        };
        let handles = if follow_up {
            &self.follow_up_handles
        } else {
            &self.steer_handles
        };
        let handle = handles
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .get(&params.turn_id)
            .cloned();
        let Some(handle) = handle else {
            return not_found_response(message.required_id(), TURN_NOT_FOUND);
        };
        handle
            .lock()
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.into()))?
            .push_back(text);
        json_response(
            message.required_id(),
            TurnInjectionResult {
                turn: Turn {
                    turn_id: params.turn_id,
                    thread_id: reference.thread_id,
                    status: TurnStatus::Running,
                    agent_loop_status: AgentStatus::Running.as_str().to_string(),
                    model_usage: None,
                },
                outcome: TurnInjectionOutcome::Active,
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
