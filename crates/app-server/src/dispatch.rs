//! JSON-RPC registry validation and request dispatch handlers.

use super::*;

pub(super) fn json_response<T: serde::Serialize>(
    id: JsonRpcId,
    result: T,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::response(id, serde_json::to_value(result)?).to_wire_value(),
    ])
}

pub(super) fn input_items_to_text(input: &Value) -> AppServerResult<String> {
    let items: Vec<singularity_protocol::InputItem> =
        serde_json::from_value(input.clone()).map_err(AppServerError::InvalidJson)?;
    let text = items
        .into_iter()
        .map(|item| match item {
            singularity_protocol::InputItem::Text { text } => text,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(AppServerError::Workspace(
            "persisted turn input is empty".to_string(),
        ));
    }
    Ok(text)
}

pub(super) fn title_from_input(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_SESSION_TITLE_CHARS)
        .collect()
}

pub(super) fn compose_model_selector(
    provider: &str,
    model: &str,
    reasoning: Option<&str>,
) -> String {
    let mut selector = format!("{provider}/{model}");
    if let Some(reasoning) = reasoning {
        selector.push('#');
        selector.push_str(reasoning);
    }
    selector
}

pub(super) fn json_error(id: Option<JsonRpcId>, error: ErrorCode) -> AppServerResult<Vec<Value>> {
    Ok(vec![JsonRpcMessage::error(id, error).to_wire_value()])
}

pub(super) fn parse_params<T>(message: &JsonRpcMessage) -> Result<T, AppServerError>
where
    T: serde::de::DeserializeOwned,
{
    message
        .params_as()
        .map_err(|_| AppServerError::InvalidParams("Invalid params".to_string()))
}

pub(super) fn not_found_response(
    id: JsonRpcId,
    message: &'static str,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::not_found(message)).to_wire_value(),
    ])
}

pub(super) fn invalid_state_response(
    id: JsonRpcId,
    message: impl Into<String>,
) -> AppServerResult<Vec<Value>> {
    Ok(vec![
        JsonRpcMessage::error(id, ErrorCode::new(APP_ERROR_INVALID_STATE, message)).to_wire_value(),
    ])
}

pub(super) fn invalid_params_response(id: JsonRpcId) -> AppServerResult<Vec<Value>> {
    json_error(Some(id), ErrorCode::invalid_params("Invalid params"))
}

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

    /// 处理请求并返回生成的协议消息；传输 writer 负责全局输出顺序，dispatch 不假设 turn 执行顺序。
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

        // Request-only 方法以 notification（无 id）提交时必须静默忽略：
        // 不执行任何副作用，也不能伪造一个带 null id 的错误响应。
        if method.spec().kind == MethodKind::Request && notification {
            return Ok(Vec::new());
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
            Method::ThreadList => self.thread_list(message),
            Method::ThreadStart => self.thread_start(message),
            Method::SessionRead => self.session_read(message),
            Method::SessionDelete => self.session_delete(message),
            Method::ThreadSettings => self.thread_settings(message),
            Method::TurnStart => self.turn_start(message),
            Method::TurnSteer => self.turn_steer(message),
            Method::TurnFollowUp => self.turn_follow_up(message),
            Method::ProviderStatus => self.provider_status(message),
            Method::TurnInterrupt => self.turn_interrupt(message),
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

    pub(super) fn thread_list(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let threads = self
            .store
            .list_sessions()?
            .iter()
            .map(|record| self.project_thread(record))
            .collect::<Vec<_>>();
        Ok(vec![
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ThreadListResult { threads })?,
            )
            .to_wire_value(),
        ])
    }

    pub(super) fn thread_settings(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: ThreadSettingsParams = parse_params(&message)?;
        let record = match self.store.get_session(&params.thread_id) {
            Ok(record) => record,
            Err(SessionIndexError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let parts = singularity_model::split_model_selector(record.model.as_deref().unwrap_or(""));
        let current_provider = parts.provider.map(str::to_string);
        let current_model = parts.model.map(str::to_string);
        let current_reasoning = parts.effort.map(str::to_string);
        let changed =
            params.provider.is_some() || params.model.is_some() || params.reasoning.is_some();
        let provider = params
            .provider
            .or(current_provider)
            .unwrap_or_else(|| "openai_compatible".to_string());
        let model = params.model.or(current_model);
        let reasoning = params.reasoning.or(current_reasoning);
        let Some(model) = model.filter(|model| !model.trim().is_empty()) else {
            return invalid_params_response(message.required_id());
        };
        if provider.trim().is_empty()
            || provider.chars().any(char::is_whitespace)
            || model.chars().any(char::is_whitespace)
            || reasoning.as_deref().is_some_and(|value| {
                value.trim().is_empty() || value.chars().any(char::is_whitespace)
            })
        {
            return invalid_params_response(message.required_id());
        }
        let selector = compose_model_selector(&provider, &model, reasoning.as_deref());
        self.validate_model_selector(Some(&selector))?;
        if changed {
            let settings = singularity_agent::session::SessionMetadata::thread_settings(
                provider.clone(),
                model.clone(),
                reasoning.clone(),
            )?;
            let mut session = self.open_session_for_thread(&thread_from_record(&record))?;
            session.append_metadata(settings)?;
            self.store.update_session(
                &record.session_id,
                SessionMetadataUpdate {
                    model: Some(Some(&selector)),
                    ..SessionMetadataUpdate::default()
                },
            )?;
        }
        json_response(
            message.required_id(),
            ThreadSettingsResult {
                thread_id: record.session_id,
                provider: Some(provider),
                model: Some(model),
                reasoning,
                updated: changed,
            },
        )
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
        let session =
            SessionManager::create_with_id(Path::new(&cwd), &self.sessions_dir, &session_id)
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
            // 尚无 turn：status 为 null，首个 turn 真正开始时才写入。
            status: None,
            created_at: created_at.clone(),
            updated_at: created_at,
            token_usage: json!({}),
        };
        self.store.insert_session(&record)?;
        let thread = self.project_thread(&record);
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

    pub(super) fn session_read(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: SessionReadParams = parse_params(&message)?;
        if !(1..=200).contains(&params.limit) {
            return invalid_params_response(message.required_id());
        }
        let record = match self.store.get_session(&params.session_id) {
            Ok(record) => record,
            Err(SessionIndexError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        let repository = SessionRepository::new(self.sessions_dir.clone());
        let read = repository
            .read(&record.session_id)
            .map_err(AppServerError::Session)?;
        // 与 thread/list 复用同一 last-turn 投影，
        // 两个读取接口不得显示互相矛盾的状态。
        let overall_status = self.project_thread(&record).last_turn_status;
        let mut turns = project_turn_history(&read.entries);
        // 整体状态一致性：末组 running 只有在整体 active（存在存活 turn）
        // 时保留；崩溃遗留投影为 interrupted。
        if overall_status != Some(ThreadStatus::Active)
            && turns
                .last_mut()
                .is_some_and(|last| last.status == Some(TurnStatus::Running))
        {
            turns.last_mut().expect("checked above").status = Some(TurnStatus::Interrupted);
        }
        // 单向往回读分页：默认取最新 limit 轮；给 beforeItem（上一页最旧轮内
        // 任意 item 的公开 id）则定位其所属轮，返回该轮之前的 limit 轮。
        let total_turns = turns.iter().filter(|turn| turn.turn_id.is_some()).count();
        let before_index = match params.before_item.as_deref() {
            None => None,
            Some(anchor) => match turns
                .iter()
                .position(|turn| turn.items.iter().any(|item| item.id() == anchor))
            {
                Some(index) => Some(index),
                None => return invalid_params_response(message.required_id()),
            },
        };
        let page_start = before_index
            .unwrap_or(turns.len())
            .saturating_sub(params.limit as usize);
        let page_end = before_index.unwrap_or(turns.len());
        json_response(
            message.required_id(),
            SessionReadResult {
                session_id: record.session_id,
                cwd: record.cwd,
                title: record.title,
                model: record.model,
                status: overall_status,
                created_at: record.created_at,
                updated_at: record.updated_at,
                token_usage: record.token_usage,
                summary: read.summary,
                turns: turns[page_start..page_end].to_vec(),
                total_turns,
            },
        )
    }

    pub(super) fn session_delete(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        let params: SessionIdParams = parse_params(&message)?;
        let record = match self.store.get_session(&params.session_id) {
            Ok(record) => record,
            Err(SessionIndexError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(error) => return Err(error.into()),
        };
        // 会话仍有存活 turn 时拒绝删除：worker 可能正持句柄 append，删除会让
        // 后续写入落入 unlinked inode（索引行已删，turn 终态更新打空）。
        let turn_active = {
            let active_turns = self
                .active_turns
                .lock()
                .map_err(|_| AppServerError::Workspace(SAFE_WORKSPACE_FAILURE.to_string()))?;
            active_turns
                .values()
                .any(|turn| turn.thread_id == params.session_id)
        };
        if turn_active {
            return invalid_state_response(message.required_id(), SESSION_DELETE_TURN_ACTIVE);
        }
        // 打开并校验 rollout header 后再进入删除；不能先永久删 JSONL。
        let _session = self.open_session_for_thread(&thread_from_record(&record))?;
        crate::delete::delete_session(&record, &self.store)?;
        json_response(
            message.required_id(),
            SessionDeleteResult {
                session_id: params.session_id,
                deleted: true,
            },
        )
    }
}
