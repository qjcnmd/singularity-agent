//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影和终态落盘。
//! operation_started 先于本 turn 的一切事件落盘，终态记录（operation_finished，状态与
//! 错误事实）先于终态事件；一个 turn 只打开一次会话文件，同一个 SessionManager 贯穿全程。
//! 投影是尽力而为的观察侧信道，投影失败只丢掉这一次投影，不影响执行事实。

mod compaction;
mod error;

use self::error::*;

use std::path::PathBuf;
use std::sync::Arc;

use singularity_agent::agent::ControlRequest;
use singularity_agent::agent::SteeringInbox;
use singularity_agent::agent::{Agent, AgentConfig, AgentError, AgentEvent, AgentTerminalReason};
use singularity_agent::session::{
    LedgerRecord, SessionManager, SessionWriter, append_record_async, lock_writer,
    turn_usage_from_model_usage,
};
use singularity_core::load_agent_instructions;
use singularity_model::{ModelConfigManager, Provider};

use crate::assistant_items::AssistantItemEvents;
use crate::conversation::CancelWindow;
use crate::error::{TurnFailureCause, TurnRunError, provider_turn_cause};
use singularity_protocol::{
    DiagnosticSeverity, Thread, Turn, TurnErrorDetail, TurnEvent, TurnModelUsage, TurnStatus, diagnostic_code,
};

/// 一次收敛到可信终态的 turn 结果。completed/failed/interrupted 都是可信终态，
/// 没有可信终态的情形由 TurnRunError 表达。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn_status: TurnStatus,
    /// 中断终态是否来自本轮已接受的用户停止。
    pub manually_stopped: bool,
    pub truncated: bool,
    pub usage: TurnModelUsage,
    /// 失败终态的协议错误细节，与 turn/error 事件携带的是同一份；非失败终态是 None。
    /// 客户端用它报告进程结果，不必从事件流重建终态事实。
    pub error: Option<TurnErrorDetail>,
}

/// 进程内的 turn 执行器：本身不保存状态，可以共享，按需构造。
pub struct TurnRunner {
    sessions_dir: PathBuf,
    user_questions: bool,
    /// 磁盘模型配置的访问入口，和工作台共享同一个实例；每次使用取一份本次操作的局部
    /// 快照，不长期缓存配置。
    models: Arc<ModelConfigManager>,
    mcp: Arc<singularity_mcp::McpManager>,
}

impl TurnRunner {
    /// 装配执行依赖；目录与配置入口由所有任务共享。
    pub fn new(
        sessions_dir: PathBuf,
        models: Arc<ModelConfigManager>,
        mcp: Arc<singularity_mcp::McpManager>,
    ) -> Self {
        Self {
            sessions_dir,
            user_questions: false,
            models,
            mcp,
        }
    }

    /// 声明宿主能显示问题并提交答案。
    pub fn with_user_questions(mut self) -> Self {
        self.user_questions = true;
        self
    }

    /// 校验模型 selector 能被当前磁盘配置解析成具体的 provider 配置。
    /// 用于用户修改会话模型；执行准备直接解析当轮配置。
    pub(crate) fn validate_model_selector(&self, selector: &str) -> Result<(), String> {
        self.models
            .snapshot()
            .validate_selector(Some(selector))
            .map_err(|error| format!("invalid model selector: {error}"))
    }

    /// 在 Conversation 的写入窗口内打开本轮会话写者，承担随后的 operation 与终态落盘。
    /// 控制队列不落盘。
    pub(crate) fn open_turn_writer(&self, thread: &Thread) -> Result<SessionWriter, TurnRunError> {
        let path = crate::thread_catalog::thread_session_path(&self.sessions_dir, &thread.thread_id);
        let session = SessionManager::open_existing(&path, &thread.thread_id)
            .map_err(|error| TurnRunError::Preparation(error.to_string()))?;
        Ok(Arc::new(std::sync::Mutex::new(session)))
    }

    /// 执行已准备的 turn，直到终态收敛。调用方持有 crate::conversation::TurnControls，
    /// 以便在执行期间注入输入或取消。
    ///
    /// 返回 Ok 时终态（completed/failed/interrupted）已落盘，终态事件也已发出，失败终态的
    /// TurnOutcome::error 与 turn/error 事件携带同一份协议错误细节。返回
    /// TurnRunError::Terminalization 时终态记录写不下去，不会发出终态事件。
    ///
    /// `input` 是本轮已接受的完整输入，之后由会话持久化维护。
    pub(crate) async fn run(
        input: ControlRequest,
        thread: &Thread,
        controls: &Arc<crate::conversation::TurnControls>,
        (mut agent, started_at): (Agent, String),
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, TurnRunError> {
        let turn_id = controls.turn_id.clone();
        let writer = controls.writer();
        let turn = Turn {
            turn_id: turn_id.clone(),
            thread_id: thread.thread_id.clone(),
            status: TurnStatus::Running,
            usage: None,
        };
        sink(TurnEvent::TurnStarted { turn, started_at });

        let mut item_events = AssistantItemEvents::new(thread.thread_id.clone(), turn_id.clone());
        let run_result = {
            let mut on_event = |event: AgentEvent| match event {
                AgentEvent::ControlChanged => {
                    sink(TurnEvent::ControlChanged {});
                }
                event => item_events.project(sink, event),
            };
            agent.run(&input, &mut on_event, controls.cancellation()).await
        };
        controls.close_inbox();
        let cancel_accepted = controls.finish_cancel();
        let (turn_status, truncated, error) = match run_result {
            Ok(outcome) => (
                match outcome.terminal_reason {
                    // 停止后 Agent 仍可能正常收尾：终态按停止记为中断。
                    AgentTerminalReason::Completed if cancel_accepted => TurnStatus::Interrupted,
                    AgentTerminalReason::Completed => TurnStatus::Completed,
                    AgentTerminalReason::Aborted => TurnStatus::Interrupted,
                },
                outcome.truncated,
                None,
            ),
            // 执行期的存储/宿主故障不写可信终态，历史把未闭合的 operation 投影为中断；
            // 链条到此停止。
            Err(error) => {
                let cause = classify_agent_error(&error);
                let detail = TurnErrorDetail { cause, message: error.to_string() };
                if cause == TurnFailureCause::Store {
                    return Err(fail_stop_execution(&thread.thread_id, &turn_id, detail, sink));
                }
                (TurnStatus::Failed, false, Some(detail))
            }
        };
        let usage = agent.request_usage();
        // 所有执行结果共用同一套顺序：冻结取消控制、终态落盘、发布终态；存储失败一律
        // fail-stop，不发布虚假终态。
        let usage = turn_usage_from_model_usage(usage);
        let record = LedgerRecord::OperationFinished {
            turn_id: Some(turn_id.clone()),
            outcome: turn_status,
            // 失败终态的结构化原因随同一份持久记录落盘，重读历史时不依赖 runtime 最近一次的文本。
            error: error.clone(),
            user_stopped: cancel_accepted,
        };
        let finished_at = match append_record_async(&writer, record).await {
            Ok(committed) => committed.timestamp,
            Err(storage_error) => {
                return Err(fail_stop_terminalization(
                    &thread.thread_id,
                    &turn_id,
                    error.as_ref(),
                    storage_error.to_string(),
                    sink,
                ));
            }
        };
        if let Some(error) = &error {
            sink(TurnEvent::TurnFailed {
                thread_id: thread.thread_id.clone(),
                turn_id: turn_id.clone(),
                error: error.clone(),
                finished_at,
            });
        } else {
            sink(TurnEvent::TurnCompleted {
                turn: Turn {
                    turn_id: turn_id.clone(),
                    thread_id: thread.thread_id.clone(),
                    status: turn_status,
                    usage: Some(usage.clone()),
                },
                finished_at,
            });
        }
        Ok(TurnOutcome {
            turn_status,
            manually_stopped: turn_status == TurnStatus::Interrupted && cancel_accepted,
            truncated,
            usage,
            error,
        })
    }

    /// 在会话写入窗口内完成准备和开始记录；成功后才由 Conversation 移交排队输入。
    pub(crate) fn prepare_turn(
        &self,
        thread: &Thread,
        controls: &crate::conversation::TurnControls,
    ) -> Result<(Agent, String), TurnRunError> {
        // 会话写者由 Conversation 在 turn 开始前打开；本函数只做剩下的 fail-fast 准备
        // （provider/config/项目指令），就绪之后才写 operation 状态。
        let writer = controls.writer();
        let (provider, config) = self.resolve_agent_runtime(thread)?;
        // OperationStarted 记录 turn 身份。输入消息由 Agent 单独落盘；
        // 这些追加不是一个原子事务。
        let agent =
            Agent::new(controls.inbox_handle(), provider, config, writer.clone(), Arc::clone(&self.mcp));
        let agent = if self.user_questions {
            agent.with_user_questions(Arc::clone(&controls.questions))
        } else {
            agent
        };
        let mut writer = lock_writer(&writer);
        let committed = writer
            .append_record(LedgerRecord::OperationStarted { turn_id: Some(controls.turn_id.clone()) })
            .map_err(|error| TurnRunError::Preparation(error.to_string()))?;
        Ok((agent, committed.timestamp))
    }

    /// 解析 Provider 与 AgentConfig；模型容量由 Agent 从 Provider 冻结。
    /// 任何一项失败就直接失败，不留 operation 痕迹。
    fn resolve_agent_runtime(
        &self,
        thread: &Thread,
    ) -> Result<(Arc<dyn Provider + Send + Sync>, AgentConfig), TurnRunError> {
        let provider = self.resolve_provider(thread)?;
        let config = agent_config_for_thread(
            thread,
            self.sessions_dir.parent().expect("sessions directory is inside the data directory"),
        )?;
        Ok((provider, config))
    }
    fn resolve_provider(&self, thread: &Thread) -> Result<Arc<dyn Provider + Send + Sync>, TurnRunError> {
        let snapshot = self.models.snapshot();
        Ok(Arc::new(
            singularity_model::OpenAiProvider::from_snapshot(&snapshot, thread.model.as_deref())
                .map_err(|error| TurnRunError::Preparation(error.to_string()))?,
        ))
    }
}

/// 准备首次文件指令；读取失败会在 operation 开始之前报告。
fn agent_config_for_thread(
    thread: &Thread,
    instruction_home: &std::path::Path,
) -> Result<AgentConfig, TurnRunError> {
    let cwd = &thread.cwd;
    let initial_instructions = load_agent_instructions(std::path::Path::new(cwd), instruction_home)
        .map_err(TurnRunError::Preparation)?;
    Ok(AgentConfig {
        instruction_home: instruction_home.to_path_buf(),
        initial_instructions,
    })
}
