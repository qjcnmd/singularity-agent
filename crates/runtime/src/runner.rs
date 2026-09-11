//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影与终态落盘。
//!
//! 执行不变量：
//! - 准备失败与 operation_started 成功后的提交失败分开归类；
//! - 设置记录与本 turn 的 operation_started 先于一切事件落盘；终态记录
//!   （operation_finished，status/usage/truncated 单条）先于终态事件；
//! - 一个 turn 只打开一次会话文件，同一 SessionManager 贯穿全程；
//! - 投影是尽力而为的观察侧信道，投影失败只丢弃投影，不影响执行事实。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use singularity_agent::agent::TurnInbox;
use singularity_agent::agent::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentEvents, AgentTerminalReason,
};
use singularity_agent::compaction::CompactionConfig;
use singularity_agent::prompts::PromptAssembly;
use singularity_agent::session::{
    ControlDisposition, ControlRequest, LedgerRecord, OperationKind, SessionAccess, SessionData,
    SessionError, SessionManager, SessionMetadata, SessionWriter, WriterLockCoordinator,
    lock_writer,
};
use singularity_agent::tools::ToolRegistrySnapshot;
use singularity_core::{CancellationToken, load_agent_instructions};
use singularity_model::{
    DEFAULT_PROVIDER_NAME, ModelConfigurationSnapshot, Provider, ProviderConfigSnapshot,
    split_model_selector,
};
use uuid::Uuid;

use crate::assistant_items::AssistantItemEvents;
use crate::error::{TurnFailureCause, TurnFailureStage, TurnRunError, provider_turn_cause};
use crate::events::{TurnErrorDetail, TurnEvent};
use crate::objects::{Thread, Turn, TurnModelUsage, TurnStatus};
use crate::terminal::{TerminalCommit, fail_stop_terminalization};

#[derive(Debug, thiserror::Error)]
pub enum CompactionRunError {
    #[error(transparent)]
    Preparation(#[from] TurnRunError),
    #[error("compaction agent preparation failed: {0}")]
    AgentPreparation(#[source] AgentError),
    #[error("compaction start could not be persisted: {0}")]
    Start(#[source] SessionError),
    #[error("{0}")]
    Execution(#[source] AgentError),
    #[error("{0}")]
    Interrupted(#[source] AgentError),
    #[error("compaction terminalization failed: {0}")]
    Terminalization(#[source] SessionError),
}

/// 一次 turn 执行的输入。
pub(crate) struct TurnParams {
    pub thread: Thread,
    pub input: String,
    /// 本回合由协调器接受的 followUp/requeued steer 控制的 durable 请求
    /// （携带控制 identity、payload 与 FIFO 接受序号）；普通显式输入为
    /// None。有值时 runner 在本 turn 的 operation_started 之后、任何
    /// 实时事件之前落 control_accepted 终态 disposition
    /// （started_as_new_turn）。
    pub control: Option<ControlRequest>,
}

/// 一次收敛到可信终态的 turn 结果（completed/failed/interrupted 都是可信
/// 终态；不存在可信终态的情形由 TurnRunError 表达）。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn_id: String,
    pub turn_status: TurnStatus,
    pub truncated: bool,
    pub usage: TurnModelUsage,
    /// 失败终态的协议错误细节（stage/cause/message 与已发布的 turn/error
    /// 事件同源）；非失败终态为 None。客户端据此报告进程结果，
    /// 不再从事件流重建终态事实。
    pub error: Option<TurnErrorDetail>,
    /// 终态后仍留在注入箱、未在本次 turn 交付的转向输入（中断时退还调用方）。
    pub undelivered_inputs: Vec<String>,
}

/// Internal handoff preserves control identity on both success and failure.
pub(crate) struct TurnRunResult {
    pub result: Result<TurnOutcome, TurnRunError>,
    pub undelivered: Vec<ControlRequest>,
}

struct StartedTurn {
    agent: Agent,
    operation_id: String,
}

/// 进程内 turn 执行器：无状态、可共享，按需构造。
pub struct TurnRunner {
    sessions_dir: PathBuf,
    provider_snapshot: RwLock<ProviderConfigSnapshot>,
    /// 进程级写者锁协调器：所有会话打开路径共用稳定锁文件。
    coordinator: Arc<WriterLockCoordinator>,
    #[cfg(any(test, feature = "test-support"))]
    provider_override: Option<Arc<dyn Provider + Send + Sync>>,
}

impl TurnRunner {
    /// Discover skills using the same application home as Agent execution.
    pub fn skills(&self, cwd: &std::path::Path) -> singularity_core::skills::SkillCatalog {
        singularity_core::skills::SkillCatalog::discover(
            cwd,
            self.sessions_dir.parent().unwrap_or(&self.sessions_dir),
        )
    }
    pub fn new(sessions_dir: PathBuf, provider_snapshot: ProviderConfigSnapshot) -> Self {
        let coordinator = Arc::new(WriterLockCoordinator::new(&sessions_dir));
        Self {
            sessions_dir,
            provider_snapshot: RwLock::new(provider_snapshot),
            coordinator,
            #[cfg(any(test, feature = "test-support"))]
            provider_override: None,
        }
    }

    /// 测试注入：以固定 provider 取代快照解析结果。
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_provider_override(mut self, provider: Arc<dyn Provider + Send + Sync>) -> Self {
        self.provider_override = Some(provider);
        self
    }

    /// 会话目录与进程内写者锁协调器：目录操作只经 crate::ThreadCatalog
    /// 暴露给客户端，此处仅供 runtime 内部（目录接缝与会话打开路径）使用。
    pub(crate) fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    pub(crate) fn coordinator(&self) -> &Arc<WriterLockCoordinator> {
        &self.coordinator
    }

    pub(crate) fn load_control_state(
        &self,
        thread: &Thread,
    ) -> Result<
        (
            Vec<singularity_protocol::ControlSnapshot>,
            Vec<ControlRequest>,
            u64,
        ),
        singularity_agent::session::SessionError,
    > {
        let path = crate::store::thread_session_path(&self.sessions_dir, &thread.thread_id);
        let session = SessionData::open(&path)?;
        session.verify_session_id(&thread.thread_id)?;
        let reduced = singularity_agent::session::reduce_controls(session.entries());
        let next_sequence = reduced
            .iter()
            .map(|control| control.sequence)
            .max()
            .map_or(0, |sequence| sequence.saturating_add(1));
        let pending = reduced
            .iter()
            .filter(|control| control.is_pending_input())
            .map(|control| ControlRequest {
                control_id: control.control_id.clone(),
                turn_id: control.turn_id.clone(),
                channel: control.channel,
                sequence: control.sequence,
                text: control.text.clone(),
            })
            .collect();
        Ok((reduced, pending, next_sequence))
    }

    /// 打开本轮唯一会话写者（含崩溃修复并返回 SessionWriter）。
    /// workspace 检查先行：任何失败都不打开会话、不留 operation 痕迹。
    /// 调用方（协调器）在 turn 开始前持有写者，使控制接受可经同一写者
    /// durable 落盘。
    pub(crate) fn open_turn_writer(&self, thread: &Thread) -> Result<SessionWriter, TurnRunError> {
        workspace_path(thread).map_err(|message| TurnRunError::Preparation {
            cause: TurnFailureCause::Workspace,
            message,
        })?;
        let session =
            self.open_and_repair_session(thread)
                .map_err(|error| TurnRunError::Preparation {
                    cause: TurnFailureCause::Store,
                    message: error.to_string(),
                })?;
        Ok(Arc::new(std::sync::Mutex::new(session)))
    }

    /// 空闲时短开会话写者追加控制事实；活动 turn 使用 TurnControls 的共享写者。
    pub(crate) fn append_control_record(
        &self,
        thread: &Thread,
        record: LedgerRecord,
    ) -> Result<(), String> {
        let mut session = self
            .open_and_repair_session(thread)
            .map_err(|error| error.to_string())?;
        session
            .append_record(record)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// 快照的默认模型 selector（未配置时为 None）。
    pub fn default_model_selector(&self) -> Option<String> {
        self.lock_provider_snapshot().resolved_default_selector()
    }

    /// 原子替换未来回合使用的 provider 配置；活动回合已持有其不可变实例。
    pub fn refresh_provider_snapshot(&self, snapshot: ProviderConfigSnapshot) {
        *self.lock_provider_snapshot_mut() = snapshot;
    }

    /// 校验模型 selector 能被快照解析为具体 provider 配置。
    pub fn validate_model_selector(&self, selector: Option<&str>) -> Result<(), String> {
        self.lock_provider_snapshot()
            .validate_selector(selector)
            .map_err(|error| format!("invalid model selector: {error}"))
    }

    /// 在 turn 之外压缩既有 Thread：以独立 compaction operation 落盘
    /// （operation_started/operation_finished，无 turn 绑定）。
    /// cancellation 由调用方持有，可随时中止压缩。
    pub(crate) fn compact_thread(
        &self,
        thread: &Thread,
        cancellation: &CancellationToken,
        writer: SessionWriter,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, CompactionRunError> {
        workspace_path(thread).map_err(|message| {
            CompactionRunError::Preparation(TurnRunError::Preparation {
                cause: TurnFailureCause::Workspace,
                message,
            })
        })?;
        let registry = ToolRegistrySnapshot::new();
        let (provider, config, model) = self
            .resolve_agent_runtime(thread, &registry)
            .map_err(CompactionRunError::Preparation)?;
        let operation_id = Uuid::now_v7().to_string();
        let mut agent = Agent::new(
            TurnInbox::default_handle(),
            provider,
            model,
            registry,
            config,
            Arc::clone(&writer),
        )
        .map_err(CompactionRunError::AgentPreparation)?;
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationStarted {
                operation_id: operation_id.clone(),
                kind: OperationKind::Compaction,
                turn_id: None,
            })
            .map_err(CompactionRunError::Start)?;
        let outcome = agent.compact_now(&mut AgentEvents::default(), cancellation);
        let terminal_status = match &outcome {
            Ok(_) => TurnStatus::Completed,
            Err(AgentError::Compaction(
                singularity_agent::compaction::CompactionError::Aborted,
            )) => TurnStatus::Interrupted,
            Err(AgentError::Compaction(
                singularity_agent::compaction::CompactionError::Provider(error),
            )) if error.kind == singularity_model::ModelErrorKind::Cancelled => {
                TurnStatus::Interrupted
            }
            Err(_) => TurnStatus::Failed,
        };
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationFinished {
                operation_id,
                turn_id: None,
                outcome: terminal_status,
                usage: Some(singularity_agent::session::turn_usage_from_model_usage(
                    agent.request_usage().0,
                    agent.request_usage().1,
                )),
                truncated: false,
            })
            .map_err(CompactionRunError::Terminalization)?;
        outcome.map_err(|error| {
            if terminal_status == TurnStatus::Interrupted {
                CompactionRunError::Interrupted(error)
            } else {
                CompactionRunError::Execution(error)
            }
        })
    }

    /// 执行一个 turn 直到终态收敛。
    ///
    /// 调用方持有 crate::TurnControls 以便在执行期间注入输入或取消；
    /// 返回 Ok 时终态（completed/failed/interrupted）已持久化且终态事件
    /// 已发出——失败终态的 TurnOutcome::error 携带与 turn/error 事件
    /// 同源的协议错误细节；返回 TurnRunError::Terminalization 时终态
    /// 记录无法落盘，不存在任何虚假终态事件。
    pub(crate) fn run(
        &self,
        params: TurnParams,
        controls: &crate::conversation::TurnControls,
        sink: &mut dyn FnMut(TurnEvent),
        control_sink: &mut dyn FnMut(),
    ) -> TurnRunResult {
        let started = match self.start_turn(&params, controls) {
            Ok(prepared) => prepared,
            Err(error) => {
                let undelivered = controls.finish_inbox();
                let cancel_acceptances = controls.close_cancel_acceptances();
                let writer = controls.writer();
                let result = match flush_cancel_acceptances(
                    &writer,
                    controls,
                    cancel_acceptances,
                    control_sink,
                ) {
                    Ok(()) => Err(error),
                    Err(storage_error) => Err(fail_stop_terminalization(
                        &params.thread.thread_id,
                        &controls.turn_id,
                        storage_error,
                        sink,
                    )),
                };
                return TurnRunResult {
                    result,
                    undelivered,
                };
            }
        };
        Self::run_started_turn(started, params, controls, sink, control_sink)
    }

    fn run_started_turn(
        started: StartedTurn,
        params: TurnParams,
        controls: &crate::conversation::TurnControls,
        sink: &mut dyn FnMut(TurnEvent),
        control_sink: &mut dyn FnMut(),
    ) -> TurnRunResult {
        let StartedTurn {
            mut agent,
            operation_id,
        } = started;
        let turn_id = controls.turn_id.clone();
        let thread = params.thread;
        let writer = controls.writer();
        // The operation is already durable. Failure to claim a queued input
        // leaves no trusted terminal, and the unclaimed control must be returned.
        if let Some(request) = params.control {
            let claim = controls.append_control(&request, ControlDisposition::StartedAsNewTurn);
            if let Err(error) = claim {
                let mut undelivered = controls.finish_inbox();
                undelivered.insert(0, request);
                let cancel_acceptances = controls.close_cancel_acceptances();
                let storage_error =
                    flush_cancel_acceptances(&writer, controls, cancel_acceptances, control_sink)
                        .err()
                        .unwrap_or_else(|| error.to_string());
                return TurnRunResult {
                    result: Err(fail_stop_terminalization(
                        &thread.thread_id,
                        &turn_id,
                        storage_error,
                        sink,
                    )),
                    undelivered,
                };
            }
            control_sink();
        }
        let turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: thread.thread_id.clone(),
            status: TurnStatus::Running,
            usage: None,
        };
        sink(TurnEvent::TurnStarted {
            turn,
            input: params.input.clone(),
        });

        let mut item_events = AssistantItemEvents::new(thread.thread_id.clone(), turn_id.clone());
        let run_result = {
            let mut events = AgentEvents::default();
            let mut on_event = |event: AgentEvent| match event {
                AgentEvent::ControlChanged(control) => {
                    controls.record_control(control);
                    control_sink();
                }
                event => item_events.project(sink, event),
            };
            events.on_event = Some(&mut on_event);
            agent.run(&params.input, &mut events, &controls.cancellation)
        };
        // Close and drain once; every exit below returns these exact controls.
        let undelivered = controls.finish_inbox();
        // 这是完成与取消竞争的唯一截止点：此前完整接受的 cancel 由本轮
        // 收敛，此后 abort 被拒绝且不会写入新的 pending 事实。
        let cancel_acceptances = controls.close_cancel_acceptances();
        let cancel_accepted = !cancel_acceptances.is_empty();
        let run_result = run_result.and_then(|outcome| {
            if !cancel_accepted
                && outcome.terminal_reason == AgentTerminalReason::Completed
                && outcome.final_text.trim().is_empty()
            {
                Err(AgentError::Loop(
                    "agent loop stopped without a final assistant message".to_string(),
                ))
            } else {
                Ok(outcome)
            }
        });
        let (turn_status, truncated, error) = match run_result {
            Ok(outcome) => (
                match outcome.terminal_reason {
                    AgentTerminalReason::Completed if cancel_accepted => TurnStatus::Interrupted,
                    AgentTerminalReason::Completed => TurnStatus::Completed,
                    AgentTerminalReason::Aborted => TurnStatus::Interrupted,
                },
                outcome.truncated,
                None,
            ),
            Err(error) => (
                TurnStatus::Failed,
                false,
                Some(TurnErrorDetail {
                    stage: TurnFailureStage::AgentLoop,
                    cause: turn_failure_cause(&error),
                    message: error.to_string(),
                }),
            ),
        };
        let (usage, usage_complete) = agent.request_usage();
        // 所有执行结果共用取消控制、终态落盘和 item 闭合顺序；
        // 任一存储失败都 fail-stop，不发布虚假终态。
        #[allow(clippy::expect_used)]
        let terminal = TerminalCommit::new(
            &operation_id,
            &turn_id,
            turn_status,
            usage,
            usage_complete,
            truncated,
        )
        .expect("Agent execution always resolves to a terminal status");
        let result = (|| {
            if let Some(storage_error) = controls.take_storage_failure() {
                return Err(fail_stop_terminalization(
                    &thread.thread_id,
                    &turn_id,
                    storage_error,
                    sink,
                ));
            }
            if turn_status == TurnStatus::Interrupted {
                for request in &undelivered {
                    if let Err(storage_error) =
                        controls.append_control(request, ControlDisposition::Cancelled)
                    {
                        return Err(fail_stop_terminalization(
                            &thread.thread_id,
                            &turn_id,
                            storage_error,
                            sink,
                        ));
                    }
                    control_sink();
                }
            }
            let flush_result =
                flush_cancel_acceptances(&writer, controls, cancel_acceptances, control_sink);
            if let Err(storage_error) =
                flush_result.and_then(|()| terminal.persist(&mut lock_writer(&writer)))
            {
                return Err(fail_stop_terminalization(
                    &thread.thread_id,
                    &turn_id,
                    storage_error,
                    sink,
                ));
            }
            let final_turn = terminal.turn(&thread.thread_id);
            item_events.finish_open_items(sink, error.is_some());
            if let Some(error) = &error {
                sink(TurnEvent::TurnFailed {
                    thread_id: thread.thread_id.clone(),
                    turn_id: turn_id.clone(),
                    error: error.clone(),
                });
            } else {
                sink(TurnEvent::TurnCompleted {
                    turn: final_turn.clone(),
                });
            }
            Ok(TurnOutcome {
                turn_id,
                turn_status: final_turn.status,
                truncated,
                usage: terminal.usage().clone(),
                error,
                undelivered_inputs: Vec::new(),
            })
        })();
        TurnRunResult {
            result,
            undelivered,
        }
    }

    fn start_turn(
        &self,
        params: &TurnParams,
        controls: &crate::conversation::TurnControls,
    ) -> Result<StartedTurn, TurnRunError> {
        // 会话写者由协调器在 turn 开始前打开（含 workspace 检查与崩溃修复）；
        // 这里只做剩余 fail-fast 准备（provider/config/项目指令），全部就绪
        // 后才写任何 operation 状态。
        let writer = controls.writer();
        let registry = ToolRegistrySnapshot::new();
        let (provider, config, model) = self.resolve_agent_runtime(&params.thread, &registry)?;
        // OperationStarted records operation/turn identity. Agent persists the
        // input message separately; these appends are not an atomic transaction.
        let operation_id = Uuid::now_v7().to_string();
        let agent = Agent::new(
            controls.inbox_handle(),
            provider,
            model,
            registry,
            config,
            writer.clone(),
        )
        .map_err(|error| TurnRunError::Preparation {
            cause: TurnFailureCause::Store,
            message: error.to_string(),
        })?;
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationStarted {
                operation_id: operation_id.clone(),
                kind: OperationKind::Run,
                turn_id: Some(controls.turn_id.clone()),
            })
            .map_err(|error| TurnRunError::Preparation {
                cause: TurnFailureCause::Store,
                message: error.to_string(),
            })?;
        Ok(StartedTurn {
            agent,
            operation_id,
        })
    }

    /// 解析 Provider、AgentConfig 与本 turn 冻结的模型配置快照并预校验
    /// compaction；任一失败直接失败，不留 operation 痕迹。
    fn resolve_agent_runtime(
        &self,
        thread: &Thread,
        registry: &ToolRegistrySnapshot,
    ) -> Result<
        (
            Arc<dyn Provider + Send + Sync>,
            AgentConfig,
            ModelConfigurationSnapshot,
        ),
        TurnRunError,
    > {
        let provider: Arc<dyn Provider + Send + Sync> = {
            #[cfg(any(test, feature = "test-support"))]
            let overridden = self.provider_override.clone();
            #[cfg(not(any(test, feature = "test-support")))]
            let overridden: Option<Arc<dyn Provider + Send + Sync>> = None;
            match overridden {
                Some(provider) => provider,
                None => Arc::new(
                    self.lock_provider_snapshot()
                        .provider_for_selector(thread.model.as_deref())
                        .map_err(|error| TurnRunError::Preparation {
                            cause: TurnFailureCause::Internal,
                            message: error.to_string(),
                        })?,
                ),
            }
        };
        let model = provider.model_configuration();
        let config = agent_config_for_thread(
            thread,
            registry,
            self.sessions_dir.parent().unwrap_or(&self.sessions_dir),
        )?;
        Ok((provider, config, model))
    }

    fn lock_provider_snapshot(&self) -> std::sync::RwLockReadGuard<'_, ProviderConfigSnapshot> {
        match self.provider_snapshot.read() {
            Ok(snapshot) => snapshot,
            Err(_) => panic!("provider snapshot lock poisoned (fail-stop)"),
        }
    }

    fn lock_provider_snapshot_mut(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, ProviderConfigSnapshot> {
        match self.provider_snapshot.write() {
            Ok(snapshot) => snapshot,
            Err(_) => panic!("provider snapshot lock poisoned (fail-stop)"),
        }
    }

    fn open_and_repair_session(&self, thread: &Thread) -> Result<SessionManager, SessionError> {
        let path = crate::store::thread_session_path(&self.sessions_dir, &thread.thread_id);
        SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            &thread.thread_id,
            SessionAccess::RepairWrite,
        )
    }
}

/// 把本 turn 已接受的取消控制落盘（durable-before-publish：先于终态记录）。
/// 存储失败以 Err 上抛，调用方与终态写入共用同一 fail-stop 出口。
fn flush_cancel_acceptances(
    writer: &SessionWriter,
    controls: &crate::conversation::TurnControls,
    acceptances: Vec<ControlRequest>,
    control_sink: &mut dyn FnMut(),
) -> Result<(), String> {
    if let Some(failure) = controls.take_storage_failure() {
        return Err(failure);
    }
    for request in acceptances {
        lock_writer(writer)
            .append_record(request.record(ControlDisposition::Cancelled))
            .map_err(|error| error.to_string())?;
        controls.record_control(request.snapshot(ControlDisposition::Cancelled));
        // The writer guard above must be released before entering the Web projection sink:
        // control RPCs acquire the slot/state locks before this same writer.
        control_sink();
    }
    if let Some(failure) = controls.take_storage_failure() {
        return Err(failure);
    }
    Ok(())
}

fn turn_failure_cause(error: &AgentError) -> TurnFailureCause {
    match error {
        AgentError::Provider(provider_error)
        | AgentError::Compaction(singularity_agent::compaction::CompactionError::Provider(
            provider_error,
        )) => provider_turn_cause(provider_error.kind),
        AgentError::Session(_) => TurnFailureCause::Store,
        AgentError::Compaction(singularity_agent::compaction::CompactionError::Session(_)) => {
            TurnFailureCause::Store
        }
        AgentError::Compaction(_) | AgentError::Loop(_) => TurnFailureCause::Internal,
    }
}

/// 在已打开的唯一会话写者上保存 selector，设置提交和 turn 初始化共用此入口。
/// 与最后一次持久选择相同时跳过；Thread 无模型覆盖时不记录。
pub(crate) fn record_thread_settings_metadata(
    session: &mut SessionManager,
    thread: &Thread,
) -> Result<(), singularity_agent::session::SessionError> {
    let Some(selector) = thread.model.as_deref() else {
        return Ok(());
    };
    let parts = split_model_selector(selector);
    let already_recorded = session
        .metadata_entries()
        .iter()
        .rev()
        .find_map(|entry| match entry {
            SessionMetadata::ThreadSettings {
                provider,
                model,
                reasoning,
            } => Some((provider.as_str(), model.as_str(), reasoning.as_deref())),
            _ => None,
        })
        .is_some_and(|(provider, model, reasoning)| {
            provider == parts.provider.unwrap_or(DEFAULT_PROVIDER_NAME)
                && Some(model) == parts.model
                && reasoning.filter(|value| !value.is_empty()) == parts.effort
        });
    if already_recorded {
        return Ok(());
    }
    session
        .append_metadata(SessionMetadata::thread_settings(
            parts.provider.unwrap_or(DEFAULT_PROVIDER_NAME),
            parts.model.unwrap_or_default(),
            parts.effort.map(str::to_string),
        ))
        .map(|_| ())
}

fn workspace_path(thread: &Thread) -> Result<&str, String> {
    singularity_core::canonicalize_workspace(&thread.cwd)?;
    Ok(&thread.cwd)
}

/// 装配固定系统提示词和文件指令来源。准备阶段预读指令以提前报告 I/O
/// 失败；每个模型步的实际注入和截断反馈由 Agent 的请求准备过程负责。
fn agent_config_for_thread(
    thread: &Thread,
    registry: &ToolRegistrySnapshot,
    instruction_home: &std::path::Path,
) -> Result<AgentConfig, TurnRunError> {
    let cwd = &thread.cwd;
    load_agent_instructions(std::path::Path::new(cwd), instruction_home).map_err(|message| {
        TurnRunError::Preparation {
            cause: TurnFailureCause::ProjectInstructions,
            message,
        }
    })?;
    let assembled = PromptAssembly::assemble(cwd, registry);
    Ok(AgentConfig {
        system_prompt: assembled,
        instruction_home: Some(instruction_home.to_path_buf()),
        compaction: CompactionConfig::default(),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    #[allow(clippy::unwrap_used)]
    fn failures_around_start_and_terminal_return_unconsumed_control_identity() {
        use super::*;
        use crate::conversation::TurnControls;
        use singularity_agent::session::{
            ControlChannel, open_operations, reduce_controls, reduce_operations,
        };
        use singularity_model::test_support::ScriptedProvider;

        for boundary in ["before_start", "after_start", "before_terminal"] {
            let home = crate::test_support::temp_sessions();
            let sessions = home.path().join("sessions");
            let provider = Arc::new(ScriptedProvider::ok("done"));
            let runner =
                TurnRunner::new(sessions.clone(), crate::test_support::provider_snapshot())
                    .with_provider_override(provider.clone());
            let thread = crate::ThreadCatalog::new(&runner)
                .create_thread(home.path().to_str().unwrap(), None)
                .unwrap();
            let writer = runner.open_turn_writer(&thread).unwrap();
            let path = lock_writer(&writer).path().to_path_buf();
            let request = ControlRequest {
                control_id: "queued-control".into(),
                turn_id: "previous-turn".into(),
                channel: ControlChannel::FollowUp,
                sequence: 0,
                text: Some("queued input".into()),
            };
            lock_writer(&writer)
                .append_record(request.record(ControlDisposition::Pending))
                .unwrap();
            let controls = TurnControls::new(
                "active-turn",
                TurnInbox::default_handle(),
                Arc::new(std::sync::atomic::AtomicU64::new(1)),
                writer.clone(),
                Arc::new(crate::conversation::ControlProjection::new(Vec::new())),
            );
            let steer = controls.steer("unconsumed steer").unwrap();
            let params = TurnParams {
                thread,
                input: "queued input".into(),
                control: Some(request.clone()),
            };
            let mut events = Vec::new();
            let mut saved = Vec::new();
            let run = if boundary == "before_start" {
                saved = std::fs::read(&path).unwrap();
                std::fs::remove_file(&path).unwrap();
                runner.run(
                    params,
                    &controls,
                    &mut |event| events.push(event),
                    &mut || {},
                )
            } else {
                let started = runner.start_turn(&params, &controls).unwrap();
                if boundary == "after_start" {
                    saved = std::fs::read(&path).unwrap();
                    std::fs::remove_file(&path).unwrap();
                }
                TurnRunner::run_started_turn(
                    started,
                    params,
                    &controls,
                    &mut |event| {
                        if boundary == "before_terminal"
                            && matches!(event, TurnEvent::TurnStarted { .. })
                        {
                            saved = std::fs::read(&path).unwrap();
                            std::fs::remove_file(&path).unwrap();
                        }
                        events.push(event);
                    },
                    &mut || {},
                )
            };
            if boundary == "before_start" {
                assert!(matches!(
                    run.result,
                    Err(TurnRunError::Preparation {
                        cause: TurnFailureCause::Store,
                        ..
                    })
                ));
            } else {
                assert!(
                    matches!(run.result, Err(TurnRunError::Terminalization(error)) if error.cause == TurnFailureCause::Store)
                );
            }
            assert!(!events.iter().any(|event| matches!(
                event,
                TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
            )));
            assert!(provider.requests().is_empty());
            let returned = run
                .undelivered
                .iter()
                .find(|control| control.control_id == steer.control_id)
                .unwrap();
            assert_eq!(returned.sequence, steer.sequence);
            assert_eq!(returned.channel, steer.channel);
            assert_eq!(returned.text, steer.text);
            assert_eq!(returned.turn_id, steer.turn_id);
            assert_eq!(
                run.undelivered.len(),
                if boundary == "after_start" { 2 } else { 1 }
            );
            if boundary == "after_start" {
                assert_eq!(run.undelivered[0], request);
            }
            assert!(controls.steer("late input").is_err());
            drop(controls);
            drop(writer);
            std::fs::write(&path, saved).unwrap();
            let reopened = SessionManager::open_existing(&path).unwrap();
            let operations = reduce_operations(reopened.entries());
            assert_eq!(operations.len(), usize::from(boundary != "before_start"));
            // Normal repair closes the interrupted operation without executing inputs/tools.
            drop(reopened);
            let repaired = SessionManager::open_existing_with_access(
                &path,
                runner.coordinator(),
                path.file_stem().unwrap().to_str().unwrap(),
                SessionAccess::RepairWrite,
            )
            .unwrap();
            assert!(open_operations(&reduce_operations(repaired.entries())).is_empty());
            let controls = reduce_controls(repaired.entries());
            assert_eq!(
                controls
                    .iter()
                    .find(|control| control.control_id == request.control_id)
                    .unwrap()
                    .disposition,
                if boundary == "before_terminal" {
                    ControlDisposition::StartedAsNewTurn
                } else {
                    ControlDisposition::Pending
                }
            );
        }
    }

    #[test]
    fn summary_authentication_failure_keeps_its_provider_cause() {
        let error = super::AgentError::Compaction(
            singularity_agent::compaction::CompactionError::Provider(
                singularity_model::ProviderError::new(
                    singularity_model::ModelErrorKind::AuthError,
                    "summary credentials rejected",
                ),
            ),
        );
        assert_eq!(
            super::turn_failure_cause(&error),
            super::TurnFailureCause::ProviderAuth
        );
    }
}
