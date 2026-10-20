//! 单个 turn 的完整执行管线：准备、会话单写者、Agent 执行、事件投影和终态落盘。
//! operation_started 先于本 turn 的一切事件落盘，终态记录（operation_finished，状态与
//! 错误事实）先于终态事件；会话内全部操作共用同一个 SessionManager。
//! 投影是尽力而为的观察侧信道，投影失败只丢掉这一次投影，不影响执行事实。

mod compaction;

use std::path::PathBuf;
use std::sync::Arc;

use singularity_agent::agent::{Agent, AgentConfig, AgentEvent, AgentTerminalReason, UserQuestions};
use singularity_agent::agent::{SteeringInbox, SteeringInboxHandle};
use singularity_agent::session::{LedgerRecord, SessionWriter, append_record_async, lock_writer};
use singularity_model::{ModelConfigManager, OpenAiProvider};

use crate::error::{TurnFailureCause, TurnRunError};
use crate::event_projection::AgentEventProjector;
use singularity_protocol::{Thread, Turn, TurnErrorDetail, TurnEvent, TurnStatus};

/// 一次收敛到可信终态的 turn 结果。completed/failed/interrupted 都是可信终态，
/// 没有可信终态的情形由 TurnRunError 表达。
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub turn_status: TurnStatus,
    /// 中断终态是否来自本轮已接受的用户停止。
    pub manually_stopped: bool,
}

/// 进程内的 turn 执行器：本身不保存状态，可以共享，按需构造。
pub struct TurnRunner {
    sessions_dir: PathBuf,
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
        Self { sessions_dir, models, mcp }
    }

    /// 校验模型 selector 能被当前磁盘配置解析成具体的 provider 配置。
    /// 用于用户修改会话模型；执行准备直接解析当轮配置。
    pub(crate) fn validate_model_selector(&self, selector: &str) -> Result<(), String> {
        self.models
            .snapshot()
            .validate_selector(Some(selector))
            .map_err(|error| format!("invalid model selector: {error}"))
    }

    /// 执行已准备的 turn，直到终态收敛。调用方持有 crate::conversation::TurnControls，
    /// 以便在执行期间注入输入或取消。
    ///
    /// 返回 Ok 时终态（completed/failed/interrupted）已落盘，终态事件也已发出，失败终态的
    /// 错误细节由 turn/error 事件携带。返回
    /// TurnRunError::Execution 时终态记录写不下去，不会发出终态事件。
    ///
    /// 首条与追加输入均由 Agent 从会话输入箱读取并保存。
    pub(crate) async fn run(
        controls: &Arc<crate::conversation::TurnControls>,
        (mut agent, started_at): (Agent, String),
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome, TurnRunError> {
        let turn_id = controls.turn_id.clone();
        let writer = controls.writer();
        let turn = Turn {
            turn_id: turn_id.clone(),
            status: TurnStatus::Running,
        };
        sink(TurnEvent::TurnStarted { turn, started_at });

        let mut item_events = AgentEventProjector::new(Some(turn_id.clone()));
        let run_result = {
            let mut on_event = |event: AgentEvent| match event {
                AgentEvent::CompactionUpdated { ref item } => {
                    let active = matches!(
                        item,
                        singularity_protocol::HistoryItem::CompactionResult {
                            status: TurnStatus::Running,
                            ..
                        }
                    );
                    controls.set_compacting(active);
                    item_events.project(sink, event);
                    sink(TurnEvent::SessionStateChanged {});
                }
                AgentEvent::PendingQuestionChanged => {
                    sink(TurnEvent::SessionStateChanged {});
                }
                event => item_events.project(sink, event),
            };
            agent.run(&mut on_event, controls.cancellation()).await
        };
        let cancel_accepted = controls.finish_cancel();
        let (turn_status, error) = match run_result {
            Ok(reason) => (
                match reason {
                    // 停止后 Agent 仍可能正常收尾：终态按停止记为中断。
                    AgentTerminalReason::Completed if cancel_accepted => TurnStatus::Interrupted,
                    AgentTerminalReason::Completed => TurnStatus::Completed,
                    AgentTerminalReason::Aborted => TurnStatus::Interrupted,
                },
                None,
            ),
            // 执行期的存储/宿主故障不写可信终态，历史把未闭合的 operation 投影为中断；
            // 链条到此停止。
            Err(error) => {
                let detail = error.detail();
                if detail.cause == TurnFailureCause::Store {
                    return Err(TurnRunError::Execution(detail));
                }
                (TurnStatus::Failed, Some(detail))
            }
        };
        // 所有执行结果共用同一套顺序：冻结取消控制、终态落盘、发布终态；存储失败一律
        // fail-stop，不发布虚假终态。
        let record = LedgerRecord::OperationFinished {
            turn_id: turn_id.clone(),
            outcome: turn_status,
            // 失败终态的结构化原因随同一份持久记录落盘，重读历史时不依赖 runtime 最近一次的文本。
            error: error.clone(),
            user_stopped: cancel_accepted,
        };
        let finished_at = match append_record_async(&writer, record).await {
            Ok(timestamp) => timestamp,
            Err(storage_error) => {
                let message = match error {
                    Some(error) => format!("{error}; 保存执行结果失败：{storage_error}"),
                    None => format!("保存执行结果失败：{storage_error}"),
                };
                return Err(TurnRunError::Execution(TurnErrorDetail {
                    cause: TurnFailureCause::Store,
                    message,
                    item_id: None,
                }));
            }
        };
        let manually_stopped = turn_status == TurnStatus::Interrupted && cancel_accepted;
        if let Some(error) = &error {
            sink(TurnEvent::TurnFailed {
                turn_id: turn_id.clone(),
                error: error.clone(),
                finished_at,
            });
        } else {
            sink(TurnEvent::TurnCompleted {
                turn: Turn {
                    turn_id: turn_id.clone(),
                    status: turn_status,
                },
                manually_stopped,
                finished_at,
            });
        }
        Ok(TurnOutcome { turn_status, manually_stopped })
    }

    /// 完成本轮准备和开始记录后，开放运行控制窗口。
    pub(crate) fn prepare_turn(
        &self,
        thread: &Thread,
        controls: &crate::conversation::TurnControls,
        bash_tasks: Arc<singularity_agent::tools::BashTasks>,
    ) -> Result<(Agent, String), TurnRunError> {
        // 会话写者由 Conversation 持有；本函数只做剩下的 fail-fast 准备
        // （provider/config/项目指令），就绪之后才写 operation 状态。
        let writer = controls.writer();
        let agent = self.prepare_agent(
            thread,
            controls.inbox_handle(),
            Arc::clone(&controls.questions),
            Arc::clone(&writer),
            singularity_agent::agent::AgentExecutionContext {
                bash_tasks,
                turn_id: controls.turn_id.clone(),
            },
        )?;
        // OperationStarted 记录 turn 身份。输入消息由 Agent 单独落盘；
        // 这些追加不是一个原子事务。
        let mut writer = lock_writer(&writer);
        let started_at = writer
            .append_record(LedgerRecord::OperationStarted { turn_id: controls.turn_id.clone() })
            .map_err(|error| TurnRunError::Preparation(error.to_string()))?;
        Ok((agent, started_at))
    }

    /// 普通执行与独立压缩共用准备步骤，每次读取当前指令。
    fn prepare_agent(
        &self,
        thread: &Thread,
        inbox: SteeringInboxHandle,
        questions: Arc<UserQuestions>,
        writer: SessionWriter,
        execution: singularity_agent::agent::AgentExecutionContext,
    ) -> Result<Agent, TurnRunError> {
        let provider = self.resolve_provider(thread)?;
        let instruction_home = self.instruction_home();
        let initial_instructions =
            singularity_core::load_agent_instructions(std::path::Path::new(&thread.cwd), &instruction_home)
                .map_err(TurnRunError::Preparation)?;
        let config = AgentConfig { instruction_home, initial_instructions };
        Ok(Agent::new(inbox, provider, config, writer, questions, Arc::clone(&self.mcp), execution))
    }

    /// Provider 持有本轮冻结的模型容量；解析失败发生在 operation 开始之前。
    fn resolve_provider(&self, thread: &Thread) -> Result<OpenAiProvider, TurnRunError> {
        let snapshot = self.models.snapshot();
        OpenAiProvider::from_snapshot(&snapshot, thread.model.as_deref())
            .map_err(|error| TurnRunError::Preparation(error.to_string()))
    }

    /// 文件指令与 Skill 的用户数据根目录，即会话目录所在的数据目录。
    fn instruction_home(&self) -> PathBuf {
        self.sessions_dir
            .parent()
            .expect("sessions directory is inside the data directory")
            .to_path_buf()
    }
}
