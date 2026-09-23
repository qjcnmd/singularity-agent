//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影和终态落盘。
//!
//! 执行不变量：
//! - 准备阶段的失败，与 operation_started 成功之后的提交失败，分开归类；
//! - 本 turn 的 operation_started 先于一切事件落盘；终态记录
//!   （operation_finished，状态与错误事实）先于终态事件；
//! - 一个 turn 只打开一次会话文件，同一个 SessionManager 贯穿全程；
//! - 投影是尽力而为的观察侧信道：投影失败只丢掉这次投影，不影响执行事实。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use singularity_agent::agent::ControlRequest;
use singularity_agent::agent::TurnInbox;
use singularity_agent::agent::{Agent, AgentConfig, AgentError, AgentEvent, AgentTerminalReason};
use singularity_agent::prompts::assemble_developer_instructions;
use singularity_agent::session::{
    ExpectedSession, LedgerRecord, OperationKind, SessionAccess, SessionError, SessionManager,
    SessionWriter, WriterLockCoordinator, lock_writer, turn_usage_from_model_usage,
};
use singularity_agent::tools::ToolRegistrySnapshot;
use singularity_core::load_agent_instructions;
use singularity_model::{ModelConfigManager, ModelConfigurationSnapshot, Provider};
use singularity_protocol::ControlDisposition;
use uuid::Uuid;

use crate::assistant_items::AssistantItemEvents;
use crate::conversation::CancelWindow;
use crate::error::{TurnFailureCause, TurnRunError, provider_turn_cause};
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
    #[error("compaction terminalization failed: {0}")]
    Terminalization(#[source] SessionError),
}

/// 已经落盘的独立压缩结果；失败和中断同样是可信终态。
#[derive(Debug)]
pub struct CompactionOutcome {
    pub status: TurnStatus,
    pub reduced: bool,
    pub error: Option<TurnErrorDetail>,
}

impl CompactionOutcome {
    /// 摘要已经落盘时由历史正文给出反馈，其余结果投影成压缩终态。
    pub fn terminal(self) -> Option<singularity_protocol::SessionTerminalSnapshot> {
        let visible = match self.status {
            TurnStatus::Failed | TurnStatus::Interrupted => true,
            TurnStatus::Completed => !self.reduced,
            TurnStatus::Running => false,
        };
        visible.then(|| singularity_protocol::SessionTerminalSnapshot {
            source: singularity_protocol::SessionTerminalSource::Compaction,
            status: self.status,
            message: self.error.map(|error| error.message),
        })
    }
}

/// 一次收敛到可信终态的 turn 结果（completed/failed/interrupted 都是可信终态；
/// 没有可信终态的情形由 TurnRunError 表达）。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn_id: String,
    pub turn_status: TurnStatus,
    pub truncated: bool,
    pub usage: TurnModelUsage,
    /// 本轮是否接受过用户停止，它和终态并列：真实失败可以和已接受的停止同时存在。
    /// 队列推进用的是内部交接的 `TurnRunResult::cancel_accepted`，返回错误的路径也一样。
    pub user_stopped: bool,
    /// 失败终态的协议错误细节（其中的 cause/message 与已发布的 turn/error 事件同源）；
    /// 非失败终态是 None。客户端用它报告进程结果，不必再从事件流里重建终态事实。
    pub error: Option<TurnErrorDetail>,
}

/// 内部交接用的结果：无论成功还是失败，都保持控制身份。
pub(crate) struct TurnRunResult {
    pub result: Result<TurnOutcome, TurnRunError>,
    pub undelivered: Vec<ControlRequest>,
    /// 本轮冻结下来的「是否接受过停止」。它和 `TurnOutcome::user_stopped` 同源，
    /// 但失败出口没有 `TurnOutcome`：未送达输入怎么处置必须由这条事实决定，调用方
    /// 不能从错误类型去反推用户是否停止过。
    pub cancel_accepted: bool,
}

struct StartedTurn {
    agent: Agent,
    operation_id: String,
}

/// 进程内的 turn 执行器：本身不保存状态，可以共享，按需构造。
pub struct TurnRunner {
    sessions_dir: PathBuf,
    /// 磁盘模型配置的唯一访问入口，和工作台共享同一个实例；每次使用都从它取一份
    /// 本次操作的局部快照，不长期缓存配置。
    models: Arc<Mutex<ModelConfigManager>>,
    /// 进程内的写者协调器：本进程所有会话打开路径共用它来维持单写者。跨进程独占
    /// 数据目录由 CLI 数据目录层的锁负责，和这个协调器无关。
    coordinator: Arc<WriterLockCoordinator>,
    /// provider 的网络执行环境：由装配入口显式注入，配置对象本身不带它。
    runtime_handle: tokio::runtime::Handle,
    #[cfg(any(test, feature = "test-support"))]
    provider_override: Option<Arc<dyn Provider + Send + Sync>>,
    /// 测试注入点：独立压缩在 Agent 返回之后、冻结提交边界之前调用一次，用来确定性地
    /// 构造「Agent 已经成功、停止还没冻结」这个窗口。
    #[cfg(any(test, feature = "test-support"))]
    compaction_commit_pause: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl TurnRunner {
    pub fn new(
        sessions_dir: PathBuf,
        models: Arc<Mutex<ModelConfigManager>>,
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
            #[cfg(any(test, feature = "test-support"))]
            compaction_commit_pause: Mutex::new(None),
        }
    }

    /// 测试注入：用一个固定的 provider 取代快照解析的结果。
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_provider_override(mut self, provider: Arc<dyn Provider + Send + Sync>) -> Self {
        self.provider_override = Some(provider);
        self
    }

    /// 测试注入：让下一次独立压缩在 Agent 返回之后、冻结提交边界之前停住，由回调
    /// 确定性地构造「已接受停止」的时序。
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::expect_used)]
    pub fn pause_next_compaction_commit(&self, pause: Arc<dyn Fn() + Send + Sync>) {
        *self
            .compaction_commit_pause
            .lock()
            .expect("compaction commit pause lock poisoned") = Some(pause);
    }

    /// 校验模型 selector 能被当前磁盘配置解析成具体的 provider 配置。
    /// 这是执行前的内部准备检查；宿主侧的只读查询直接用模型配置快照。
    pub(crate) fn validate_model_selector(&self, selector: &str) -> Result<(), String> {
        self.lock_models()
            .snapshot()
            .validate_selector(Some(selector))
            .map_err(|error| format!("invalid model selector: {error}"))
    }

    /// 打开本轮唯一的会话写者（包含崩溃修复）。workspace 检查放在最前面：任何失败都不会
    /// 打开会话，也不留 operation 痕迹。调用方（协调器）在 turn 开始前就持有这个写者，使它
    /// 成为本会话在本进程内的唯一写者，并承担随后的 operation 与终态落盘；控制队列不落盘。
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

    /// 在 turn 之外压缩已有的 Thread：以独立的 compaction operation 落盘
    /// （operation_started/operation_finished，不绑定 turn）。`window` 和普通 turn 共用同一个
    /// 停止接受窗口，边界之前接受的停止进入终态裁决，但不会改写真实的失败原因。
    pub(crate) fn compact_thread(
        &self,
        thread: &Thread,
        window: &CancelWindow,
        writer: SessionWriter,
    ) -> Result<CompactionOutcome, CompactionRunError> {
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
        // 测试注入点：只生效一次，取值时就把它取走。
        #[cfg(any(test, feature = "test-support"))]
        #[allow(clippy::expect_used)]
        if let Some(pause) = self
            .compaction_commit_pause
            .lock()
            .expect("compaction commit pause lock poisoned")
            .take()
        {
            pause();
        }
        // 提交边界：先冻结「是否接受过停止」，再落盘终态。已经接受过停止的压缩和
        // 普通 turn 一样收敛为 Interrupted；取消在 Agent 层已经归约成 Aborted
        // （provider 的 Cancelled 类型到不了这里），其余失败一律 Failed。
        let user_stopped = window.freeze();
        let terminal_status = match &outcome {
            Ok(_) if user_stopped => TurnStatus::Interrupted,
            Ok(_) => TurnStatus::Completed,
            Err(AgentError::Aborted) => TurnStatus::Interrupted,
            Err(_) => TurnStatus::Failed,
        };
        // 独立压缩的失败原因随同一份 operation 终态一起落盘：进程重启后仍能查到这次
        // 压缩为什么失败，而不是只看到一次 provider 请求和一个没有原因的 Failed。
        let error = outcome
            .as_ref()
            .err()
            .filter(|_| terminal_status == TurnStatus::Failed)
            .map(turn_error_detail);
        lock_writer(&writer)
            .append_record(LedgerRecord::OperationFinished {
                operation_id,
                turn_id: None,
                outcome: terminal_status,
                error: error.clone(),
                user_stopped,
            })
            .map_err(CompactionRunError::Terminalization)?;
        Ok(CompactionOutcome {
            status: terminal_status,
            reduced: matches!(
                outcome,
                Ok(singularity_agent::compaction::CompactionOutcome::Reduced)
            ),
            error,
        })
    }

    /// 执行一个 turn，直到终态收敛。调用方持有 crate::conversation::TurnControls，以便在执行
    /// 期间注入输入或取消。
    ///
    /// 返回 Ok 时终态（completed/failed/interrupted）已经落盘、终态事件也已经发出——失败终态的
    /// TurnOutcome::error 带着与 turn/error 事件同源的协议错误细节。返回
    /// TurnRunError::Terminalization 时终态记录写不下去，不会发出任何虚假的终态事件。
    ///
    /// `input` 沿用队列里已有的表示，正文只在控制请求里保存一次；无论在哪一步失败，
    /// `undelivered` 都会完整交回本次还没消费的已接受输入。
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
                // 启动失败同样要冻结停止事实：未送达输入的处置和执行期失败共用同一条规则。
                let cancel_accepted = controls.finish_cancel();
                undelivered.insert(0, input.unbound());
                if cancel_accepted {
                    cancel_undelivered(&undelivered, sink);
                }
                return TurnRunResult {
                    result: Err(error),
                    undelivered,
                    cancel_accepted,
                };
            }
        };
        let StartedTurn {
            mut agent,
            operation_id,
        } = started;
        let turn_id = controls.turn_id.clone();
        let writer = controls.writer();
        // 这条输入现在开始自己那一轮：控制身份在这里和这个 turn 关联。
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
        // 只关闭并排空一次；下面每个退出路径都交回这批控制请求本身。
        let mut undelivered = controls.finish_inbox();
        // 本轮输入未被 Agent 落盘：仍算未送达，随队列交回。
        if !input_saved {
            undelivered.insert(0, input.unbound());
        }
        let cancel_accepted = controls.finish_cancel();
        let failure_code = run_result
            .as_ref()
            .err()
            .and_then(|error| classify_agent_error(error).1);
        let (turn_status, truncated, error) = match (run_result, failure_code) {
            (Ok(outcome), _) => (
                match outcome.terminal_reason {
                    // 停止后 Agent 仍可能正常收尾：终态按停止记为中断。
                    AgentTerminalReason::Completed if cancel_accepted => TurnStatus::Interrupted,
                    AgentTerminalReason::Completed => TurnStatus::Completed,
                    AgentTerminalReason::Aborted => TurnStatus::Interrupted,
                },
                outcome.truncated,
                None,
            ),
            // 执行期的存储/宿主故障不能伪装成普通的可信 Failed：不写终态记录，未闭合的
            // operation 留给下一次显式打开时的既有修复去补「结果未知」，未执行的输入照常
            // 交回，链条到此停止。
            (Err(error), Some(code)) => {
                // 致命失败不能吞掉已经接受的停止：未送达输入怎么处置仍由冻结事实决定，
                // 处置事件与正常终态路径保持一致。
                if cancel_accepted {
                    cancel_undelivered(&undelivered, sink);
                }
                return TurnRunResult {
                    result: Err(fail_stop_execution(
                        &thread.thread_id,
                        &turn_id,
                        &error,
                        code,
                        sink,
                    )),
                    undelivered,
                    cancel_accepted,
                };
            }
            (Err(error), None) => (TurnStatus::Failed, false, Some(turn_error_detail(&error))),
        };
        let (usage, usage_complete) = agent.request_usage();
        // 所有执行结果共用同一套顺序：取消控制、终态落盘、闭合 item；
        // 任何一次存储失败都 fail-stop，不发布虚假终态。
        let usage = turn_usage_from_model_usage(usage, usage_complete);
        let result = (|| {
            // 已经接受的停止同样要取消本轮未交付的输入：处置由「是否接受过停止」决定，
            // 不再从终态枚举里重新推断（真实失败和停止可以同时存在）。
            if cancel_accepted {
                cancel_undelivered(&undelivered, sink);
            }
            let record = LedgerRecord::OperationFinished {
                operation_id: operation_id.clone(),
                turn_id: Some(turn_id.clone()),
                outcome: turn_status,
                // 失败终态的结构化原因随同一份持久记录落盘：它是这个 turn 失败原因的
                // 长期来源，重读历史时不再依赖 runtime 最近一次的文本。
                error: error.clone(),
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
            cancel_accepted,
        }
    }

    fn start_turn(
        &self,
        thread: &Thread,
        controls: &crate::conversation::TurnControls,
    ) -> Result<StartedTurn, TurnRunError> {
        // 会话写者由协调器在 turn 开始前打开（含 workspace 检查和崩溃修复）；这里只做剩下的
        // fail-fast 准备（provider/config/项目指令），全部就绪之后才写任何 operation 状态。
        let writer = controls.writer();
        let registry = ToolRegistrySnapshot::default();
        let (provider, config, model) = self.resolve_agent_runtime(thread, &registry)?;
        // 冻结事实先于任何事件落盘：公开快照用它报告本轮的有效上下文窗口。
        controls.record_context_window(model.context_window());
        // OperationStarted 记录 operation/turn 身份。输入消息由 Agent 单独落盘；
        // 这些追加不是一个原子事务。
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

    /// 解析 Provider、AgentConfig 和本 turn 冻结的模型配置快照，并预先校验 compaction；
    /// 任何一项失败就直接失败，不留 operation 痕迹。
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
                    // 局部快照在本次准备内冻结；配置锁在构造 provider 之前释放。
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

    /// 共享配置入口的互斥锁；中毒就 fail-stop。
    fn lock_models(&self) -> std::sync::MutexGuard<'_, ModelConfigManager> {
        match self.models.lock() {
            Ok(models) => models,
            Err(_) => panic!("model configuration lock poisoned (fail-stop)"),
        }
    }

    fn open_and_repair_session(&self, thread: &Thread) -> Result<SessionManager, SessionError> {
        let path =
            crate::thread_catalog::thread_session_path(&self.sessions_dir, &thread.thread_id);
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

/// 同一个穷尽的分类既决定终态原因，也决定是否必须停止执行链。
/// 存储与宿主故障写不出可信终态，因此返回对应的致命诊断码。
fn classify_agent_error(error: &AgentError) -> (TurnFailureCause, Option<&'static str>) {
    match error {
        AgentError::Provider(error) => (provider_turn_cause(error.kind), None),
        AgentError::Session(_) => (
            TurnFailureCause::Store,
            Some(diagnostic_code::STORAGE_FATAL),
        ),
        AgentError::HostFailure(_) => (
            TurnFailureCause::Internal,
            Some(diagnostic_code::HOST_FATAL),
        ),
        AgentError::Instructions(_) | AgentError::SkillLoad(_) => {
            (TurnFailureCause::ProjectInstructions, None)
        }
        AgentError::Aborted | AgentError::InvalidSummary(_) => (TurnFailureCause::Internal, None),
    }
}

fn turn_error_detail(error: &AgentError) -> TurnErrorDetail {
    TurnErrorDetail {
        cause: classify_agent_error(error).0,
        message: error.to_string(),
    }
}

/// 本轮接受过停止时，未送达的输入不再进入下一轮：在事件流里和正常终态路径一样标记为
/// 已取消；启动失败、执行期致命失败和终态落盘失败共用这条处置规则。
fn cancel_undelivered(undelivered: &[ControlRequest], sink: &mut dyn FnMut(TurnEvent)) {
    for request in undelivered {
        sink(TurnEvent::ControlChanged {
            control: request.snapshot(ControlDisposition::Cancelled),
        });
    }
}

/// 校验 thread 的工作目录仍然可用（存在，且能被规范化）；只返回通过与否，不返回另一个路径值。
fn validate_workspace(thread: &Thread) -> Result<(), String> {
    singularity_core::canonicalize_workspace(&thread.cwd).map(|_| ())
}

/// 准备固定提示词和首次文件指令；读取失败会在 operation 开始之前报告。
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
    let assembled = assemble_developer_instructions(cwd, registry);
    Ok(AgentConfig {
        developer_instructions: assembled,
        instruction_home: Some(instruction_home.to_path_buf()),
        initial_instructions,
    })
}

/// 终态写不下去时的 fail-stop 出口：发出 storage_fatal 诊断，不发布任何终态事件，
/// 客户端不会把没确认写入的结果当成完成。已经发生的执行失败随同一份错误一起报告，
/// 不会被收尾故障覆盖。
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

/// 执行期存储/宿主故障的 fail-stop 出口：不写终态记录，也不发布终态事件。
fn fail_stop_execution(
    thread_id: &str,
    turn_id: &str,
    error: &AgentError,
    code: &str,
    sink: &mut dyn FnMut(TurnEvent),
) -> TurnRunError {
    let detail = turn_error_detail(error);
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
