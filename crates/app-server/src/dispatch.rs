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
            Method::ThreadRead => self.thread_read_legacy(message),
            Method::ThreadResume => self.thread_resume(message),
            Method::ThreadStart => self.thread_start(message),
            Method::ThreadFork => self.removed_thread_method(message),
            Method::ThreadArchive => self.removed_thread_method(message),
            Method::ThreadDelete => self.removed_thread_method(message),
            Method::TurnStart => self.turn_start(message),
            Method::TurnInput => self.removed_turn_method(message),
            Method::TurnPause => self.removed_turn_method(message),
            Method::TurnResume => self.removed_turn_method(message),
            Method::AgentCapability => self.agent_capability(message),
            Method::TurnInterrupt => self.turn_interrupt(message),
            Method::TurnStatus => self.turn_status_legacy(message),
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
                transports: vec![TransportCapability {
                    transport: "stdio".to_string(),
                    available: true,
                    auth_token_required: false,
                }],
            },
        )
    }

    pub(super) fn thread_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let threads = self
            .store
            .list_sessions()?
            .iter()
            .map(thread_from_record)
            .collect::<Vec<_>>();
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ThreadListResult { threads })?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn thread_resume(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadIdParams = parse_params(&message)?;
        let record = match self.store.get_session(&params.thread_id) {
            Ok(record) => record,
            Err(StoreError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let session = self.open_session_for_thread(&thread_from_record(&record))?;
        if session.path() != Path::new(&record.rollout_path) {
            return invalid_state_response(message.required_id(), SAFE_WORKSPACE_FAILURE);
        }
        let thread = thread_from_record(
            &self.store.update_session(
                &params.thread_id,
                SessionMetadataUpdate {
                    status: Some(SessionStatus::Active),
                    ..SessionMetadataUpdate::default()
                },
            )?,
        );
        json_response(message.required_id(), ThreadResult { thread })
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
        let model = match params.model.as_deref() {
            Some(model) => Some(model.to_string()),
            None => self.provider_snapshot.resolved_default_selector(),
        };
        let session_id = Uuid::now_v7().to_string();
        let session = SessionManager::create_with_id(
            Path::new(&cwd),
            &self.sessions_dir,
            &session_id,
        )
        .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))?;
        ensure_owner_only_file(session.path())
            .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))?;
        let rollout_path = session.path().to_string_lossy().to_string();
        let created_at = now_iso();
        let record = SessionRecord {
            session_id,
            rollout_path,
            cwd: cwd.clone(),
            title: None,
            model: model.clone(),
            status: SessionStatus::Active,
            created_at: created_at.clone(),
            updated_at: created_at,
            token_usage: json!({}),
        };
        self.store.insert_session(&record)?;
        let thread = thread_from_record(&record);
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

    // 以下旧协议方法会在 Phase 3 从 registry 删除；此前只返回稳定拒绝，
    // 不再触碰任何持久化状态机。
    fn removed_thread_method(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        invalid_state_response(
            message.required_id(),
            "thread read/fork/archive/delete are removed; use session/read or session/delete",
        )
    }

    fn removed_turn_method(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        invalid_state_response(
            message.required_id(),
            "turn status/pause/resume/input are removed; use turn/steer or turn/followUp",
        )
    }

    fn thread_read_legacy(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        invalid_state_response(
            message.required_id(),
            "thread/read is removed; use session/read",
        )
    }

    fn turn_status_legacy(&self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        not_found_response(message.required_id(), TURN_NOT_FOUND)
    }
}
