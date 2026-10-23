//! 手动和自动压缩共用：总结早期历史、保留近期原文，成功后保存摘要和保留位置。

use super::{AgentError, RequestContext, Result, TurnEvent};
use crate::compaction::{PreparedCompaction, validate_summary};
use crate::request_execution::execute_request;
use crate::session::{CompactionEntry, context, lock_writer, with_writer_async};
use singularity_protocol::{HistoryItem, TurnStatus};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;
const KEEP_RECENT_RATIO: f64 = 0.1;

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

    /// 立即压缩，返回是否生成了摘要；可以停止，摘要保存成功前原上下文不变。
    pub async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let item_id = crate::session::new_entry_id();
        on_event(TurnEvent::ItemUpdated {
            item: HistoryItem::CompactionResult {
                id: item_id.clone(),
                status: TurnStatus::Running,
                message: None,
            },
        });
        let result = self.compact_context(&item_id, on_event, cancellation).await;
        let (outcome, item) = match result {
            Ok(Some(summary)) => (Ok(true), HistoryItem::Compaction { id: item_id, summary }),
            Ok(None) => (
                Ok(false),
                HistoryItem::CompactionResult {
                    id: item_id,
                    status: TurnStatus::Completed,
                    message: None,
                },
            ),
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
        item_id: &str,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<Option<String>> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let session = std::sync::Arc::clone(&self.session);
        let prefix = tokio::task::spawn_blocking(move || {
            let writer = lock_writer(&session);
            context::compaction_prefix(&writer, KEEP_RECENT_RATIO)
        })
        .await
        .expect("summary selection completes while the runtime is running");
        // 没有更早的对话可总结时，也无需首次加载指令、技能和 MCP 定义。
        let Some(prefix) = prefix else {
            return Ok(None);
        };
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
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let entry = CompactionEntry {
            summary: summary.clone(),
            first_kept_entry_id: prepared.first_kept_entry_id,
        };
        // 摘要和保留位置写在同一行；写入失败时旧上下文仍然有效。
        let id = item_id.to_owned();
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        Ok(Some(summary))
    }
}
