//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影与终态落盘。
//!
//! 执行不变量：
//! - 准备阶段 fail-fast：任何失败都不留下 operation 痕迹；
//! - 设置记录与本 turn 的 `operation_started` 先于一切事件落盘；终态记录
//!   （`operation_finished`，status/usage/truncated 单条）先于终态事件；
//! - 一个 turn 只打开一次会话文件，同一 [`SessionManager`] 贯穿全程；
//! - 投影是尽力而为的观察侧信道，投影失败只丢弃投影，不影响执行事实。

use std::path::PathBuf;
use std::sync::Arc;

use singularity_agent::agent::TurnInbox;
use singularity_agent::agent::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentEvents, AgentOutcome, AgentTerminalReason,
};
use singularity_agent::compaction::CompactionConfig;
use singularity_agent::prompts::PromptAssembly;
use singularity_agent::session::{
    CompactionReason, ControlDisposition, ControlRequest, LedgerRecord, OperationIntent,
    OperationKind, SessionAccess, SessionManager, SessionMetadata, SessionWriter,
    WriterLockCoordinator, lock_writer,
};
use singularity_agent::tools::ToolRegistrySnapshot;
use singularity_core::{CancellationToken, load_project_instructions_from_cwd};
use singularity_model::{
    DEFAULT_PROVIDER_NAME, ModelConfigurationSnapshot, ModelUsage, Provider,
    ProviderConfigSnapshot, split_model_selector,
};
use singularity_protocol::diagnostic_code;
use uuid::Uuid;

use crate::assistant_items::AssistantItemEvents;
use crate::error::{
    TurnFailure, TurnFailureCause, TurnFailureStage, TurnRunError, provider_turn_cause,
};
use crate::events::{DiagnosticSeverity, TurnErrorDetail, TurnEvent};
use crate::objects::{Thread, Turn, TurnModelUsage, TurnStatus};
use crate::terminal::{TerminalCommit, fail_stop_terminalization};

/// 一次 turn 执行的输入。
pub struct TurnParams {
    pub thread: Thread,
    pub input: String,
    /// Optional per-execution selector override. It is resolved into this
    /// turn's immutable model snapshot and is never written as Thread settings.
    pub model_override: Option<String>,
    /// 本回合由协调器接受的 followUp/requeued steer 控制的 durable 请求
    /// （携带控制 identity、payload 与 FIFO 接受序号）；普通显式输入为
    /// `None`。有值时 runner 在本 turn 的 `operation_started` 之后、任何
    /// 实时事件之前落 `control_accepted` 终态 disposition
    /// （`started_as_new_turn`）。
    pub control: Option<ControlRequest>,
}

/// 用 headless core 执行一个 turn 的上下文：会话与 Agent 已在准备阶段构建，
/// 这里只运行 AgentLoop 并实时映射事件。
struct AgentRunContext<'a> {
    agent: &'a mut Agent,
    input_text: &'a str,
    cancellation: &'a CancellationToken,
    item_events: &'a mut AssistantItemEvents,
    sink: &'a mut dyn FnMut(TurnEvent),
}

/// 失败 turn 的终态提交上下文：落盘 Failed 终态并发布失败事件。
struct FailureCommitContext<'a> {
    session: &'a SessionWriter,
    operation_id: &'a str,
    turn_id: &'a str,
    controls: &'a crate::conversation::TurnControls,
    item_events: &'a mut AssistantItemEvents,
    error: &'a RunnerError,
    usage: ModelUsage,
    usage_complete: bool,
    sink: &'a mut dyn FnMut(TurnEvent),
}

/// 一次收敛到可信终态的 turn 结果（completed/failed/interrupted 都是可信
/// 终态；不存在可信终态的情形由 [`TurnRunError`] 表达）。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn_id: String,
    pub turn_status: TurnStatus,
    /// 最终 assistant 文本；中断/失败时可能为空。
    pub final_text: String,
    pub truncated: bool,
    pub usage: TurnModelUsage,
    /// 失败终态的协议错误细节（stage/cause/message 与已发布的 `turn/error`
    /// 事件同源）；非失败终态为 `None`。客户端据此报告进程结果，
    /// 不再从事件流重建终态事实。
    pub error: Option<TurnErrorDetail>,
    /// 终态后仍留在注入箱、未在本次 turn 交付的转向输入（中断时退还调用方）。
    pub undelivered_inputs: Vec<String>,
}

/// 进程内 turn 执行器：无状态、可共享，按需构造。
pub struct TurnRunner {
    sessions_dir: PathBuf,
    provider_snapshot: ProviderConfigSnapshot,
    /// 进程级写者锁协调器：所有会话打开路径共用，stale 清理每进程一次。
    coordinator: Arc<WriterLockCoordinator>,
    #[cfg(any(test, feature = "test-support"))]
    provider_override: Option<Arc<dyn Provider + Send + Sync>>,
}

impl TurnRunner {
    pub fn new(sessions_dir: PathBuf, provider_snapshot: ProviderConfigSnapshot) -> Self {
        let coordinator = Arc::new(WriterLockCoordinator::new(&sessions_dir));
        Self {
            sessions_dir,
            provider_snapshot,
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

    /// 会话目录与进程内写者锁协调器：目录操作只经 [`crate::ThreadCatalog`]
    /// 暴露给客户端，此处仅供 runtime 内部（目录接缝与会话打开路径）使用。
    pub(crate) fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    pub(crate) fn coordinator(&self) -> &Arc<WriterLockCoordinator> {
        &self.coordinator
    }

    /// 打开本轮唯一会话写者（含崩溃修复并返回 [`SessionWriter`]）。
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

    /// 在活动 turn 之外落盘一条控制终态 disposition（如撤回 followUp）：
    /// 短开会话写者追加后释放。只在无活动 turn（无写者占用）时使用；活动
    /// turn 期间走 `TurnControls` 的共享写者路径。
    pub fn append_control_disposition(
        &self,
        thread: &Thread,
        request: &ControlRequest,
        disposition: ControlDisposition,
    ) -> Result<(), String> {
        let mut session = self
            .open_and_repair_session(thread)
            .map_err(|error| error.to_string())?;
        session
            .append_record(request.disposition_record(disposition))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// 快照的默认模型 selector（未配置时为 None）。
    pub fn default_model_selector(&self) -> Option<String> {
        self.provider_snapshot.resolved_default_selector()
    }

    /// 校验模型 selector 能被快照解析为具体 provider 配置。
    pub fn validate_model_selector(&self, selector: Option<&str>) -> Result<(), String> {
        if let Some(selector) = selector {
            self.provider_snapshot
                .provider_for_selector(Some(selector))
                .map(|_| ())
                .map_err(|error| format!("invalid model selector: {error}"))?;
        }
        Ok(())
    }

    /// 在 turn 之外压缩既有 Thread：以独立 compaction operation 落盘
    /// （`operation_started`/`operation_finished`，无 turn 绑定）。
    /// `cancellation` 由调用方持有，可随时中止压缩（TUI 中 Esc 取消）。
    pub fn compact_thread(
        &self,
        thread: &Thread,
        cancellation: &CancellationToken,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, String> {
        workspace_path(thread)?;
        let registry = tool_registry();
        let (provider, config, model, _) = self
            .resolve_agent_runtime(thread, &registry)
            .map_err(|error| error.to_string())?;
        let session = self
            .open_and_repair_session(thread)
            .map_err(|error| error.to_string())?;
        let operation_id = Uuid::now_v7().to_string();
        let writer: SessionWriter = Arc::new(std::sync::Mutex::new(session));
        let mut agent = Agent::new(
            TurnInbox::default_handle(),
            provider,
            model,
            registry,
            config,
            Arc::clone(&writer),
            operation_id.clone(),
        )
        .map_err(|error| error.to_string())?;
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationStarted {
                operation_id: operation_id.clone(),
                kind: OperationKind::Compaction,
                turn_id: None,
                intent: OperationIntent::Compaction {
                    reason: CompactionReason::Manual,
                },
            })
            .map_err(|error| error.to_string())?;
        let outcome = agent.compact_now(cancellation);
        let terminal_status = match &outcome {
            Ok(_) => TurnStatus::Completed,
            Err(AgentError::Compaction(
                singularity_agent::compaction::CompactionError::Aborted,
            )) => TurnStatus::Interrupted,
            Err(AgentError::Compaction(
                singularity_agent::compaction::CompactionError::Provider(error),
            )) if error.error.kind == singularity_model::ModelErrorKind::Cancelled => {
                TurnStatus::Interrupted
            }
            Err(_) => TurnStatus::Failed,
        };
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationFinished {
                operation_id,
                turn_id: None,
                outcome: terminal_status,
                usage: None,
                truncated: false,
            })
            .map_err(|error| error.to_string())?;
        outcome.map_err(|error| error.to_string())
    }

    /// 执行一个 turn 直到终态收敛。
    ///
    /// 调用方持有 [`crate::TurnControls`] 以便在执行期间注入输入或取消；
    /// 返回 `Ok` 时终态（completed/failed/interrupted）已持久化且终态事件
    /// 已发出——失败终态的 [`TurnOutcome::error`] 携带与 `turn/error` 事件
    /// 同源的协议错误细节；返回 [`TurnRunError::Terminalization`] 时终态
    /// 记录无法落盘，不存在任何虚假终态事件。
    pub fn run(
        &self,
        params: TurnParams,
        controls: &crate::conversation::TurnControls,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> Result<TurnOutcome, TurnRunError> {
        let turn_id = controls.turn_id.clone();
        let thread = params.thread;
        let execution_thread = params.model_override.as_ref().map_or_else(
            || thread.clone(),
            |model| {
                let mut thread = thread.clone();
                thread.model = Some(model.clone());
                thread
            },
        );
        // 会话写者由协调器在 turn 开始前打开（含 workspace 检查与崩溃修复）；
        // 这里只做剩余 fail-fast 准备（provider/config/项目指令），全部就绪
        // 后才写任何 operation 状态。
        let writer = controls.writer();
        let registry = tool_registry();
        let (provider, config, model, instructions_truncated) = self
            .resolve_agent_runtime(&execution_thread, &registry)
            .map_err(|error| TurnRunError::Preparation {
                cause: error.cause,
                message: error.to_string(),
            })?;
        record_thread_settings_metadata(&mut lock_writer(&writer), &thread).map_err(|error| {
            TurnRunError::Preparation {
                cause: TurnFailureCause::Store,
                message: error,
            }
        })?;
        // durable-before-publish：operation_started 先于任何实时事件落盘；
        // run 意图携带本 turn 规范化、不可变的用户输入（crash window 不
        // 丢失已接受 run 的完整输入意图）。
        let operation_id = Uuid::now_v7().to_string();
        let mut agent = Agent::new(
            controls.inbox_handle(),
            provider,
            model.clone(),
            registry,
            config,
            writer.clone(),
            operation_id.clone(),
        )
        .map_err(|error| TurnRunError::Preparation {
            cause: TurnFailureCause::Store,
            message: error.to_string(),
        })?;
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationStarted {
                operation_id: operation_id.clone(),
                kind: OperationKind::Run,
                turn_id: Some(turn_id.clone()),
                intent: OperationIntent::Run {
                    model,
                    input: params.input.clone(),
                },
            })
            .map_err(|error| TurnRunError::Preparation {
                cause: TurnFailureCause::Store,
                message: error.to_string(),
            })?;
        // followUp/requeued steer 控制的 durable 归宿：本 turn 以它启动，
        // 终态 disposition 先于任何实时事件落盘（协调器是唯一 FIFO owner，
        // runner 是它落 ledger 的唯一写入路径；接受时的 pending 记录已在
        // 协调器侧落盘）。
        if let Some(request) = &params.control {
            lock_writer(&writer)
                .append_record(request.disposition_record(ControlDisposition::StartedAsNewTurn))
                .map_err(|error| TurnRunError::Preparation {
                    cause: TurnFailureCause::Store,
                    message: error.to_string(),
                })?;
        }
        let turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: thread.thread_id.clone(),
            status: TurnStatus::Running,
            usage: None,
        };
        sink(TurnEvent::TurnStarted { turn });
        if instructions_truncated {
            sink(TurnEvent::Diagnostic {
                thread_id: thread.thread_id.clone(),
                turn_id: turn_id.clone(),
                severity: DiagnosticSeverity::Warning,
                code: diagnostic_code::PROJECT_INSTRUCTIONS_TRUNCATED.to_string(),
                message:
                    "project instructions were truncated because they exceeded the size budget"
                        .to_string(),
            });
        }

        let mut item_events = AssistantItemEvents::new(
            thread.thread_id.clone(),
            turn_id.clone(),
            format!("{turn_id}_assistant"),
        );
        let run_result = self.run_agent_core(AgentRunContext {
            agent: &mut agent,
            input_text: &params.input,
            cancellation: &controls.cancellation,
            item_events: &mut item_events,
            sink,
        });
        // AgentLoop 已停止后立即关闭实时注入窗口；终态后的输入必须通过新的
        // turn 发起，不能在内存中静默排队。
        controls.close_inbox();
        if let Some(storage_error) = controls.take_storage_failure() {
            let failure = TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: Some(storage_error),
            };
            fail_stop_terminalization(&thread.thread_id, &turn_id, &failure, sink);
            return Err(TurnRunError::Terminalization(failure));
        }
        // 本轮唯一会话写者贯穿全程：取消控制与终态记录 / usage 落盘复用
        // 同一共享写者（不重新打开）。
        let status = match run_result {
            Ok(status)
                if matches!(
                    status.turn_status,
                    TurnStatus::Completed | TurnStatus::Interrupted
                ) =>
            {
                status
            }
            Ok(status) => {
                let error =
                    RunnerError::Agent(AgentError::Loop(status.error.unwrap_or_else(|| {
                        "agent loop did not reach a terminal result".to_string()
                    })));
                return self.finish_failure(FailureCommitContext {
                    session: &writer,
                    operation_id: &operation_id,
                    turn_id: &turn_id,
                    controls,
                    item_events: &mut item_events,
                    error: &error,
                    usage: status.model_usage,
                    usage_complete: status.usage_complete,
                    sink,
                });
            }
            Err(error) => {
                return self.finish_failure(FailureCommitContext {
                    session: &writer,
                    operation_id: &operation_id,
                    turn_id: &turn_id,
                    controls,
                    item_events: &mut item_events,
                    error: &error,
                    usage: ModelUsage::default(),
                    usage_complete: false,
                    sink,
                });
            }
        };

        // 终态收敛：本 turn 已接受的取消控制先落盘，再单条原子落盘
        // `operation_finished` → 终态事件。任一写入失败都直接 fail-stop，
        // 绝不发布虚假终态或降级成另一个状态。
        // 不变量：status.turn_status 为终态（completed/interrupted）时
        // TerminalCommit 恒可构造（此路径排除了 Failed 与非终态）。
        #[allow(clippy::expect_used)]
        let terminal = TerminalCommit::new(
            &operation_id,
            &turn_id,
            status.turn_status,
            &status.model_usage,
            status.usage_complete,
            status.truncated,
        )
        .expect("run() only reaches this point with a terminal turn status");
        let undelivered = controls.drain_inbox_before_terminal();
        if let Some(storage_error) = controls.take_storage_failure() {
            let failure = TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: Some(storage_error),
            };
            fail_stop_terminalization(&thread.thread_id, &turn_id, &failure, sink);
            return Err(TurnRunError::Terminalization(failure));
        }
        if status.turn_status == TurnStatus::Interrupted {
            for request in &undelivered {
                if let Err(storage_error) =
                    controls.append_disposition(request, ControlDisposition::Cancelled)
                {
                    let failure = TurnFailure {
                        stage: TurnFailureStage::TerminalOutcome,
                        cause: TurnFailureCause::Store,
                        original: Some(storage_error),
                    };
                    fail_stop_terminalization(&thread.thread_id, &turn_id, &failure, sink);
                    return Err(TurnRunError::Terminalization(failure));
                }
            }
        }
        let flush_result = flush_cancel_acceptances(&mut lock_writer(&writer), controls);
        if let Err(storage_error) =
            flush_result.and_then(|()| terminal.persist(&mut lock_writer(&writer)))
        {
            let failure = TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: Some(storage_error),
            };
            fail_stop_terminalization(&thread.thread_id, &turn_id, &failure, sink);
            return Err(TurnRunError::Terminalization(failure));
        }
        // 取消可能打断已开始 item 的工具执行：终态事件前补齐所有未闭合 item。
        for tool_call_id in item_events.open_tool_items() {
            item_events.emit_tool_terminal(sink, &tool_call_id, true);
        }
        item_events.emit_assistant_terminal_completed(sink);
        let final_turn = terminal.turn(&thread.thread_id);
        sink(TurnEvent::TurnCompleted {
            turn: final_turn.clone(),
        });
        Ok(TurnOutcome {
            turn_id,
            turn_status: final_turn.status,
            final_text: status.final_answer.unwrap_or_default(),
            truncated: status.truncated,
            usage: terminal.usage().clone(),
            error: None,
            undelivered_inputs: Vec::new(),
        })
    }

    /// 解析 Provider、AgentConfig 与本 turn 冻结的模型配置快照并预校验
    /// compaction；任一失败直接失败，不留 operation 痕迹。元组末位布尔表示
    /// 项目指令因预算超限被截断。
    fn resolve_agent_runtime(
        &self,
        thread: &Thread,
        registry: &ToolRegistrySnapshot,
    ) -> Result<
        (
            Arc<dyn Provider + Send + Sync>,
            AgentConfig,
            ModelConfigurationSnapshot,
            bool,
        ),
        PreparationFailure,
    > {
        let provider: Arc<dyn Provider + Send + Sync> = {
            #[cfg(any(test, feature = "test-support"))]
            let overridden = self.provider_override.clone();
            #[cfg(not(any(test, feature = "test-support")))]
            let overridden: Option<Arc<dyn Provider + Send + Sync>> = None;
            match overridden {
                Some(provider) => provider,
                None => Arc::new(
                    self.provider_snapshot
                        .provider_for_selector(thread.model.as_deref())
                        .map_err(|error| PreparationFailure::internal(error.to_string()))?,
                ),
            }
        };
        let model = provider.model_configuration();
        let (config, instructions_truncated) = agent_config_for_thread(thread, &model, registry)?;
        Ok((provider, config, model, instructions_truncated))
    }

    fn open_and_repair_session(&self, thread: &Thread) -> Result<SessionManager, RunnerError> {
        let path = crate::store::thread_session_path(&self.sessions_dir, &thread.thread_id);
        SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            &thread.thread_id,
            SessionAccess::RepairWrite,
        )
        .map_err(RunnerError::Session)
    }

    /// 用 headless core 执行一个 turn：会话与 Agent 已在准备阶段构建，
    /// 这里只运行 AgentLoop 并实时映射事件。
    fn run_agent_core(&self, context: AgentRunContext<'_>) -> Result<RunStatus, RunnerError> {
        let AgentRunContext {
            agent,
            input_text,
            cancellation,
            item_events,
            sink,
        } = context;
        let run_result = {
            let mut events = AgentEvents::new();
            let mut on_event = |event: AgentEvent| item_events.project(sink, event);
            events.on_event = Some(&mut on_event);
            agent.run(input_text, &mut events, cancellation)
        };
        match run_result {
            Ok(outcome) => Ok(outcome_to_run_status(outcome)),
            Err(error) => Err(RunnerError::Agent(error)),
        }
    }

    fn finish_failure(
        &self,
        context: FailureCommitContext<'_>,
    ) -> Result<TurnOutcome, TurnRunError> {
        let FailureCommitContext {
            session,
            operation_id,
            turn_id,
            controls,
            item_events,
            error,
            usage,
            usage_complete,
            sink,
        } = context;
        let (usage, usage_complete) = match error {
            RunnerError::Agent(AgentError::RunFailed { outcome, .. }) => {
                (outcome.usage.clone(), outcome.usage_complete)
            }
            _ => (usage, usage_complete),
        };
        let failure = turn_failure_from_error(error, TurnFailureStage::AgentLoop);
        let _undelivered = controls.drain_inbox_before_terminal();
        if let Some(storage_error) = controls.take_storage_failure() {
            let terminal_failure = TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: Some(storage_error),
            };
            let thread_id = lock_writer(session).session_id().to_string();
            fail_stop_terminalization(&thread_id, turn_id, &terminal_failure, sink);
            return Err(TurnRunError::Terminalization(terminal_failure));
        }
        // 不变量：Failed 恒为终态，TerminalCommit 必可构造。
        #[allow(clippy::expect_used)]
        let terminal = TerminalCommit::new(
            operation_id,
            turn_id,
            TurnStatus::Failed,
            &usage,
            usage_complete,
            false,
        )
        .expect("Failed always maps to a terminal status");
        // 取消控制与失败终态无法落盘同样 fail-stop：不发布任何终态事件。
        let flush_result = flush_cancel_acceptances(&mut lock_writer(session), controls);
        if let Err(storage_error) =
            flush_result.and_then(|()| terminal.persist(&mut lock_writer(session)))
        {
            let failure = TurnFailure {
                stage: TurnFailureStage::TerminalOutcome,
                cause: TurnFailureCause::Store,
                original: Some(storage_error),
            };
            fail_stop_terminalization(lock_writer(session).session_id(), turn_id, &failure, sink);
            return Err(TurnRunError::Terminalization(failure));
        }
        let thread_id = lock_writer(session).session_id().to_string();
        let error_detail =
            self.emit_failure_terminal_events(&thread_id, item_events, &failure, &terminal, sink);
        Ok(TurnOutcome {
            turn_id: turn_id.to_string(),
            turn_status: TurnStatus::Failed,
            final_text: String::new(),
            truncated: false,
            usage: terminal.usage().clone(),
            error: Some(error_detail),
            undelivered_inputs: Vec::new(),
        })
    }

    /// 尽力发送失败 item 与 turn 级终态事件；一个事件失败不阻断另一个。
    /// 终态事件携带已落盘的 usage：失败轮同样报告真实成本。返回事件携带的
    /// 同一错误细节，供 `TurnOutcome` 交付调用方（事件与结果同源单构造）。
    fn emit_failure_terminal_events(
        &self,
        thread_id: &str,
        item_events: &mut AssistantItemEvents,
        failure: &TurnFailure,
        terminal: &TerminalCommit,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> TurnErrorDetail {
        item_events.emit_assistant_terminal_failed(sink);
        for tool_call_id in item_events.open_tool_items() {
            item_events.emit_tool_terminal(sink, &tool_call_id, true);
        }
        let message = failure
            .original
            .clone()
            .unwrap_or_else(|| format!("turn failed during {} ({})", failure.stage, failure.cause));
        let error = TurnErrorDetail {
            stage: failure.stage,
            cause: failure.cause,
            message,
        };
        let turn = terminal.turn(thread_id);
        sink(TurnEvent::TurnFailed {
            thread_id: turn.thread_id,
            turn_id: turn.turn_id,
            error: error.clone(),
        });
        error
    }
}

/// 把本 turn 已接受的取消控制落盘（durable-before-publish：先于终态记录）。
/// 存储失败以 `Err` 上抛，调用方与终态写入共用同一 fail-stop 出口。
fn flush_cancel_acceptances(
    session: &mut SessionManager,
    controls: &crate::conversation::TurnControls,
) -> Result<(), String> {
    if let Some(failure) = controls.take_storage_failure() {
        return Err(failure);
    }
    for request in controls.take_cancel_acceptances() {
        session
            .append_record(request.disposition_record(ControlDisposition::Cancelled))
            .map_err(|error| error.to_string())?;
    }
    if let Some(failure) = controls.take_storage_failure() {
        return Err(failure);
    }
    Ok(())
}

/// 准备/执行阶段的内部错误表示，分类为 [`TurnFailure`] 时使用。
enum RunnerError {
    Session(singularity_agent::session::SessionError),
    Agent(AgentError),
}

impl std::fmt::Display for RunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session(error) => write!(f, "{error}"),
            Self::Agent(error) => write!(f, "{error}"),
        }
    }
}

fn turn_failure_cause(error: &RunnerError) -> TurnFailureCause {
    match error {
        RunnerError::Session(_) => TurnFailureCause::Store,
        RunnerError::Agent(agent_error) => agent_turn_failure_cause(agent_error),
    }
}

/// RunFailed 是「已积累持久事实后的失败」包装：分类必须穿透包装还原权威
/// 根因，否则带进度的 provider 失败会被误报为 internal。
fn agent_turn_failure_cause(error: &AgentError) -> TurnFailureCause {
    match error {
        AgentError::Provider(provider_error) => provider_turn_cause(provider_error.error.kind),
        AgentError::RunFailed { error, .. } => agent_turn_failure_cause(error),
        AgentError::Session(_) => TurnFailureCause::Store,
        AgentError::Compaction(singularity_agent::compaction::CompactionError::Session(_)) => {
            TurnFailureCause::Store
        }
        AgentError::Compaction(_) | AgentError::Loop(_) => TurnFailureCause::Internal,
    }
}

fn turn_failure_from_error(error: &RunnerError, fallback_stage: TurnFailureStage) -> TurnFailure {
    TurnFailure {
        stage: fallback_stage,
        cause: turn_failure_cause(error),
        original: Some(error.to_string()),
    }
}

/// 设置持久化点：变更提交点只更新内存投影（运行中同样接受），本函数在
/// turn 开始时于本轮已打开的同一会话写者上做 turn 边界记录：写入当前
/// selector。与最后一条已记录值相同则跳过，不产生重复行；
/// Thread 无模型覆盖时不记录。
fn record_thread_settings_metadata(
    session: &mut SessionManager,
    thread: &Thread,
) -> Result<(), String> {
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
        .map_err(|error| error.to_string())
}

/// AgentLoop 结束时的中间状态投影；turn 终态是单字段事实，
/// JSONL 终态词形在需要处由它派生。
struct RunStatus {
    turn_status: TurnStatus,
    final_answer: Option<String>,
    truncated: bool,
    model_usage: ModelUsage,
    usage_complete: bool,
    error: Option<String>,
}

fn outcome_to_run_status(outcome: AgentOutcome) -> RunStatus {
    let mut status = RunStatus {
        turn_status: TurnStatus::Failed,
        final_answer: None,
        truncated: outcome.truncated,
        model_usage: outcome.usage,
        usage_complete: outcome.usage_complete,
        error: None,
    };
    match outcome.terminal_reason {
        AgentTerminalReason::Aborted => {
            status.turn_status = TurnStatus::Interrupted;
        }
        AgentTerminalReason::Completed if outcome.final_text.trim().is_empty() => {
            status.error = Some("agent loop stopped without a final assistant message".to_string());
        }
        AgentTerminalReason::Completed => {
            status.turn_status = TurnStatus::Completed;
            status.final_answer = Some(outcome.final_text);
        }
    }
    status
}

fn workspace_path(thread: &Thread) -> Result<String, String> {
    if thread.cwd.trim().is_empty() || !std::path::Path::new(&thread.cwd).is_absolute() {
        return Err("thread does not have an absolute workspace".to_string());
    }
    Ok(thread.cwd.clone())
}

/// 准备阶段失败：分类 + 真实原因文本（认证材料不进入错误文本）。
struct PreparationFailure {
    cause: TurnFailureCause,
    message: String,
}

impl PreparationFailure {
    fn internal(message: String) -> Self {
        Self {
            cause: TurnFailureCause::Internal,
            message,
        }
    }
}

impl std::fmt::Display for PreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 装配一次 turn 的 AgentConfig：系统提示词由 [`PromptAssembly`] 单点拥有
/// （基础人格 + 工具名单 + 项目指令），模型/压缩事实由 provider 快照与默认
/// 压缩配置注入。预算超限走截断 + 告警路径：截断事实对模型可见（系统提示词
/// 尾注），并经 operation_started 之后的诊断事件告知客户端。真 I/O 错误仍
/// fail closed。
fn agent_config_for_thread(
    thread: &Thread,
    _model: &ModelConfigurationSnapshot,
    registry: &ToolRegistrySnapshot,
) -> Result<(AgentConfig, bool), PreparationFailure> {
    let cwd = workspace_path(thread).map_err(|message| PreparationFailure {
        cause: TurnFailureCause::Workspace,
        message,
    })?;
    let cwd_path = std::path::Path::new(&cwd).to_path_buf();
    let instructions =
        load_project_instructions_from_cwd(&cwd_path).map_err(|error| PreparationFailure {
            cause: TurnFailureCause::ProjectInstructions,
            message: error.to_string(),
        })?;
    let assembled = PromptAssembly::assemble(&cwd, registry, instructions.as_ref());
    Ok((
        AgentConfig {
            system_prompt: assembled.system_prompt,
            compaction: CompactionConfig::default(),
        },
        assembled.instructions_truncated,
    ))
}

fn tool_registry() -> ToolRegistrySnapshot {
    ToolRegistrySnapshot::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use singularity_model::{ModelError, ModelErrorKind, ProviderError};

    fn run_failed(error: AgentError) -> RunnerError {
        RunnerError::Agent(AgentError::RunFailed {
            error: Box::new(error),
            outcome: Box::new(singularity_agent::agent::AgentOutcome {
                final_text: String::new(),
                truncated: false,
                turns: 1,
                usage: singularity_model::ModelUsage::default(),
                usage_complete: false,
                terminal_reason: singularity_agent::agent::AgentTerminalReason::Completed,
            }),
        })
    }

    /// 失败归因穿透 RunFailed 包装：带进度的 provider 失败不得退化为
    /// internal（回归：turn_failure_cause 的递归分支）。
    #[test]
    fn run_failed_wrapped_provider_error_keeps_provider_cause() {
        let provider = AgentError::Provider(ProviderError::from_model_error(ModelError::new(
            ModelErrorKind::RateLimited,
            "rate limited",
        )));
        assert_eq!(
            turn_failure_cause(&run_failed(provider)),
            TurnFailureCause::ProviderRateLimited
        );
    }

    #[test]
    fn run_failed_wrapped_loop_error_is_internal() {
        assert_eq!(
            turn_failure_cause(&run_failed(AgentError::Loop("invariant".to_string()))),
            TurnFailureCause::Internal
        );
    }
}
