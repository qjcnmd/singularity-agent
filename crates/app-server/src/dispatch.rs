//! JSON-RPC 注册表校验与请求分发处理器。

use singularity_agent::session::{SessionError, SessionManager, SessionRepository};
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

pub(super) fn input_items_to_text(
    items: &[singularity_protocol::InputItem],
) -> AppServerResult<String> {
    let text = items
        .iter()
        .map(|item| match item {
            singularity_protocol::InputItem::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return Err(AppServerError::InvalidParams(
            "persisted turn input is empty".to_string(),
        ));
    }
    Ok(text)
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
        self.handle_with_output(message)
    }

    /// 处理请求并返回生成的协议消息；传输 writer 负责全局输出顺序，dispatch 不假设 turn 执行顺序。
    pub fn handle_with_output(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
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

        // 初始化门禁先于参数校验：未初始化前，任何请求方法的响应都是
        // Not initialized，参数解析在各自 handler 内唯一进行。
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

        // 通知直达处理器：不合成 id，无响应路径；Request-kind 方法的
        // notification 已在上面静默忽略，此处只可能是 Notification-kind 方法。
        let result = match method {
            Method::Initialize => self.initialize(message),
            Method::Initialized => {
                self.initialized_acknowledged = true;
                Ok(Vec::new())
            }
            Method::ThreadList => self.thread_list(message),
            Method::ThreadStart => self.thread_start(message),
            Method::ThreadRead => self.thread_read(message),
            Method::SessionDelete => self.session_delete(message),
            Method::ThreadSettings => self.thread_settings(message),
            Method::TurnStart => Err(AppServerError::InvalidParams(
                "turn/start must be claimed via the turn lane".to_string(),
            )),
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
        let threads = singularity_runtime::store::list_threads(&self.sessions_dir)
            .map_err(AppServerError::Store)?
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
        let record = match singularity_runtime::store::read_thread_summary(
            &self.sessions_dir,
            &params.thread_id,
        ) {
            Ok(record) => record,
            Err(singularity_runtime::store::ResumeError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(singularity_runtime::store::ResumeError::Store(error)) => {
                return Err(AppServerError::Store(error));
            }
        };
        let patch = singularity_runtime::SettingsPatch {
            provider: params.provider,
            model: params.model,
            reasoning: match params.reasoning {
                singularity_protocol::ReasoningPatch::Keep => {
                    singularity_runtime::ReasoningPatch::Keep
                }
                singularity_protocol::ReasoningPatch::Set(value) => {
                    singularity_runtime::ReasoningPatch::Set(value)
                }
                singularity_protocol::ReasoningPatch::Clear => {
                    singularity_runtime::ReasoningPatch::Clear
                }
            },
        };
        // queue_settings 在提交点完成校验、组合与持久化，返回合并后的完整
        // selector。客户端只做投影，不反推 provider/model/reasoning。
        let conversation = self.conversation_for(&record.thread_id)?;
        let result = match conversation.queue_settings(patch) {
            Ok(result) => result,
            Err(error) => {
                let message = match error {
                    singularity_runtime::ConversationError::Configuration(message)
                    | singularity_runtime::ConversationError::State(message) => {
                        format!("invalid model selector: {message}")
                    }
                    other => other.to_string(),
                };
                return Err(AppServerError::InvalidParams(message));
            }
        };
        let parts = singularity_model::split_model_selector(&result.selector);
        json_response(
            message.required_id(),
            ThreadSettingsResult {
                thread_id: record.thread_id,
                provider: parts.provider.map(str::to_string),
                model: parts.model.map(str::to_string),
                reasoning: parts.effort.map(str::to_string),
                updated: result.timing
                    != singularity_runtime::SettingsApplyTiming::NothingToApply,
                queued: result.timing
                    == singularity_runtime::SettingsApplyTiming::QueuedForNextTurn,
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
            Err(error) => return Err(AppServerError::Store(error)),
        };
        let protocol_thread = Thread {
            thread_id: thread.thread_id,
            model,
            cwd: Some(cwd),
            last_turn_status: None,
        };
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

    pub(super) fn thread_read(&mut self, message: JsonRpcMessage) -> AppServerResult<Vec<Value>> {
        let params: ThreadReadParams = parse_params(&message)?;
        if !(1..=200).contains(&params.limit) {
            return invalid_params_response(message.required_id());
        }
        let record = match singularity_runtime::store::read_thread_summary(
            &self.sessions_dir,
            &params.session_id,
        ) {
            Ok(record) => record,
            Err(singularity_runtime::store::ResumeError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(singularity_runtime::store::ResumeError::Store(error)) => {
                return Err(AppServerError::Store(error));
            }
        };
        let repository = SessionRepository::new(self.sessions_dir.clone());
        let read = repository
            .read(&record.thread_id)
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
            ThreadReadResult {
                session_id: record.thread_id,
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
        let record = match singularity_runtime::store::read_thread_summary(
            &self.sessions_dir,
            &params.session_id,
        ) {
            Ok(record) => record,
            Err(singularity_runtime::store::ResumeError::NotFound(_)) => {
                return not_found_response(message.required_id(), THREAD_NOT_FOUND);
            }
            Err(singularity_runtime::store::ResumeError::Store(error)) => {
                return Err(AppServerError::Store(error));
            }
        };
        // 会话仍有存活 turn 时拒绝删除：worker 可能正持句柄 append，删除会让
        // 后续写入落入 unlinked inode。
        if self.thread_has_live_turn(&record.thread_id) {
            return invalid_state_response(message.required_id(), SESSION_DELETE_TURN_ACTIVE);
        }
        // 打开并校验 rollout header 后再进入删除；不能先永久删 JSONL。
        let rollout_path =
            singularity_runtime::store::thread_session_path(&self.sessions_dir, &record.thread_id);
        let session = match SessionManager::open_existing(&rollout_path) {
            Ok(session) => session,
            Err(SessionError::WriterConflict { .. }) => {
                return invalid_state_response(message.required_id(), SESSION_DELETE_WRITER_ACTIVE);
            }
            Err(error) => return Err(AppServerError::Session(error)),
        };
        if session.session_id() != record.thread_id {
            return Err(AppServerError::Store(format!(
                "rollout header id {} does not match requested session id {}",
                session.session_id(),
                record.thread_id
            )));
        }
        // 持锁完成 unlink：写者锁在会话删除后随实例释放，跨进程写者不会在
        // 删除窗口内开始 append。
        crate::delete::delete_session(&rollout_path)?;
        drop(session);
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

/// 已预订的 turn/start：把输入与预订一起交给执行线程。
pub struct TurnStartClaim {
    pub reservation: singularity_runtime::TurnReservation,
    pub request_id: JsonRpcId,
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
        if !singularity_runtime::thread_session_path(&self.sessions_dir, &params.thread_id)
            .is_file()
        {
            return Ok(TurnClaim::Responded(
                not_found_response(message.required_id(), THREAD_NOT_FOUND)?.remove(0),
            ));
        }
        let conversation = self.conversation_for(&params.thread_id)?;
        let input_text = match input_items_to_text(&params.input) {
            Ok(text) => text,
            Err(_) => {
                return Ok(TurnClaim::Responded(
                    invalid_params_response(message.required_id())?.remove(0),
                ));
            }
        };
        match conversation.reserve_start() {
            Ok(reservation) => Ok(TurnClaim::Accepted(TurnStartClaim {
                reservation,
                request_id: message.required_id(),
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
            Err(singularity_runtime::ConversationError::Configuration(message))
            | Err(singularity_runtime::ConversationError::State(message)) => Err(
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

    /// 以已预订的协调器执行整条 turn 链条（worker 线程入口）。
    #[doc(hidden)]
    pub fn run_turn_started(
        &mut self,
        claim: TurnStartClaim,
        emit: &mut impl FnMut(AppServerOutput),
    ) -> AppServerResult<()> {
        let TurnStartClaim {
            reservation,
            request_id,
            input,
        } = claim;
        let mut projection = crate::lifecycle::TurnProjection::new(
            self,
            Arc::clone(reservation.conversation()),
            request_id,
            emit,
        );
        let run_result = reservation.run(&input, &mut projection);
        if let Some(poisoned) = projection.take_poisoned() {
            return Err(poisoned);
        }
        match run_result {
            Ok(_) => Ok(()),
            Err(singularity_runtime::ConversationError::Configuration(message))
            | Err(singularity_runtime::ConversationError::State(message)) => {
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
