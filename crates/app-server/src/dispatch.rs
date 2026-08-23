//! JSON-RPC registry validation and request dispatch handlers.

use singularity_agent::session::{SessionManager, SessionRepository};
use singularity_model::ProviderConfigSnapshot;

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
            // JSONL 先落盘（协调器负责校验与持久化），随后同步索引投影。
            let conversation = self.conversation_for(&record.session_id)?;
            let patch = singularity_runtime::SettingsPatch {
                provider: Some(provider.clone()),
                model: Some(model.clone()),
                reasoning: reasoning.clone(),
            };
            if let Err(error) = conversation.queue_settings(patch) {
                let message = match error {
                    singularity_runtime::ConversationError::Settings(message) => {
                        format!("invalid model selector: {message}")
                    }
                    other => other.to_string(),
                };
                return Err(AppServerError::InvalidParams(message));
            }
            let updated_model = conversation.thread().ok().and_then(|t| t.model);
            if let Some(updated_model) = updated_model {
                self.store.update_session(
                    &record.session_id,
                    SessionMetadataUpdate {
                        model: Some(Some(&updated_model)),
                        ..SessionMetadataUpdate::default()
                    },
                )?;
            }
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
        let cwd = match singularity_runtime::store::canonical_thread_cwd(params.cwd.as_deref()) {
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
            None => self.turn_runner.default_model_selector(),
        };
        let thread = match singularity_runtime::store::create_thread(
            &self.sessions_dir,
            &cwd,
            model.clone(),
        ) {
            Ok(thread) => thread,
            Err(_) => {
                return Err(AppServerError::Workspace(
                    SAFE_WORKSPACE_FAILURE.to_string(),
                ));
            }
        };
        let rollout_path =
            singularity_runtime::store::thread_session_path(&self.sessions_dir, &thread.thread_id)
                .to_string_lossy()
                .to_string();
        let created_at = now_iso();
        let record = SessionRecord {
            session_id: thread.thread_id.clone(),
            rollout_path,
            cwd: cwd.clone(),
            title: None,
            model,
            // 尚无 turn：status 为 null，首个 turn 真正开始时才写入。
            status: None,
            created_at: created_at.clone(),
            updated_at: created_at,
            token_usage: json!({}),
        };
        self.store.insert_session(&record)?;
        let protocol_thread = self.project_thread(&record);
        let mut messages =
            vec![self.event_notification(AppEvent::thread_started(&protocol_thread))?];
        messages.push(
            JsonRpcMessage::response(
                message.required_id(),
                serde_json::to_value(ThreadStartResult {
                    thread: protocol_thread.clone(),
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
        if self.thread_turn_active(&record.session_id) {
            return invalid_state_response(message.required_id(), SESSION_DELETE_TURN_ACTIVE);
        }
        // 打开并校验 rollout header 后再进入删除；不能先永久删 JSONL。
        let session = SessionManager::open_existing(Path::new(&record.rollout_path))
            .map_err(AppServerError::Session)?;
        if session.session_id() != record.session_id {
            return Err(AppServerError::Store(SessionIndexError::InvalidState(
                format!(
                    "rollout header id {} does not match index session id {}",
                    session.session_id(),
                    record.session_id
                ),
            )));
        }
        drop(session);
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

/// turn/start 请求路由裁定（stdio 二进制与测试共用）。
pub enum TurnClaim {
    /// 已获得活动窗口预订；worker 线程负责消费执行。
    Accepted(TurnStartClaim),
    /// 需要直接回给客户端的响应（thread 未找到 / 并发占用 / 参数无效）。
    Responded(Value),
}

impl std::fmt::Debug for TurnClaim {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accepted(_) => formatter.write_str("Accepted(_)"),
            Self::Responded(value) => formatter.debug_tuple("Responded").field(value).finish(),
        }
    }
}

/// 已预订的 turn/start：把输入、投影头与预订一起交给执行线程。
pub struct TurnStartClaim {
    pub reservation: singularity_runtime::TurnReservation,
    pub request_id: JsonRpcId,
    pub thread_id: String,
    pub title: Option<String>,
    pub input: String,
}

impl AppServer {
    /// 路由裁定；二进制与库测试共用（`#[doc(hidden)]` 暴露给 stdio transport）。
    #[doc(hidden)]
    pub fn claim_turn(&self, message: JsonRpcMessage) -> AppServerResult<TurnClaim> {
        if message.method_name() != Some(Method::TurnStart.as_str()) {
            return Err(AppServerError::InvalidParams(
                "claim requires turn/start".to_string(),
            ));
        }
        let params: TurnStartParams = parse_params(&message)?;
        let record = match self.store.get_session(&params.thread_id) {
            Ok(record) => record,
            Err(SessionIndexError::NotFound(_)) => {
                return Ok(TurnClaim::Responded(
                    not_found_response(message.required_id(), THREAD_NOT_FOUND)?.remove(0),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let payload = serde_json::to_value(&params.input)?;
        let input_text = match input_items_to_text(&payload) {
            Ok(text) => text,
            Err(_) => {
                return Ok(TurnClaim::Responded(
                    invalid_params_response(message.required_id())?.remove(0),
                ));
            }
        };
        // 协调器按 JSONL 重开（含崩溃修复）并持有全程单写者语义。
        let conversation = self.conversation_for(&record.session_id)?;
        let title = title_from_input(&input_text);
        match conversation.reserve_start() {
            Ok(reservation) => Ok(TurnClaim::Accepted(TurnStartClaim {
                reservation,
                request_id: message.required_id(),
                thread_id: record.session_id,
                title: Some(title),
                input: input_text,
            })),
            Err(singularity_runtime::ConversationError::TurnAlreadyActive) => {
                Ok(TurnClaim::Responded(
                    invalid_state_response(
                        message.required_id(),
                        "another turn is already running for this session",
                    )?
                    .remove(0),
                ))
            }
            Err(singularity_runtime::ConversationError::Settings(message)) => Err(
                AppServerError::InvalidParams(format!("invalid model selector: {message}")),
            ),
            Err(singularity_runtime::ConversationError::Turn(error)) => {
                // 预订入口不可能产生执行/终态化错误；防御性内部映射。
                Err(AppServerError::TurnExecution {
                    stage: TurnFailureStage::AgentLoop,
                    cause: TurnFailureCause::Internal,
                    original: Some(error.to_string()),
                })
            }
        }
    }

    pub(crate) fn turn_start(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let mut messages = Vec::new();
        self.run_turn_start(message, &mut |output| messages.push(output))?;
        Ok(messages)
    }

    /// 执行 `turn/start`：委托共享核心运行 turn，事件经投影适配器流出。
    pub fn handle_turn_start_streaming_with_output(
        &mut self,
        message: JsonRpcMessage,
        mut emit: impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        self.run_turn_start(message, &mut emit)
    }

    fn run_turn_start(
        &mut self,
        message: JsonRpcMessage,
        emit: &mut impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        match self.claim_turn(message)? {
            TurnClaim::Accepted(claim) => self.run_turn_started(claim, emit),
            TurnClaim::Responded(response) => {
                emit(response);
                Ok(())
            }
        }
    }

    /// 以已预订的协调器执行整条 turn 链条（worker 线程入口）；终态与 usage
    /// 索引同步在投影内完成。`#[doc(hidden)]` 暴露给 stdio transport。
    #[doc(hidden)]
    pub fn run_turn_started(
        &mut self,
        claim: TurnStartClaim,
        emit: &mut impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let TurnStartClaim {
            reservation,
            request_id,
            thread_id,
            title,
            input,
        } = claim;
        let mut projection = crate::lifecycle::TurnProjection::new(
            self,
            Arc::clone(reservation.conversation()),
            request_id,
            &thread_id,
            title,
            emit,
        );
        let run_result = reservation.run(&input, &mut projection);
        if let Some(poisoned) = projection.take_poisoned() {
            return Err(poisoned);
        }
        match run_result {
            Ok(_) => Ok(()),
            Err(singularity_runtime::ConversationError::Settings(message)) => {
                Err(AppServerError::InvalidParams(message))
            }
            Err(singularity_runtime::ConversationError::Turn(error)) => {
                crate::lifecycle::classify_run_result(Err(error))
            }
            Err(singularity_runtime::ConversationError::TurnAlreadyActive) => {
                // 预订后不可能再发生；防御性保留。
                Err(AppServerError::Workspace(
                    "another turn is already running for this session".to_string(),
                ))
            }
        }
    }

    pub(crate) fn provider_status(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        json_response(
            message.required_id(),
            provider_configuration(self.turn_runner.provider_snapshot()),
        )
    }

    pub(crate) fn server_shutdown(
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

    pub(crate) fn turn_interrupt(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        self.control_handle().turn_interrupt(message)
    }

    pub(crate) fn turn_steer(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        self.control_handle().turn_steer(message)
    }

    pub(crate) fn turn_follow_up(
        &mut self,
        message: JsonRpcMessage,
    ) -> AppServerResult<Vec<Value>> {
        self.control_handle().turn_follow_up(message)
    }
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

/// 组合 `provider/model[#reasoning]` selector（展示用；持久化校验由协调器完成）。
fn compose_model_selector(provider: &str, model: &str, reasoning: Option<&str>) -> String {
    let mut selector = format!("{provider}/{model}");
    if let Some(reasoning) = reasoning {
        selector.push('#');
        selector.push_str(reasoning);
    }
    selector
}
