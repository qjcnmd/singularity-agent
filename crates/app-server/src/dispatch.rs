//! JSON-RPC registry validation and request dispatch handlers.

use super::*;

impl AppServer {
    /// 解析一行 JSON-RPC，并通过协议状态机进行分发。
    pub fn handle_json(&mut self, line: &str) -> AppServerResult<Vec<Value>> {
        let message: JsonRpcMessage = serde_json::from_str(line)?;
        self.handle(message)
    }

    /// 处理一个已解析的 JSON-RPC 请求，并返回零个或多个协议响应或事件。
    pub fn handle(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.handle_with_output(message)
    }

    /// 处理请求并返回生成的协议消息；单 worker 传输无需排序预留。
    pub fn handle_with_output(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<AppServerOutput>> {
        self.handle_unsequenced(message)
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
            Method::ProjectTrust => self.project_trust(message),
            Method::EventSubscribe => self.event_subscribe(message),
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
        // 恢复线程时确保绑定会话文件存在（缺失则创建空会话；旧 SQLite 历史不迁移）。
        if open_or_create_thread_session(&thread).is_err() {
            return invalid_state_response(message.required_id(), SAFE_WORKSPACE_FAILURE);
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
        // 无显式 model 时，若配置能无歧义解析默认 selector 则冻结到 Thread.model，
        // 防止重启后默认配置变化静默切换 model；无法解析时保留 NULL 契约。
        let model = match params.model.as_deref() {
            Some(model) => Some(model.to_string()),
            None => self.provider_snapshot.resolved_default_selector(),
        };
        let thread = self.store.create_thread(model.as_deref(), Some(&cwd))?;
        // 线程 ↔ 会话文件绑定：`<sessions_dir>/<thread_id>.jsonl`（Phase 3a 跨轮历史通道）。
        let thread_cwd = thread.cwd.clone().unwrap_or(cwd);
        let sessions_dir = Path::new(&thread_cwd)
            .join(".singularity")
            .join("agent-sessions");
        if SessionManager::create_with_name(
            Path::new(&thread_cwd),
            &sessions_dir,
            &thread.thread_id,
        )
        .is_err()
        {
            return invalid_state_response(message.required_id(), SAFE_WORKSPACE_FAILURE);
        }
        let mut messages = vec![self.event_notification(AppEvent::thread_started(&thread))?];
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
        // fork 冻结源 Thread 已确定的 selector；源为 NULL 时沿用与 thread/start
        // 相同的解析规则，不凭空写入未实际用于模型请求的值。
        let model = match selected_model {
            Some(model) => Some(model.to_string()),
            None => self.provider_snapshot.resolved_default_selector(),
        };
        let thread = self.store.create_thread(model.as_deref(), Some(&cwd))?;
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
            Err(StoreError::ThreadHasNonterminalTurn { turn_id, .. }) => invalid_state_response(
                message.required_id(),
                format!(
                    "thread already has an active or pending turn {turn_id}; use sg turn resume/pause/input {turn_id}"
                ),
            ),
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
            Err(StoreError::ThreadHasNonterminalTurn { turn_id, .. }) => invalid_state_response(
                message.required_id(),
                format!(
                    "thread already has an active or pending turn {turn_id}; use sg turn resume/pause/input {turn_id}"
                ),
            ),
            Err(error) => Err(error.into()),
        }
    }

    /// 查询/设置/重置项目信任决策（写 `<singularity_home>/trust.json`）。
    pub(super) fn project_trust(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ProjectTrustParams = parse_params(&message)?;
        let path = match canonical_thread_cwd(Some(&params.path)) {
            Ok(path) => path,
            Err(_) => return invalid_params_response(message.required_id()),
        };
        let mut decisions = self.trust_decisions();
        match params.decision {
            ProjectTrustDecision::Set(trusted) => {
                if decisions.set(Path::new(&path), trusted).is_err() {
                    return invalid_state_response(message.required_id(), SAFE_WORKSPACE_FAILURE);
                }
            }
            ProjectTrustDecision::Ask => {
                if decisions.remove(Path::new(&path)).is_err() {
                    return invalid_state_response(message.required_id(), SAFE_WORKSPACE_FAILURE);
                }
            }
            ProjectTrustDecision::Query => {}
        }
        json_response(
            message.required_id(),
            ProjectTrustResult {
                path: path.clone(),
                decision: decisions.get(Path::new(&path)),
            },
        )
    }
}
