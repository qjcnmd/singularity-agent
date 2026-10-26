//! 手动和自动压缩共用：总结早期历史、保留近期原文，成功后保存摘要和保留位置。

use super::{AgentError, RequestContext, Result, TurnEvent};
use crate::compaction::{CompactionPrefix, PreparedCompaction, validate_summary};
use crate::request_execution::execute_request;
use crate::session::{CompactionEntry, with_writer_async};
use singularity_protocol::{HistoryItem, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;

impl RequestContext {
    pub(super) async fn reduce_context_if_needed(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        if self.context_pressure_tokens()
            >= (self.context_window() as f64 * AUTO_COMPACTION_TRIGGER_RATIO).floor() as u64
        {
            self.compact_now(on_event, cancellation).await?;
        }
        Ok(())
    }

    /// 自动压缩在本轮请求上下文中选择历史，之后仍可停止本轮的模型和工具循环。
    pub(super) async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let Some(prefix) = CompactionPrefix::select(&self.session).await else {
            return compaction_unneeded(on_event, cancellation);
        };
        self.compact_selected(prefix, on_event, cancellation, || true).await?;
        Ok(true)
    }

    /// 总结已选历史。独立压缩由调用方在保存前锁定停止结果；自动压缩不结束整轮停止窗口。
    pub async fn compact_selected(
        &mut self,
        prefix: CompactionPrefix,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<()> {
        let item_id = crate::session::new_entry_id();
        on_event(TurnEvent::ItemUpdated {
            item: HistoryItem::CompactionResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        });
        let result = self.compact_context(prefix, &item_id, on_event, cancellation, begin_commit).await;
        let (outcome, item) = match result {
            Ok(summary) => (Ok(()), HistoryItem::Compaction { id: item_id, summary }),
            Err(AgentError::Aborted) => (
                Err(AgentError::Aborted),
                HistoryItem::CompactionResult {
                    id: item_id,
                    status: TurnStatus::Interrupted,
                    message: None,
                },
            ),
            Err(error) => {
                let item = HistoryItem::CompactionResult {
                    id: item_id.clone(),
                    status: TurnStatus::Failed,
                    message: Some(error.to_string()),
                };
                (Err(AgentError::Compaction { item_id, source: Box::new(error) }), item)
            }
        };
        on_event(TurnEvent::ItemUpdated { item });
        outcome
    }

    async fn compact_context(
        &mut self,
        prefix: CompactionPrefix,
        item_id: &str,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
        begin_commit: impl FnOnce() -> bool + Send,
    ) -> Result<String> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        self.ensure_definitions(on_event, cancellation).await?;
        let definitions = self.request_definitions().clone();
        let model = self.provider.model_configuration();
        let prepared =
            tokio::task::spawn_blocking(move || PreparedCompaction::new(prefix, definitions, &model))
                .await
                .expect("summary preparation completes while the runtime is running")?;
        let (response, _) = execute_request(
            &self.provider,
            &self.session,
            &prepared.request,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::Compaction,
        )
        .await?;
        let summary = validate_summary(response)?;
        if !begin_commit() || cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let entry = CompactionEntry {
            summary: summary.clone(),
            first_kept_entry_id: prepared.first_kept_entry_id,
        };
        // 摘要和保留位置写在同一行；写入失败时旧上下文仍然有效。
        let id = item_id.to_owned();
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        Ok(summary)
    }
}

/// 没有可总结的历史时沿用原有的压缩反馈，不创建请求上下文。
pub fn compaction_unneeded(
    on_event: &mut (dyn FnMut(TurnEvent) + Send),
    cancellation: &CancellationToken,
) -> Result<bool> {
    let id = crate::session::new_entry_id();
    let cancelled = cancellation.is_cancelled();
    let final_status = if cancelled {
        TurnStatus::Interrupted
    } else {
        TurnStatus::Completed
    };
    for status in [TurnStatus::Running, final_status] {
        on_event(TurnEvent::ItemUpdated {
            item: HistoryItem::CompactionResult { id: id.clone(), status, message: None },
        });
    }
    if cancelled {
        Err(AgentError::Aborted)
    } else {
        Ok(false)
    }
}
