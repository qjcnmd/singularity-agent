//! 手动和自动共用压缩：总结早期历史、保留近期原文，成功后提交摘要和新定义。

use super::{Agent, AgentError, AgentEvent, Result};
use crate::compaction::PreparedCompaction;
use crate::request_execution::execute_request;
use crate::session::{LedgerRecord, append_record_async, context, lock_writer, with_writer_async};
use singularity_protocol::{HistoryItem, TurnFailureCause, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;
const KEEP_RECENT_RATIO: f64 = 0.1;

/// 一次完整压缩的结果；失败已有对应显示条目，调用方只安排后续执行。
#[derive(Debug)]
pub enum CompactionOutcome {
    Compacted,
    Unchanged,
    Failed(AgentError),
}

impl CompactionOutcome {
    /// 自动入口把失败交给当前回合，手动入口直接展示本次结果。
    pub(super) fn into_result(self) -> Result<bool> {
        match self {
            Self::Compacted => Ok(true),
            Self::Unchanged => Ok(false),
            Self::Failed(error) => Err(error),
        }
    }
}

impl Agent {
    pub(super) async fn reduce_context_if_needed(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        if self.context_pressure_tokens()
            >= (self.context_window() as f64 * AUTO_COMPACTION_TRIGGER_RATIO).floor() as u64
        {
            self.compact_now(on_event).await.into_result()?;
        }
        Ok(())
    }

    /// 压缩期间不接受用户停止；请求仍按共同规则处理超时、重试和最终失败。
    pub async fn compact_now(&mut self, on_event: &mut (dyn FnMut(AgentEvent) + Send)) -> CompactionOutcome {
        let item_id = crate::session::new_entry_id();
        on_event(AgentEvent::CompactionUpdated {
            item: HistoryItem::CompactionResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        });
        let saved_id = item_id.clone();
        let result = match with_writer_async(&self.session, move |writer| {
            writer.append_record_with_id(saved_id, LedgerRecord::OperationStarted { turn_id: None })
        })
        .await
        {
            Ok(_) => self.compact_context(on_event).await,
            Err(error) => Err(error.into()),
        };
        let result = self.save_compaction_result(result).await;
        let (outcome, item) = match result {
            Ok(Some(summary)) => {
                (CompactionOutcome::Compacted, HistoryItem::Compaction { id: item_id, summary })
            }
            Ok(None) => (
                CompactionOutcome::Unchanged,
                HistoryItem::CompactionResult {
                    id: item_id,
                    status: TurnStatus::Completed,
                    message: None,
                },
            ),
            Err(error) => {
                let item = HistoryItem::CompactionResult {
                    id: item_id.clone(),
                    status: TurnStatus::Failed,
                    message: Some(error.to_string()),
                };
                (
                    CompactionOutcome::Failed(AgentError::Compaction { item_id, source: Box::new(error) }),
                    item,
                )
            }
        };
        on_event(AgentEvent::CompactionUpdated { item });
        outcome
    }

    /// 所有触发方式在同一处保存终态；存储已失败时停止写入，同时保留原始原因。
    async fn save_compaction_result(&self, result: Result<Option<String>>) -> Result<Option<String>> {
        if result.as_ref().err().is_some_and(|error| error.cause() == TurnFailureCause::Store) {
            return result;
        }
        let record = LedgerRecord::OperationFinished {
            turn_id: None,
            outcome: if result.is_ok() {
                TurnStatus::Completed
            } else {
                TurnStatus::Failed
            },
            error: result.as_ref().err().map(AgentError::detail),
            user_stopped: false,
        };
        if let Err(storage) = append_record_async(&self.session, record).await {
            return Err(match result {
                Err(execution) => AgentError::FailureRecording { execution: Box::new(execution), storage },
                Ok(_) => AgentError::Session(storage),
            });
        }
        result
    }

    async fn compact_context(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<Option<String>> {
        // 独立令牌只适配共同请求接口，不连接普通任务的停止信号。
        let cancellation = CancellationToken::new();
        self.ensure_definitions(on_event, &cancellation).await?;
        let definitions = self.request_definitions().clone();
        let session = std::sync::Arc::clone(&self.session);
        let model = self.provider.model_configuration();
        let summary = tokio::task::spawn_blocking(move || {
            let prefix = {
                let writer = lock_writer(&session);
                let keep_tokens =
                    (context::estimated_tokens(&writer) as f64 * KEEP_RECENT_RATIO).ceil() as u64;
                context::compaction_prefix(&writer, keep_tokens)
            };
            prefix.map(|prefix| PreparedCompaction::new(prefix, &definitions, &model)).transpose()
        })
        .await
        .expect("summary preparation completes while the runtime is running")?;
        // 保留近期完整消息后没有更早的对话可总结，沿用现有上下文。
        let Some(summary) = summary else {
            return Ok(None);
        };
        let (response, id) = execute_request(
            &self.provider,
            &self.session,
            &mut self.accounting,
            &summary.request,
            on_event,
            &cancellation,
            singularity_protocol::RequestPurpose::Compaction,
        )
        .await?;
        let mut entry = summary.into_entry(response)?;
        let (definitions, registry) = self.load_definitions(on_event, &cancellation).await?;
        entry.definitions = Some(definitions.clone());
        let summary = entry.summary.clone();
        // 一条完整记录同时切换对话与定义；读取或写入失败时旧上下文仍有效。
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        self.definitions = Some(definitions);
        self.registry = registry;
        self.usage_correction = 0;
        Ok(Some(summary))
    }
}
