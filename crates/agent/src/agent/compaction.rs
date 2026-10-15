//! 手动和自动共用压缩：总结早期历史、保留近期原文，成功后提交摘要和新定义。

use super::{Agent, AgentError, AgentEvent, Result};
use crate::compaction::PreparedCompaction;
use crate::request_execution::execute_request;
use crate::session::{context, lock_writer, with_writer_async};
use tokio_util::sync::CancellationToken;

const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;
const KEEP_RECENT_RATIO: f64 = 0.1;

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
            >= (self.model.context_window() as f64 * AUTO_COMPACTION_TRIGGER_RATIO).floor() as u64
        {
            self.compact_now(on_event).await?;
        }
        Ok(())
    }

    /// 压缩期间不接受用户停止；请求仍按共同规则处理超时、重试和最终失败。
    pub async fn compact_now(&mut self, on_event: &mut (dyn FnMut(AgentEvent) + Send)) -> Result<bool> {
        on_event(AgentEvent::CompactionStateChanged { active: true });
        let result = self.compact_context(on_event).await;
        on_event(AgentEvent::CompactionStateChanged { active: false });
        result
    }

    async fn compact_context(&mut self, on_event: &mut (dyn FnMut(AgentEvent) + Send)) -> Result<bool> {
        // 独立令牌只适配共同请求接口，不连接普通任务的停止信号。
        let cancellation = CancellationToken::new();
        self.ensure_definitions(on_event, &cancellation).await?;
        let definitions = self.request_definitions().clone();
        let session = std::sync::Arc::clone(&self.session);
        let model = self.model.clone();
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
            return Ok(false);
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
        let item = singularity_protocol::HistoryItem::Compaction {
            id: id.clone(),
            summary: entry.summary.clone(),
        };
        // 一条完整记录同时切换对话与定义；读取或写入失败时旧上下文仍有效。
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        self.definitions = Some(definitions);
        self.registry = registry;
        self.usage_correction = 0;
        on_event(AgentEvent::CompactionFinished { item });
        Ok(true)
    }
}
