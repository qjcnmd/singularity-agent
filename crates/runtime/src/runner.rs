//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影与终态落盘。
//!
//! 执行不变量：
//! - 准备失败与 operation_started 成功后的提交失败分开归类；
//! - 本 turn 的 operation_started 先于一切事件落盘；终态记录
//!   （operation_finished，status/usage/truncated 单条）先于终态事件；
//! - 一个 turn 只打开一次会话文件，同一 SessionManager 贯穿全程；
//! - 投影是尽力而为的观察侧信道，投影失败只丢弃投影，不影响执行事实。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::ControlRequest;
use singularity_agent::agent::TurnInbox;
use singularity_agent::agent::{Agent, AgentConfig, AgentError, AgentEvent, AgentTerminalReason};
use singularity_agent::compaction::CompactionConfig;
use singularity_agent::prompts::assemble_system_prompt;
use singularity_agent::session::{
    ExpectedSession, LedgerRecord, OperationKind, SessionAccess, SessionError, SessionManager,
    SessionWriter, WriterLockCoordinator, lock_writer, turn_usage_from_model_usage,
};
use singularity_agent::tools::ToolRegistrySnapshot;
use singularity_core::load_agent_instructions;
use singularity_model::{ModelConfigOwner, ModelConfigurationSnapshot, Provider};
use singularity_protocol::ControlDisposition;
use uuid::Uuid;

use crate::assistant_items::AssistantItemEvents;
use crate::conversation::CancelWindow;
use crate::error::{TurnFailureCause, TurnFailureStage, TurnRunError, provider_turn_cause};
use singularity_protocol::{
    DiagnosticSeverity, Thread, Turn, TurnErrorDetail, TurnEvent, TurnModelUsage, TurnStatus,
    diagnostic_code,
};

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

/// 一次收敛到可信终态的 turn 结果（completed/failed/interrupted 都是可信
/// 终态；不存在可信终态的情形由 TurnRunError 表达）。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn_id: String,
    pub turn_status: TurnStatus,
    pub truncated: bool,
    pub usage: TurnModelUsage,
    /// 本轮是否接受过用户停止。它是与终态并列的独立事实：真实失败可以与
    /// 已接受的停止同时存在，链条是否继续消费队列只消费这一项。
    pub user_stopped: bool,
    /// 失败终态的协议错误细节（stage/cause/message 与已发布的 turn/error
    /// 事件同源）；非失败终态为 None。客户端据此报告进程结果，
    /// 不再从事件流重建终态事实。
    pub error: Option<TurnErrorDetail>,
}

/// 内部交接在成功与失败两种情况下都保持控制身份。
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
    /// 磁盘模型配置的唯一访问入口，与工作台共享同一实例；每次使用都
    /// 从它捕获本次操作的局部快照，不长期缓存配置。
    models: Arc<Mutex<ModelConfigOwner>>,
    /// 进程内写者协调器：本进程的所有会话打开路径共用它维持单写者。
    /// 跨进程的数据目录独占由 CLI 数据目录层的锁负责，与此协调器无关。
    coordinator: Arc<WriterLockCoordinator>,
    /// provider 网络执行环境：由装配入口显式注入，配置对象不携带它。
    runtime_handle: tokio::runtime::Handle,
    #[cfg(any(test, feature = "test-support"))]
    provider_override: Option<Arc<dyn Provider + Send + Sync>>,
}

impl TurnRunner {
    /// 执行器只接收自己的依赖：会话目录、共享配置入口、与 ThreadCatalog
    /// 共用的同一个进程内写者协调器，以及 provider 执行环境句柄。
    pub fn new(
        sessions_dir: PathBuf,
        models: Arc<Mutex<ModelConfigOwner>>,
        coordinator: Arc<WriterLockCoordinator>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            sessions_dir,
            models,
            coordinator,
            runtime_handle,
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

    /// 校验模型 selector 能被当前磁盘配置解析为具体 provider 配置。
    /// 这是执行前的内部准备检查；宿主侧的只读查询直接用模型配置快照。
    pub(crate) fn validate_model_selector(&self, selector: Option<&str>) -> Result<(), String> {
        self.lock_models()
            .snapshot()
            .validate_selector(selector)
            .map_err(|error| format!("invalid model selector: {error}"))
    }

    /// 打开本轮唯一会话写者（含崩溃修复并返回 SessionWriter）。
    /// workspace 检查先行：任何失败都不打开会话、不留 operation 痕迹。
    /// 调用方（协调器）在 turn 开始前持有写者，使它成为本会话在本进程内的
    /// 唯一写者，并承载随后的 operation 与终态落盘；控制队列不落盘。
    pub(crate) fn open_turn_writer(&self, thread: &Thread) -> Result<SessionWriter, TurnRunError> {
        validate_workspace(thread).map_err(|message| TurnRunError::Preparation {
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

    /// 在 turn 之外压缩既有 Thread：以独立 compaction operation 落盘
    /// （operation_started/operation_finished，无 turn 绑定）。
    ///
    /// `window` 与普通 turn 共用同一停止接受窗口：压缩的提交边界冻结「是否
    /// 接受过停止」，边界之后的 stop 不再被接受；边界之前接受的停止进入终态
    /// 裁决，但不改写真实的失败原因。
    pub(crate) fn compact_thread(
        &self,
        thread: &Thread,
        window: &CancelWindow,
        writer: SessionWriter,
    ) -> Result<singularity_agent::compaction::CompactionOutcome, CompactionRunError> {
        validate_workspace(thread).map_err(|message| {
            CompactionRunError::Preparation(TurnRunError::Preparation {
                cause: TurnFailureCause::Workspace,
                message,
            })
        })?;
        let registry = ToolRegistrySnapshot::default();
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
        let outcome = agent.compact_now(&mut |_| {}, &window.cancellation);
        // 提交边界：先冻结停止接受事实，再落盘终态。已接受过停止的压缩与
        // 普通 turn 一样收敛为 Interrupted；取消在 Agent 层已归约为 Aborted
        // （provider 的 Cancelled 类型不会到达这里），其余失败一律 Failed。
        let user_stopped = window.freeze();
        let terminal_status = match &outcome {
            Ok(_) if user_stopped => TurnStatus::Interrupted,
            Ok(_) => TurnStatus::Completed,
            Err(AgentError::Aborted) => TurnStatus::Interrupted,
            Err(_) => TurnStatus::Failed,
        };
        let (usage, usage_complete) = agent.request_usage();
        // 独立压缩的失败原因随同一份 operation 终态落盘：进程重启后仍能定位
        // 这次压缩为什么失败，而不是只看到一次 provider 请求与无原因 Failed。
        let error = outcome
            .as_ref()
            .err()
            .filter(|_| terminal_status == TurnStatus::Failed)
            .map(|error| TurnErrorDetail {
                stage: TurnFailureStage::AgentLoop,
                cause: turn_failure_cause(error),
                message: error.to_string(),
            });
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationFinished {
                operation_id,
                turn_id: None,
                outcome: terminal_status,
                usage: Some(singularity_agent::session::turn_usage_from_model_usage(
                    usage,
                    usage_complete,
                )),
                // 独立压缩不绑定 turn，但它的失败原因同样属于这次 operation。
                error,
                truncated: false,
                user_stopped,
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
    ///
    /// `input` 沿用队列的既有表示，正文只在控制请求内保存一次；无论在哪一步
    /// 失败，`undelivered` 都完整交回本次尚未消费的已接受输入。
    pub(crate) fn run(
        &self,
        input: ControlRequest,
        thread: &Thread,
        controls: &crate::conversation::TurnControls,
        sink: &mut dyn FnMut(TurnEvent),
    ) -> TurnRunResult {
        let started = match self.start_turn(thread, controls) {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut undelivered = controls.finish_inbox();
                controls.finish_cancel();
                undelivered.insert(0, input.unbound());
                return TurnRunResult {
                    result: Err(error),
                    undelivered,
                };
            }
        };
        let StartedTurn {
            mut agent,
            operation_id,
        } = started;
        let turn_id = controls.turn_id.clone();
        let writer = controls.writer();
        // 本条输入现在开始自己的 turn：控制身份在此与那个 turn 关联。
        sink(TurnEvent::ControlChanged {
            control: input
                .bound_to(&turn_id)
                .snapshot(ControlDisposition::StartedAsNewTurn),
        });
        let turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: thread.thread_id.clone(),
            status: TurnStatus::Running,
            usage: None,
        };
        sink(TurnEvent::TurnStarted {
            turn,
            started_at: singularity_core::now_iso(),
        });

        let mut item_events = AssistantItemEvents::new(thread.thread_id.clone(), turn_id.clone());
        let mut input_saved = false;
        let run_result = {
            let mut on_event = |event: AgentEvent| match event {
                event @ AgentEvent::UserMessage { .. } => {
                    input_saved = true;
                    item_events.project(sink, event);
                }
                AgentEvent::ControlChanged(control) => {
                    sink(TurnEvent::ControlChanged { control });
                }
                event => item_events.project(sink, event),
            };
            agent.run(&input.text, &mut on_event, controls.cancellation())
        };
        // 只关闭并排空一次；下列每个退出路径都交回这批控制请求本身。
        let mut undelivered = controls.finish_inbox();
        if !input_saved {
            undelivered.insert(0, input.unbound());
        }
        let cancel_accepted = controls.finish_cancel();
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
            // 执行期的存储/宿主故障不能伪装成普通可信 Failed：不写终态记录，
            // 未闭合的 operation 留给下一次显式打开时的既有修复补未知结果，
            // 未执行输入照常交回，链条就此停止。
            Err(error) if stops_chain(&error) => {
                return TurnRunResult {
                    result: Err(fail_stop_execution(
                        &thread.thread_id,
                        &turn_id,
                        &error,
                        sink,
                    )),
                    undelivered,
                };
            }
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
        let usage = turn_usage_from_model_usage(usage, usage_complete);
        let result = (|| {
            // 已接受的停止同样取消本轮未交付的输入：处置由「是否接受过停止」
            // 决定，不从终态枚举重新推断（真实失败与停止可以同时存在）。
            if cancel_accepted {
                for request in &undelivered {
                    sink(TurnEvent::ControlChanged {
                        control: request.snapshot(ControlDisposition::Cancelled),
                    });
                }
            }
            let record = LedgerRecord::OperationFinished {
                operation_id: operation_id.clone(),
                turn_id: Some(turn_id.clone()),
                outcome: turn_status,
                usage: Some(usage.clone()),
                // 失败终态的结构化原因随同一份持久记录落盘：它是这个 turn 失败
                // 原因的长期来源，历史重读不再依赖最近一次 runtime 文本。
                error: error.clone(),
                truncated,
                user_stopped: cancel_accepted,
            };
            if let Err(storage_error) = lock_writer(&writer).append_record(record) {
                return Err(fail_stop_terminalization(
                    &thread.thread_id,
                    &turn_id,
                    error.as_ref(),
                    storage_error.to_string(),
                    sink,
                ));
            }
            item_events.finish_open_items(sink, error.is_some());
            if let Some(error) = &error {
                sink(TurnEvent::TurnFailed {
                    thread_id: thread.thread_id.clone(),
                    turn_id: turn_id.clone(),
                    error: error.clone(),
                });
            } else {
                sink(TurnEvent::TurnCompleted {
                    turn: Turn {
                        turn_id: turn_id.clone(),
                        thread_id: thread.thread_id.clone(),
                        status: turn_status,
                        usage: Some(usage.clone()),
                    },
                });
            }
            Ok(TurnOutcome {
                turn_id,
                turn_status,
                truncated,
                usage,
                user_stopped: cancel_accepted,
                error,
            })
        })();
        TurnRunResult {
            result,
            undelivered,
        }
    }

    fn start_turn(
        &self,
        thread: &Thread,
        controls: &crate::conversation::TurnControls,
    ) -> Result<StartedTurn, TurnRunError> {
        // 会话写者由协调器在 turn 开始前打开（含 workspace 检查与崩溃修复）；
        // 这里只做剩余 fail-fast 准备（provider/config/项目指令），全部就绪
        // 后才写任何 operation 状态。
        let writer = controls.writer();
        let registry = ToolRegistrySnapshot::default();
        let (provider, config, model) = self.resolve_agent_runtime(thread, &registry)?;
        // 冻结事实先于任何事件落盘：公开快照据此报告本轮有效上下文窗口。
        controls.record_context_window(model.context_window());
        // OperationStarted 记录 operation/turn 身份。Agent 单独持久化
        // 输入消息；这些追加不是原子事务。
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
                None => {
                    // 局部快照在本次准备内冻结；配置锁在构造 provider 前释放。
                    let snapshot = self.lock_models().snapshot();
                    Arc::new(
                        singularity_model::OpenAiProvider::from_snapshot(
                            &snapshot,
                            thread.model.as_deref(),
                            self.runtime_handle.clone(),
                        )
                        .map_err(|error| TurnRunError::Preparation {
                            cause: TurnFailureCause::Internal,
                            message: error.to_string(),
                        })?,
                    )
                }
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

    /// 共享配置入口的互斥锁；中毒即 fail-stop。
    fn lock_models(&self) -> std::sync::MutexGuard<'_, ModelConfigOwner> {
        match self.models.lock() {
            Ok(models) => models,
            Err(_) => panic!("model configuration lock poisoned (fail-stop)"),
        }
    }

    fn open_and_repair_session(&self, thread: &Thread) -> Result<SessionManager, SessionError> {
        let path = crate::store::thread_session_path(&self.sessions_dir, &thread.thread_id);
        SessionManager::open_existing_with_access(
            &path,
            &self.coordinator,
            ExpectedSession {
                id: &thread.thread_id,
                cwd: None,
            },
            SessionAccess::RepairWrite,
        )
    }
}

fn turn_failure_cause(error: &AgentError) -> TurnFailureCause {
    match error {
        AgentError::Provider(error) => provider_turn_cause(error.kind),
        AgentError::Session(_) => TurnFailureCause::Store,
        // 文件指令与技能正文都是指令材料：同一真实来源只映射一次，不按发生
        // 阶段改写类别。
        AgentError::Instructions(_) | AgentError::SkillLoad(_) => {
            TurnFailureCause::ProjectInstructions
        }
        AgentError::ContextCapacity(_) => TurnFailureCause::ContextCapacity,
        AgentError::Aborted | AgentError::InvalidSummary(_) | AgentError::HostFailure(_) => {
            TurnFailureCause::Internal
        }
    }
}

/// 执行期不允许再写可信终态的故障：存储写入失败与程序故障属于同一类宿主
/// 故障出口；provider/工具/协议失败仍走可信 Failed 终态并继续消费队列。
fn stops_chain(error: &AgentError) -> bool {
    matches!(error, AgentError::Session(_) | AgentError::HostFailure(_))
}

/// 校验 thread 的工作目录仍可用（存在且可规范化）；只是校验，
/// 不返回另一个路径值，调用方需要的是通过与否。
fn validate_workspace(thread: &Thread) -> Result<(), String> {
    singularity_core::canonicalize_workspace(&thread.cwd).map(|_| ())
}

/// 准备固定提示词及首次文件指令，读取失败在 operation 开始前报告。
fn agent_config_for_thread(
    thread: &Thread,
    registry: &ToolRegistrySnapshot,
    instruction_home: &std::path::Path,
) -> Result<AgentConfig, TurnRunError> {
    let cwd = &thread.cwd;
    let initial_instructions = load_agent_instructions(std::path::Path::new(cwd), instruction_home)
        .map_err(|message| TurnRunError::Preparation {
            cause: TurnFailureCause::ProjectInstructions,
            message,
        })?;
    let assembled = assemble_system_prompt(cwd, registry);
    Ok(AgentConfig {
        system_prompt: assembled,
        instruction_home: Some(instruction_home.to_path_buf()),
        initial_instructions,
        compaction: CompactionConfig::default(),
    })
}

/// 终态无法落盘时的 fail-stop 出口：发 storage_fatal 诊断，不发布任何
/// 终态事件；客户端不会把未确认写入的结果当作完成。已经发生的执行失败
/// 随同一份错误一起报告，不被收尾故障覆盖。
fn fail_stop_terminalization(
    thread_id: &str,
    turn_id: &str,
    execution: Option<&TurnErrorDetail>,
    storage_error: String,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    publish_fatal(
        thread_id,
        turn_id,
        diagnostic_code::STORAGE_FATAL,
        &storage_error,
        sink,
    );
    TurnRunError::Terminalization {
        execution: execution.cloned(),
        storage: Some(storage_error),
    }
}

/// 执行期存储/宿主故障的 fail-stop 出口：不写终态记录、不发布终态事件，
/// 未闭合的 operation 由下一次显式打开时的既有修复补未知结果。
fn fail_stop_execution(
    thread_id: &str,
    turn_id: &str,
    error: &AgentError,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    let detail = TurnErrorDetail {
        stage: TurnFailureStage::AgentLoop,
        cause: turn_failure_cause(error),
        message: error.to_string(),
    };
    let code = match error {
        AgentError::Session(_) => diagnostic_code::STORAGE_FATAL,
        _ => diagnostic_code::HOST_FATAL,
    };
    publish_fatal(thread_id, turn_id, code, &detail.message, sink);
    TurnRunError::Terminalization {
        execution: Some(detail),
        storage: None,
    }
}

fn publish_fatal(
    thread_id: &str,
    turn_id: &str,
    code: &str,
    message: &str,
    sink: &mut dyn FnMut(TurnEvent),
) {
    sink(TurnEvent::Diagnostic {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        severity: DiagnosticSeverity::Error,
        code: code.to_string(),
        message: message.to_string(),
    });
}

#[cfg(test)]
mod tests {
    /// 每个真实来源只映射一次：阶段不同不改变类别，容量问题不再是 Internal。
    #[test]
    fn failure_causes_keep_their_real_source() {
        use super::*;
        use singularity_model::ProviderError;

        let cases = [
            (
                AgentError::Instructions("AGENTS.md is unreadable".into()),
                TurnFailureCause::ProjectInstructions,
            ),
            (
                AgentError::SkillLoad("review.md disappeared".into()),
                TurnFailureCause::ProjectInstructions,
            ),
            (
                AgentError::ContextCapacity("no room for a response".into()),
                TurnFailureCause::ContextCapacity,
            ),
            (
                AgentError::Provider(ProviderError::new(
                    singularity_model::ModelErrorKind::AuthError,
                    "credentials rejected",
                )),
                TurnFailureCause::ProviderAuth,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(turn_failure_cause(&error), expected, "{error:?}");
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn failures_around_start_and_terminal_return_unconsumed_control_identity() {
        use super::*;
        use crate::conversation::TurnControls;
        use singularity_agent::agent::control_id;
        use singularity_agent::session::reduce_operations;
        use singularity_model::test_support::ScriptedProvider;
        use singularity_protocol::ControlChannel;

        for boundary in ["before_start", "after_start", "before_terminal"] {
            let fixture = crate::test_support::SessionsFixture::new();
            let provider = Arc::new(ScriptedProvider::ok("done"));
            let runner = fixture.runner(Some(provider.clone()));
            let thread = fixture
                .catalog()
                .create_thread(fixture.home().to_str().unwrap(), None)
                .unwrap();
            let writer = runner.open_turn_writer(&thread).unwrap();
            let path = lock_writer(&writer).path().to_path_buf();
            let request = ControlRequest {
                control_id: "queued-control".into(),
                turn_id: None,
                channel: ControlChannel::FollowUp,
                sequence: 0,
                text: "queued input".into(),
            };
            let controls =
                TurnControls::new("active-turn", TurnInbox::default_handle(), writer.clone());
            // 控制身份与接受序号由 Conversation 生成；本测试直接把等价请求放入
            // 注入箱，钉住它在失败路径上的归还。
            let steer = ControlRequest {
                control_id: control_id(ControlChannel::Steer, 1),
                turn_id: Some("active-turn".into()),
                channel: ControlChannel::Steer,
                sequence: 1,
                text: "unconsumed steer".into(),
            };
            assert!(controls.enqueue(steer.clone()));
            let input = request.clone();
            let mut events = Vec::new();
            let mut saved = Vec::new();
            if boundary == "before_start" {
                saved = std::fs::read(&path).unwrap();
                std::fs::remove_file(&path).unwrap();
            }
            let run = runner.run(input, &thread, &controls, &mut |event| {
                if (boundary == "after_start" && matches!(event, TurnEvent::ControlChanged { .. }))
                    || (boundary == "before_terminal"
                        && matches!(event, TurnEvent::TurnStarted { .. }))
                {
                    saved = std::fs::read(&path).unwrap();
                    std::fs::remove_file(&path).unwrap();
                }
                events.push(event);
            });
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
                    matches!(run.result, Err(TurnRunError::Terminalization { execution: Some(error), .. }) if error.cause == TurnFailureCause::Store)
                );
            }
            assert!(!events.iter().any(|event| matches!(
                event,
                TurnEvent::TurnCompleted { .. } | TurnEvent::TurnFailed { .. }
            )));
            if boundary == "before_terminal" {
                assert!(
                    events.iter().any(|event| matches!(
                        event,
                        TurnEvent::Diagnostic { code, severity, .. }
                            if code == diagnostic_code::STORAGE_FATAL
                                && *severity == DiagnosticSeverity::Error
                    )),
                    "terminal store failure must publish storage_fatal: {events:?}"
                );
            }
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
                2,
                "the unconsumed current input and the steering input are both returned"
            );
            // 准备阶段失败也归还当前输入，上层不再按错误阶段补回。
            assert_eq!(run.undelivered[0], request);
            assert!(
                !controls.enqueue(ControlRequest {
                    control_id: control_id(ControlChannel::Steer, 2),
                    turn_id: Some("active-turn".into()),
                    channel: ControlChannel::Steer,
                    sequence: 2,
                    text: "late input".into(),
                }),
                "the injection window is closed once the turn has ended"
            );
            drop(controls);
            drop(writer);
            std::fs::write(&path, saved).unwrap();
            let reopened = SessionManager::open_existing(&path).unwrap();
            let operation = reduce_operations(reopened.entries()).unwrap();
            assert_eq!(operation.is_some(), boundary != "before_start");
            // 常规修复关闭被中断的 operation，不执行输入/工具。
            drop(reopened);
            let repaired = SessionManager::open_existing_with_access(
                &path,
                &fixture.coordinator,
                ExpectedSession {
                    id: path.file_stem().unwrap().to_str().unwrap(),
                    cwd: None,
                },
                SessionAccess::RepairWrite,
            )
            .unwrap();
            assert!(reduce_operations(repaired.entries()).unwrap().is_none());
        }
    }
}
