//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影与终态落盘。
//!
//! 从 app-server 生命周期层提取的协调事实：
//! - 准备阶段 fail-fast：任何失败都不留下 turn 痕迹；
//! - `turn_started` 先于一切事件落盘；terminal/usage metadata 先于终态事件；
//! - 一个 turn 只打开一次会话文件，同一 [`SessionManager`] 贯穿全程；
//! - 投影是尽力而为的观察侧信道，投影失败只丢弃投影，不影响执行事实。

use std::path::PathBuf;
use std::sync::Arc;

use singularity_agent::agent::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentEvents, AgentOutcome, AgentTerminalReason,
};
use singularity_agent::compaction::CompactionConfig;
use singularity_agent::session::{
    SessionManager, SessionMetadata, SessionMetadataKind, WriterLockCoordinator,
};
use singularity_agent::tools::ToolRegistry;
use singularity_core::{CancellationToken, load_project_instructions_from_cwd};
use singularity_model::{DEFAULT_MAX_CONTEXT_TOKENS, ModelUsage, Provider, ProviderConfigSnapshot};
use singularity_protocol::diagnostic_code;
use uuid::Uuid;

use crate::assistant_items::AssistantItemEvents;
use crate::error::{
    ProviderFailureKind, TurnFailure, TurnFailureCause, TurnFailureStage, TurnRunError,
};
use crate::events::{AgentDiagnosticSeverity, TurnErrorDetail, TurnEvent, TurnEventSink};
use crate::objects::{
    ProviderStatus, Thread, ThreadStatus, Turn, TurnStatus, TurnUsage, turn_usage_from_model_usage,
};
use crate::terminal::{TerminalCommit, fail_stop_terminalization};

/// 项目指令截断的稳定诊断代码与模型可见尾注：截断事实同时告知客户端与模型。
const PROJECT_INSTRUCTIONS_TRUNCATED_NOTE: &str = "\n\n[warning] project instructions were truncated because they exceeded the size budget; content beyond the cut was not included.";

/// 一次 turn 执行的输入。
pub struct TurnParams {
    pub thread: Thread,
    pub input: String,
}

/// 用 headless core 执行一个 turn 的上下文：会话与 Agent 已在准备阶段构建，
/// 这里只运行 AgentLoop 并实时映射事件。
struct AgentRunContext<'a> {
    agent: &'a mut Agent,
    input_text: &'a str,
    cancellation: &'a CancellationToken,
    item_events: &'a mut AssistantItemEvents,
    sink: &'a mut dyn TurnEventSink,
}

/// 失败 turn 的终态提交上下文：落盘 Failed 终态并发布失败事件。
struct FailureCommitContext<'a> {
    session: &'a mut SessionManager,
    turn_id: &'a str,
    item_events: &'a mut AssistantItemEvents,
    error: &'a RunnerError,
    usage: ModelUsage,
    usage_complete: bool,
    sink: &'a mut dyn TurnEventSink,
}

/// 一次成功收敛到终态的 turn 结果。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub thread_id: String,
    pub turn_id: String,
    pub turn_status: TurnStatus,
    /// 最终 assistant 文本；中断/失败时可能为空。
    pub final_text: String,
    pub truncated: bool,
    pub usage: TurnUsage,
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

    pub fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    /// 进程内共享的写者锁协调器。
    pub fn coordinator(&self) -> &Arc<WriterLockCoordinator> {
        &self.coordinator
    }

    pub fn provider_snapshot(&self) -> &ProviderConfigSnapshot {
        &self.provider_snapshot
    }

    /// Provider 配置快照的只读展示投影（provider/status 的 wire 映射输入）。
    pub fn provider_status(&self) -> ProviderStatus {
        let snapshot = &self.provider_snapshot;
        let config = snapshot.redacted_config();
        let configuration = snapshot.configuration();
        ProviderStatus {
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

    /// 快照的默认模型 selector（未配置时为 None）。
    pub fn default_model_selector(&self) -> Option<String> {
        self.provider_snapshot.resolved_default_selector()
    }

    /// 校验模型 selector 能被快照解析为具体 provider 配置。
    pub fn validate_model_selector(&self, selector: Option<&str>) -> Result<(), String> {
        if let Some(selector) = selector
            && (self.provider_snapshot.has_explicit_model_selection()
                || selector.contains('/')
                || selector.contains('#'))
        {
            self.provider_snapshot
                .provider_for_selector(Some(selector))
                .map(|_| ())
                .map_err(|error| format!("invalid model selector: {error}"))?;
        }
        Ok(())
    }

    /// 在 turn 之外压缩既有 Thread，不写入 turn 生命周期 metadata。
    /// `cancellation` 由调用方持有，可随时中止压缩（TUI 中 Esc 取消）。
    pub fn compact_thread(
        &self,
        thread: &Thread,
        cancellation: &CancellationToken,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, String> {
        workspace_path(thread)?;
        let registry = tool_registry();
        let (provider, config, _) = self
            .resolve_agent_runtime(thread, &registry)
            .map_err(|error| error.to_string())?;
        let session = self
            .open_and_repair_session(thread)
            .map_err(|error| error.to_string())?;
        let mut agent =
            Agent::new(provider, registry, config, session).map_err(|error| error.to_string())?;
        agent
            .compact_now(cancellation)
            .map_err(|error| error.to_string())
    }

    /// 执行一个 turn 直到终态收敛。
    ///
    /// 调用方持有 [`crate::TurnControls`] 以便在执行期间注入输入或取消；
    /// 返回 `Ok` 时终态已持久化且终态事件已发出，返回
    /// [`TurnRunError::Execution`] 时 turn 已以失败终态收敛，
    /// 返回 [`TurnRunError::Terminalization`] 时 terminal metadata 无法落盘，
    /// 不存在任何虚假终态事件。
    pub fn run(
        &self,
        params: TurnParams,
        controls: &crate::conversation::TurnControls,
        sink: &mut dyn TurnEventSink,
    ) -> Result<TurnOutcome, TurnRunError> {
        let turn_id = Uuid::new_v4().to_string();
        let thread = params.thread;
        // fail-fast 准备：workspace、provider/config、会话打开修复、Agent 构造
        // 全部就绪后才写任何 turn 状态；准备阶段的失败发生在状态写入之前。
        workspace_path(&thread).map_err(|message| TurnRunError::Preparation {
            cause: TurnFailureCause::Workspace,
            message,
        })?;
        let registry = tool_registry();
        let (provider, config, instructions_truncated) = self
            .resolve_agent_runtime(&thread, &registry)
            .map_err(|error| TurnRunError::Preparation {
                cause: error.cause,
                message: error.to_string(),
            })?;
        let mut session =
            self.open_and_repair_session(&thread)
                .map_err(|error| TurnRunError::Preparation {
                    cause: TurnFailureCause::Store,
                    message: error.to_string(),
                })?;
        append_turn_started_metadata(&mut session, &turn_id).map_err(|error| {
            TurnRunError::Preparation {
                cause: TurnFailureCause::Store,
                message: error,
            }
        })?;
        let mut agent = self
            .prepare_agent(&turn_id, session, provider, registry, config, controls)
            .map_err(|error| TurnRunError::Preparation {
                cause: TurnFailureCause::Internal,
                message: error,
            })?;

        let turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: thread.thread_id.clone(),
            status: TurnStatus::Running,
            usage: None,
        };
        sink.emit(TurnEvent::TurnStarted { turn });
        if instructions_truncated {
            sink.emit(TurnEvent::Diagnostic {
                thread_id: thread.thread_id.clone(),
                turn_id: turn_id.clone(),
                severity: AgentDiagnosticSeverity::Warning,
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
        // 回收本轮唯一会话写者，供终态 metadata / usage 落盘复用。
        let mut session = agent.into_session();
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
                    session: &mut session,
                    turn_id: &turn_id,
                    item_events: &mut item_events,
                    error: &error,
                    usage: status.model_usage,
                    usage_complete: status.usage_complete,
                    sink,
                });
            }
            Err(error) => {
                return self.finish_failure(FailureCommitContext {
                    session: &mut session,
                    turn_id: &turn_id,
                    item_events: &mut item_events,
                    error: &error,
                    usage: ModelUsage::default(),
                    usage_complete: false,
                    sink,
                });
            }
        };

        // 终态收敛：单条原子落盘 `turn_terminal` → 终态事件。写入失败直接
        // fail-stop，绝不发布虚假终态或降级成另一个状态。
        // 不变量：status 为终态（completed/failed/interrupted）时 TerminalCommit 恒可构造。
        #[allow(clippy::expect_used)]
        let terminal = TerminalCommit::new(
            &turn_id,
            status.session_status,
            &status.model_usage,
            status.usage_complete,
        )
        .expect("run() only reaches this point with a terminal thread status");
        if let Err(storage_error) = terminal.persist(&mut session) {
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
        let final_turn = terminal.turn(&thread.thread_id, status.turn_status);
        sink.emit(TurnEvent::TurnCompleted {
            turn: final_turn.clone(),
        });
        Ok(TurnOutcome {
            thread_id: thread.thread_id,
            turn_id,
            turn_status: final_turn.status,
            final_text: status.final_answer.unwrap_or_default(),
            truncated: status.truncated,
            usage: final_turn
                .usage
                .unwrap_or_else(|| turn_usage_from_model_usage(&ModelUsage::default(), false)),
        })
    }

    /// 解析 Provider 与 AgentConfig 并预校验 compaction；任一失败直接失败，
    /// 不留 turn 痕迹。布尔返回值表示项目指令因预算超限被截断。
    fn resolve_agent_runtime(
        &self,
        thread: &Thread,
        registry: &ToolRegistry,
    ) -> Result<(Arc<dyn Provider + Send + Sync>, AgentConfig, bool), PreparationFailure> {
        let provider: Arc<dyn Provider + Send + Sync> = {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(provider) = &self.provider_override {
                Arc::clone(provider)
            } else {
                Arc::new(
                    self.provider_snapshot
                        .provider_for_selector(thread.model.as_deref())
                        .map_err(|error| PreparationFailure::internal(error.to_string()))?,
                )
            }
            #[cfg(not(any(test, feature = "test-support")))]
            Arc::new(
                self.provider_snapshot
                    .provider_for_selector(thread.model.as_deref())
                    .map_err(|error| PreparationFailure::internal(error.to_string()))?,
            )
        };
        let (config, instructions_truncated) =
            agent_config_for_thread(thread, provider.as_ref(), &self.provider_snapshot, registry)?;
        let provider_max_output_tokens = provider.protocol_contract().max_output_tokens;
        let config = AgentConfig::prepare_for_provider_limits(config, provider_max_output_tokens)
            .map_err(|error| PreparationFailure::internal(error.to_string()))?;
        Ok((provider, config, instructions_truncated))
    }

    fn open_and_repair_session(&self, thread: &Thread) -> Result<SessionManager, RunnerError> {
        let path = crate::store::thread_session_path(&self.sessions_dir, &thread.thread_id);
        let mut session = SessionManager::open_existing_with_coordinator(&path, &self.coordinator)
            .map_err(RunnerError::Session)?;
        // 头部 id 与请求不一致属于损坏状态。
        session
            .verify_session_id(&thread.thread_id)
            .map_err(RunnerError::Session)?;
        session
            .repair_interrupted_turns()
            .map_err(RunnerError::Session)?;
        session
            .repair_orphaned_tool_calls()
            .map_err(RunnerError::Session)?;
        Ok(session)
    }

    fn prepare_agent(
        &self,
        turn_id: &str,
        session: SessionManager,
        provider: Arc<dyn Provider + Send + Sync>,
        registry: ToolRegistry,
        config: AgentConfig,
        controls: &crate::conversation::TurnControls,
    ) -> Result<Agent, String> {
        let agent =
            Agent::new(provider, registry, config, session).map_err(|error| error.to_string())?;
        controls.register_inbox(turn_id, agent.inbox_handle());
        Ok(agent)
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
            turn_id,
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
        // 不变量：Failed 恒为终态，TerminalCommit 必可构造。
        #[allow(clippy::expect_used)]
        let terminal = TerminalCommit::new(turn_id, ThreadStatus::Failed, &usage, usage_complete)
            .expect("Failed always maps to a terminal status");
        // 失败终态无法落盘同样 fail-stop：不发布任何终态事件。
        if terminal.persist(session).is_err() {
            fail_stop_terminalization(session.session_id(), turn_id, &failure, sink);
            return Err(TurnRunError::Terminalization(failure));
        }
        let thread_id = session.session_id().to_string();
        self.emit_failure_terminal_events(turn_id, &thread_id, item_events, &failure, sink);
        Err(TurnRunError::Execution(failure))
    }

    /// 尽力发送失败 item 与 turn 级终态事件；一个事件失败不阻断另一个。
    fn emit_failure_terminal_events(
        &self,
        turn_id: &str,
        thread_id: &str,
        item_events: &mut AssistantItemEvents,
        failure: &TurnFailure,
        sink: &mut dyn TurnEventSink,
    ) {
        item_events.emit_assistant_terminal_failed(sink);
        for tool_call_id in item_events.open_tool_items() {
            item_events.emit_tool_terminal(sink, &tool_call_id, true);
        }
        let message = failure.original.clone().unwrap_or_else(|| {
            format!(
                "turn failed during {} ({})",
                failure.stage.as_str(),
                failure.cause.wire_str()
            )
        });
        sink.emit(TurnEvent::TurnFailed {
            turn: Turn {
                turn_id: turn_id.to_string(),
                thread_id: thread_id.to_string(),
                status: TurnStatus::Failed,
                usage: None,
            },
            error: TurnErrorDetail {
                stage: failure.stage,
                cause: failure.cause,
                message,
            },
        });
    }
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
        RunnerError::Agent(AgentError::Provider(provider_error)) => {
            ProviderFailureKind::from_model_error_kind(&provider_error.error.kind).into()
        }
        RunnerError::Agent(_) => TurnFailureCause::Internal,
    }
}

fn turn_failure_from_error(error: &RunnerError, fallback_stage: TurnFailureStage) -> TurnFailure {
    TurnFailure {
        stage: fallback_stage,
        cause: turn_failure_cause(error),
        original: Some(error.to_string()),
    }
}

/// turn_started 通过本轮已打开的同一 `SessionManager` 落盘（开始标记）。
fn append_turn_started_metadata(session: &mut SessionManager, turn_id: &str) -> Result<(), String> {
    let already_started = session.metadata_entries().iter().any(|entry| {
        entry.turn_id() == Some(turn_id) && entry.kind() == SessionMetadataKind::TurnStarted
    });
    if !already_started {
        session
            .append_metadata(SessionMetadata::turn_started(turn_id))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// AgentLoop 结束时的中间状态投影。
struct RunStatus {
    turn_status: TurnStatus,
    session_status: ThreadStatus,
    final_answer: Option<String>,
    truncated: bool,
    model_usage: ModelUsage,
    usage_complete: bool,
    error: Option<String>,
}

fn outcome_to_run_status(outcome: AgentOutcome) -> RunStatus {
    let mut status = RunStatus {
        turn_status: TurnStatus::Failed,
        session_status: ThreadStatus::Failed,
        final_answer: None,
        truncated: outcome.truncated,
        model_usage: outcome.usage,
        usage_complete: outcome.usage_complete,
        error: None,
    };
    match outcome.terminal_reason {
        AgentTerminalReason::Aborted => {
            status.turn_status = TurnStatus::Interrupted;
            status.session_status = ThreadStatus::Interrupted;
        }
        AgentTerminalReason::Completed if outcome.final_text.trim().is_empty() => {
            status.error = Some("agent loop stopped without a final assistant message".to_string());
        }
        AgentTerminalReason::Completed => {
            status.turn_status = TurnStatus::Completed;
            status.session_status = ThreadStatus::Completed;
            status.final_answer = Some(outcome.final_text);
        }
        AgentTerminalReason::Failed => {
            status.error = Some("agent loop stopped without a final assistant message".to_string());
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

/// 准备阶段失败：分类 + 真实原因文本（对外前仍需敏感边界）。
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

fn agent_config_for_thread(
    thread: &Thread,
    provider: &dyn Provider,
    snapshot: &ProviderConfigSnapshot,
    registry: &ToolRegistry,
) -> Result<(AgentConfig, bool), PreparationFailure> {
    let cwd = workspace_path(thread).map_err(|message| PreparationFailure {
        cause: TurnFailureCause::Workspace,
        message,
    })?;
    let cwd_path = std::path::Path::new(&cwd).to_path_buf();
    // 工具名单从 ToolRegistry 动态生成：提示词只列出工具名，完整定义由模型
    // 通过服务端 schema 获取。
    let tool_names = registry
        .names()
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let base_prompt = singularity_agent::prompts::build_system_prompt(
        &cwd_path.display().to_string(),
        &tool_names,
    );
    // 预算超限走截断 + 告警路径：截断事实对模型可见（系统提示词尾注），并经
    // turn_started 之后的诊断事件告知客户端。真 I/O 错误仍 fail closed。
    let (system_prompt, instructions_truncated) =
        match load_project_instructions_from_cwd(&cwd_path) {
            Ok(Some(instructions)) => {
                let mut system_prompt = format!(
                    "{base_prompt}\n\n--- project instructions ---\n{}",
                    instructions.content()
                );
                if instructions.truncated() {
                    system_prompt.push_str(PROJECT_INSTRUCTIONS_TRUNCATED_NOTE);
                }
                (system_prompt, instructions.truncated())
            }
            Ok(None) => (base_prompt, false),
            Err(error) => {
                return Err(PreparationFailure {
                    cause: TurnFailureCause::ProjectInstructions,
                    message: error.to_string(),
                });
            }
        };
    let context_window = provider
        .protocol_contract()
        .max_context_tokens
        .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS) as u64;
    let max_output_tokens = provider.protocol_contract().max_output_tokens as u64;
    Ok((
        AgentConfig {
            model: thread
                .model
                .clone()
                .or_else(|| snapshot.resolved_default_selector())
                .unwrap_or_default(),
            system_prompt,
            context_window,
            max_output_tokens,
            compaction: CompactionConfig::default(),
            retry: singularity_agent::agent::TurnRetryConfig::default(),
        },
        instructions_truncated,
    ))
}

fn tool_registry() -> ToolRegistry {
    ToolRegistry::new()
}
