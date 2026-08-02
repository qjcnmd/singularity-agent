//! JSON-RPC registry validation and request dispatch handlers.

use super::*;
use singularity_protocol::{
    TraceListParams, TraceListResult, TraceMetricsParams, TraceMetricsResult, TraceShowParams,
    TraceShowResult, TraceTailParams,
};

impl AppServer {
    /// 解析一行 JSON-RPC，并通过协议状态机进行分发。
    pub fn handle_json(&mut self, line: &str) -> AppServerResult<Vec<Value>> {
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        self.handle(message)
    }

    /// 处理一个已解析的 JSON-RPC 请求，并返回零个或多个协议响应或事件。
    pub fn handle(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let outputs = self.handle_with_output(message)?;
        for output in &outputs {
            self.output_order.complete(output.reservation.order);
        }
        Ok(outputs.into_iter().map(|output| output.message).collect())
    }

    /// 处理请求并在生成消息时原子预留 stdout order 与事件 cursor。
    pub fn handle_with_output(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<AppServerOutput>> {
        self.pending_transport_trace_binding = None;
        let messages = self.handle_unsequenced(message)?;
        self.sequence_outputs(messages)
    }

    pub(super) fn handle_unsequenced(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let notification = message.is_notification();
        let id = message.id().cloned();
        let Some(method_name) = message.method_name() else {
            return if notification {
                Ok(Vec::new())
            } else {
                Ok(vec![JsonRpcMessage::invalid_request(id).to_wire_value()])
            };
        };
        let Some(method) = Method::parse(method_name) else {
            return if notification {
                Ok(Vec::new())
            } else {
                Ok(vec![JsonRpcMessage::method_not_found(id).to_wire_value()])
            };
        };

        // A notification-only registry entry may not be invoked as a request.
        // A request-only method sent without an id remains a JSON-RPC notification
        // and therefore keeps the no-response contract.
        if method.spec().kind == MethodKind::Notification && !notification {
            return Ok(vec![JsonRpcMessage::invalid_request(id).to_wire_value()]);
        }

        if method
            .spec()
            .validate_params(message.params().cloned().unwrap_or_else(|| json!({})))
            .is_err()
        {
            return if notification {
                Ok(Vec::new())
            } else {
                json_error(id, ErrorCode::invalid_params("Invalid params"))
            };
        }

        if matches!(method, Method::Initialized) && !self.initialized {
            return if notification {
                Ok(Vec::new())
            } else {
                json_error(id, ErrorCode::not_initialized())
            };
        }
        if !matches!(method, Method::Initialize | Method::Initialized)
            && !self.initialized_acknowledged
        {
            return if notification {
                Ok(Vec::new())
            } else {
                json_error(id, ErrorCode::not_initialized())
            };
        }

        let message = if notification {
            message.into_request_with_id(JsonRpcId::Number(0))
        } else {
            message
        };

        let result = match method {
            Method::Initialize => self.initialize(message),
            Method::Initialized => {
                self.initialized_acknowledged = true;
                json_response(message.required_id(), singularity_protocol::EmptyResult {})
            }
            Method::ServerCapabilities => self.server_capabilities(message),
            Method::ThreadList => self.thread_list(message),
            Method::ThreadRead => self.thread_read(message),
            Method::ThreadResume => self.thread_resume(message),
            Method::ThreadStart => self.thread_start(message),
            Method::ThreadFork => self.thread_fork(message),
            Method::ThreadArchive => self.thread_archive(message),
            Method::ThreadDelete => self.thread_delete(message),
            Method::TurnStart => self.turn_start(message),
            Method::TurnInput => self.turn_input(message),
            Method::TurnPause => self.turn_pause(message),
            Method::TurnResume => self.turn_resume(message),
            Method::AgentCapability => self.agent_capability(message),
            Method::TurnInterrupt => self.turn_interrupt(message),
            Method::TurnStatus => self.turn_status(message),
            Method::ApprovalList => self.approval_list(message),
            Method::ApprovalCenter => self.approval_center(message),
            Method::ApprovalRequest => self.approval_request(message),
            Method::ApprovalDecision => self.approval_decision(message),
            Method::EventSubscribe => self.event_subscribe(message),
            Method::ArtifactFetch => self.artifact_fetch(message),
            Method::TraceList => self.trace_list(message),
            Method::TraceShow => self.trace_show(message),
            Method::TraceTail => self.trace_tail(message),
            Method::TraceMetrics => self.trace_metrics(message),
            Method::ServerShutdown => self.server_shutdown(message),
        };
        if notification {
            return Ok(Vec::new());
        }
        match result {
            Err(AppServerError::InvalidParams(error)) => {
                json_error(id, ErrorCode::invalid_params(error))
            }
            result => result,
        }
    }

    pub(super) fn initialize(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        if self.initialized {
            return Ok(vec![
                JsonRpcMessage::error(message.required_id(), ErrorCode::already_initialized())
                    .to_wire_value(),
            ]);
        }
        let _params: InitializeParams = parse_params(&message)?;
        self.initialized = true;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(InitializeResult::local())?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn server_capabilities(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
            ServerCapabilitiesResult {
                transports: vec![
                    TransportCapability {
                        transport: "stdio".to_string(),
                        available: true,
                        auth_token_required: false,
                    },
                    TransportCapability {
                        transport: "websocket".to_string(),
                        available: false,
                        auth_token_required: true,
                    },
                ],
            },
        )
    }

    pub(super) fn thread_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let threads = self.store.list_threads()?;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ThreadListResult { threads })?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn thread_read(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadReadParams = parse_params(&message)?;
        let turn_limit = match history_turn_limit(params.limit) {
            Ok(limit) => limit,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        match self.store.read_thread_history(
            &params.thread_id,
            params.before_turn_sequence,
            turn_limit,
        ) {
            Ok(history) => json_response(
                message.required_id(),
                ThreadReadResult {
                    thread,
                    messages: history.messages,
                    next_before_turn_sequence: history.next_before_turn_sequence,
                },
            ),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn thread_resume(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        let thread = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = workspace_tools_for_thread(&thread, Arc::clone(&self.sandbox_backend)) {
            return invalid_state_response(message.required_id(), error);
        }
        match self.store.update_thread_status(
            &params.thread_id,
            singularity_protocol::ThreadStatus::Active,
        ) {
            Ok(thread) => json_response(message.required_id(), ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }
    pub(super) fn thread_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadStartParams = parse_params(&message)?;
        let cwd = match canonical_thread_cwd(params.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        if self
            .validate_model_selector(params.model.as_deref())
            .is_err()
        {
            return invalid_params_response(message.required_id());
        }
        let (thread, _trace) = self.store.create_thread_with_trace_and_policy(
            params.model.as_deref(),
            Some(&cwd),
            params
                .sandbox_mode
                .unwrap_or(PermissionProfileName::WorkspaceWrite),
            params.approval_policy.unwrap_or(ApprovalPolicy::OnRequest),
            "app_server",
            "thread started",
        )?;
        let mut messages = Vec::new();
        if let Some(event) = self.event_notification(AppEvent::thread_started(&thread))? {
            messages.push(event);
        }
        messages.push(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ThreadStartResult {
                    thread: thread.clone(),
                })?,
            )
            .to_wire_value(),
        );
        Ok(messages)
    }

    pub(super) fn thread_fork(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadForkParams = parse_params(&message)?;
        let source = match self.store.get_thread(&params.thread_id) {
            Ok(thread) => thread,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let source_cwd = match params.cwd.as_deref().or(source.cwd.as_deref()) {
            Some(cwd) => cwd,
            None => {
                return invalid_state_response(
                    message.required_id(),
                    "source thread does not have an absolute workspace",
                );
            }
        };
        let cwd = match canonical_thread_cwd(Some(source_cwd)) {
            Ok(cwd) => cwd,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        let selected_model = params.model.as_deref().or(source.model.as_deref());
        if self.validate_model_selector(selected_model).is_err() {
            return invalid_params_response(message.required_id());
        }
        let thread = self.store.create_thread_with_policy(
            selected_model,
            Some(&cwd),
            params.sandbox_mode.unwrap_or(source.sandbox_mode),
            params.approval_policy.unwrap_or(source.approval_policy),
        )?;
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ThreadForkResult {
                    source_thread_id: params.thread_id,
                    thread,
                })?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn thread_archive(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        match self.store.update_thread_status(
            &params.thread_id,
            singularity_protocol::ThreadStatus::Archived,
        ) {
            Ok(thread) => json_response(message.required_id(), ThreadResult { thread }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(StoreError::ThreadHasNonterminalTurn { .. }) => {
                invalid_state_response(message.required_id(), THREAD_EXECUTION_ACTIVE)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn thread_delete(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        match self.store.delete_thread(&params.thread_id) {
            Ok(()) => Ok(vec![
                JsonRpcMessage::response(
                    message.required_id(),
                    serde_json::to_value(ThreadDeleteResult {
                        thread_id: params.thread_id,
                        deleted: true,
                    })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), THREAD_NOT_FOUND)
            }
            Err(StoreError::ThreadHasNonterminalTurn { .. }) => {
                invalid_state_response(message.required_id(), THREAD_EXECUTION_ACTIVE)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn artifact_fetch(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: ArtifactFetchParams = parse_params(&message)?;
        match self.store.get_artifact_ref(&params.artifact_id) {
            Ok(artifact) => json_response(message.required_id(), ArtifactFetchResult { artifact }),
            Err(StoreError::NotFound(_) | StoreError::InvalidState(_)) => {
                not_found_response(message.required_id(), ARTIFACT_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn trace_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceListParams = parse_params(&message)?;
        match self
            .store
            .list_trace_page(&params.run_id, params.limit, params.offset)
        {
            Ok(events) => json_response(message.required_id(), TraceListResult { events }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_RUN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn trace_show(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceShowParams = parse_params(&message)?;
        match self.store.show_trace(&params.event_id) {
            Ok(event) => json_response(message.required_id(), TraceShowResult { event }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_EVENT_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn trace_tail(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceTailParams = parse_params(&message)?;
        match self
            .store
            .tail_trace(&params.run_id, params.limit.unwrap_or(50), params.offset)
        {
            Ok(events) => Ok(vec![
                JsonRpcMessage::response(
                    message.required_id(),
                    serde_json::to_value(TraceListResult { events })?,
                )
                .to_wire_value(),
            ]),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_RUN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn trace_metrics(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: TraceMetricsParams = parse_params(&message)?;
        match self.store.trace_metrics(&params.run_id) {
            Ok(metrics) => json_response(message.required_id(), TraceMetricsResult { metrics }),
            Err(StoreError::NotFound(_)) => {
                not_found_response(message.required_id(), TRACE_RUN_NOT_FOUND)
            }
            Err(error) => Err(error.into()),
        }
    }
}
