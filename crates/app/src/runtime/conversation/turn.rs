//! 一个回合的完整执行：准备模型和指令、运行 Agent、保存并发布最终状态。
//! 回合开始记录（operation_started）先于本回合的其他内容写入日志；结束记录
//! （operation_finished，含状态和错误）先保存再发布结束事件。

use std::sync::Arc;

use singularity_agent::agent::{Agent, AgentConfig, AgentError, RequestContext, TurnEvent};
use singularity_agent::session::{LedgerRecord, lock_writer, with_writer_async};
use singularity_model::OpenAiProvider;

use singularity_protocol::{Thread, Turn, TurnErrorDetail, TurnFailureCause, TurnStatus};

use super::Conversation;

impl Conversation {
    /// 检查模型选择能否用当前配置文件解析成可用的提供方和模型。
    /// 用户修改任务模型时调用；执行准备时直接解析当轮配置。
    pub(crate) fn validate_model_selection(
        &self,
        selection: &singularity_protocol::ModelSelection,
    ) -> Result<(), String> {
        self.models
            .validate_selection(selection)
            .map_err(|error| format!("invalid model selection: {error}"))
    }

    /// 准备、执行并保存本轮结果，使用接受输入时创建的停止控制。
    ///
    /// 返回 Ok 时最终状态（完成、失败或中断）已经保存，结束事件也已发出，失败原因随
    /// turn/error 事件发送。返回 Err 表示准备失败或结果没能保存，由运行快照显示原因。
    ///
    /// 输入由 Agent 从任务的输入队列读取并保存。
    pub(super) async fn run_single_turn(
        self: &Arc<Self>,
        controls: &Arc<crate::runtime::conversation::TurnControls>,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnStatus, TurnErrorDetail> {
        let conversation = Arc::clone(self);
        let preparing = Arc::clone(controls);
        let prepared = tokio::task::spawn_blocking(move || conversation.prepare_turn(&preparing))
            .await
            .expect("turn preparation completes while the runtime is running");
        let (mut agent, started_at) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return if controls.finish_cancel() {
                    Ok(TurnStatus::Interrupted)
                } else {
                    Err(error)
                };
            }
        };
        self.lock_state().last_context_window = Some(agent.context_window());
        let turn_id = controls.turn_id.clone();
        let writer = Arc::clone(&self.writer);
        let turn = Turn {
            turn_id: turn_id.clone(),
            status: TurnStatus::Running,
        };
        sink(TurnEvent::TurnStarted { turn, started_at });

        let run_result = {
            let mut on_event = |event: TurnEvent| match event {
                TurnEvent::ItemUpdated { ref item }
                    if matches!(
                        item,
                        singularity_protocol::HistoryItem::Compaction { .. }
                            | singularity_protocol::HistoryItem::CompactionResult { .. }
                    ) =>
                {
                    let active = matches!(
                        item,
                        singularity_protocol::HistoryItem::CompactionResult {
                            status: TurnStatus::Running,
                            ..
                        }
                    );
                    controls.set_compacting(active);
                    sink(event);
                    sink(TurnEvent::SessionStateChanged {});
                }
                event => sink(event),
            };
            agent.run(&mut on_event, controls.cancellation()).await
        };
        let cancel_accepted = controls.finish_cancel();
        let (turn_status, error) = match run_result {
            // 停止后 Agent 仍可能正常走完：只要用户停止过，就记为中断。
            Ok(()) if cancel_accepted => (TurnStatus::Interrupted, None),
            Ok(()) => (TurnStatus::Completed, None),
            Err(AgentError::Aborted) => (TurnStatus::Interrupted, None),
            // 写日志失败时不再尝试写结束记录；读取历史时，没有结束记录的回合显示为中断。
            Err(error) => {
                let detail = error.detail();
                if detail.cause == TurnFailureCause::Store {
                    return Err(detail);
                }
                (TurnStatus::Failed, Some(detail))
            }
        };
        // 所有结果按同一顺序处理：关闭停止入口、保存结束记录、发布结束事件。
        // 保存失败时直接报错，不发布没有保存的结束状态。
        let record = LedgerRecord::OperationFinished {
            turn_id: turn_id.clone(),
            outcome: turn_status,
            // 失败原因随结束记录一起保存，重新打开任务时直接从日志显示。
            error: error.clone(),
            user_stopped: cancel_accepted,
        };
        let finished_at = match with_writer_async(&writer, move |writer| writer.append_record(record)).await {
            Ok(timestamp) => timestamp,
            Err(storage_error) => {
                let message = match error {
                    Some(error) => format!("{error}; 保存执行结果失败：{storage_error}"),
                    None => format!("保存执行结果失败：{storage_error}"),
                };
                return Err(TurnErrorDetail {
                    cause: TurnFailureCause::Store,
                    message,
                    item_id: None,
                });
            }
        };
        let manually_stopped = turn_status == TurnStatus::Interrupted && cancel_accepted;
        if let Some(error) = &error {
            sink(TurnEvent::TurnFailed { error: error.clone(), finished_at });
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
        Ok(turn_status)
    }

    /// 准备模型和项目指令，全部成功后才保存回合开始记录；任何一步失败都不写日志。
    fn prepare_turn(
        &self,
        controls: &crate::runtime::conversation::TurnControls,
    ) -> Result<(Agent, String), TurnErrorDetail> {
        let writer = Arc::clone(&self.writer);
        let inbox = Arc::clone(&self.lock_state().steering_inbox);
        let agent = Agent::new(
            self.prepare_request_context(&self.thread())?,
            inbox,
            Arc::clone(&controls.questions),
            Arc::clone(&self.bash_tasks),
        );
        // 开始记录只保存回合 ID；输入消息由 Agent 之后单独保存，两次写入不是一个事务。
        let mut writer = lock_writer(&writer);
        let started_at = writer
            .append_record(LedgerRecord::OperationStarted { turn_id: controls.turn_id.clone() })
            .map_err(preparation_error)?;
        Ok((agent, started_at))
    }

    /// 普通回合与独立压缩共用的准备步骤；每次都重新读取模型配置和项目指令。
    pub(super) fn prepare_request_context(&self, thread: &Thread) -> Result<RequestContext, TurnErrorDetail> {
        let provider = OpenAiProvider::new(&self.models, &thread.model).map_err(preparation_error)?;
        let instruction_home = self.home.clone();
        let initial_instructions =
            singularity_core::load_agent_instructions(std::path::Path::new(&thread.cwd), &instruction_home)
                .map_err(preparation_error)?;
        let config = AgentConfig { instruction_home, initial_instructions };
        Ok(RequestContext::new(provider, config, Arc::clone(&self.writer), Arc::clone(&self.mcp)))
    }
}

fn preparation_error(error: impl std::fmt::Display) -> TurnErrorDetail {
    TurnErrorDetail {
        cause: TurnFailureCause::Internal,
        message: error.to_string(),
        item_id: None,
    }
}
