//! 上下文缩减的编排：触发、剪枝、摘要提交与继续请求前的指令刷新。
//! 手动压缩结束后释放 Agent；自动压缩和溢出恢复共用提交后的收尾规则。

use super::{Agent, AgentError, AgentEvent, Result};
use crate::compaction::PreparedCompaction;
use crate::request_execution::execute_request;
use crate::session::{RequestDefinitions, lock_writer, with_writer_async};
use tokio_util::sync::CancellationToken;

/// 上下文占用达到窗口的这一比例时触发自动压缩。
const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;
/// 所有压缩入口至少保留窗口的这一比例作为近期历史。
const COMPACTION_RETAIN_RATIO: f64 = 0.1;

impl Agent {
    /// 把剪枝作为「引用原消息」的追加记录落盘，后续投影使用替换后的正文。
    /// 剪枝覆盖整个活动历史：超长工具结果不分新旧。
    async fn prune_tool_results(&mut self, cancellation: &CancellationToken) -> Result<bool> {
        let signal = cancellation.clone();
        let session = std::sync::Arc::clone(&self.session);
        let changed = tokio::task::spawn_blocking(move || {
            let replacements = {
                let writer = lock_writer(&session);
                writer.context().pruned_tool_results(&writer)
            };
            let changed = !replacements.is_empty();
            for record in replacements {
                if signal.is_cancelled() {
                    return Err(AgentError::Aborted);
                }
                lock_writer(&session).append_record(record)?;
            }
            Ok(changed)
        })
        .await
        .expect("context worker completes while the runtime is running")?;
        if changed {
            self.usage_correction = 0;
        }
        Ok(changed)
    }

    /// 所有触发共用剪枝与摘要提交；返回是否缩减了上下文。
    async fn compact_context(
        &mut self,
        definitions: &RequestDefinitions,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let pruned = self.prune_tool_results(cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let keep_recent_tokens =
            (self.model.context_window() as f64 * COMPACTION_RETAIN_RATIO).floor() as u64;
        // 历史里没有可替换的前缀（内容太少）：本次无需摘要。
        let summary_definitions = definitions.clone();
        let model = self.model.clone();
        let session = std::sync::Arc::clone(&self.session);
        let summary = tokio::task::spawn_blocking(move || {
            let prefix = {
                let writer = lock_writer(&session);
                writer.context().compaction_prefix(&writer, keep_recent_tokens)
            };
            prefix
                .map(|prefix| PreparedCompaction::new(prefix, &summary_definitions, &model))
                .transpose()
        })
        .await
        .expect("image context preparation completes while the runtime is running")?;
        let Some(summary) = summary else {
            return Ok(pruned);
        };
        // 请求层已经做过唯一一次 ProviderCallError→AgentError 分类；压缩只传播结果，
        // 不再按取消令牌改写真实失败原因（停止是否被接受由操作层的终态边界裁决）。
        let (response, id) = execute_request(
            self.provider.as_ref(),
            &self.session,
            &mut self.accounting,
            &summary.request,
            definitions,
            on_event,
            cancellation,
            singularity_protocol::RequestPurpose::Compaction,
        )
        .await?;
        let entry = summary.into_entry(response)?;
        // 摘要已生成但落盘前被取消：这次摘要不写入会话。
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        with_writer_async(&self.session, move |writer| writer.append_compaction_with_id(&id, entry)).await?;
        self.usage_correction = 0;
        Ok(true)
    }

    pub(super) async fn reduce_context_if_needed(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if self.context_pressure_tokens()
            >= (self.model.context_window() as f64 * AUTO_COMPACTION_TRIGGER_RATIO).floor() as u64
        {
            self.force_compact(on_event, cancellation).await?;
        }
        Ok(())
    }

    /// 执行中的压缩完成后，直接重读指令和工具，为下一次正常请求准备材料。
    pub(super) async fn force_compact(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let changed = self.compact_context(&self.request_definitions(), on_event, cancellation).await?;
        if changed {
            self.refresh_instructions(on_event).await?;
            self.refresh_tools(on_event, cancellation).await;
        }
        Ok(changed)
    }

    /// 独立压缩复用最近请求的完整前缀；没有请求记录时从当前配置准备材料。
    /// 保存后结束操作，下一次正常执行直接加载当前指令和工具。
    pub async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let previous = lock_writer(&self.session)
            .latest_request_definitions()
            .map(|(_, definitions)| definitions.clone());
        let definitions = match previous {
            Some(definitions) => definitions,
            None => {
                self.refresh_instructions(on_event).await?;
                self.refresh_tools(on_event, cancellation).await;
                self.request_definitions()
            }
        };
        self.compact_context(&definitions, on_event, cancellation).await?;
        Ok(())
    }
}
