//! 上下文缩减的编排：触发、剪枝、摘要提交与继续请求前的指令刷新。
//! 手动压缩结束后释放 Agent；自动压缩和溢出恢复共用提交后的收尾规则。

use super::{Agent, AgentError, AgentEvent, Result};
use crate::compaction::PreparedCompaction;
use crate::events::{AgentDiagnostic, diagnostic_code};
use crate::request_execution::execute_request;
use crate::session::{lock_writer, with_writer_async};
use tokio_util::sync::CancellationToken;

/// 上下文占用达到窗口的这一比例时触发自动压缩。
const AUTO_COMPACTION_TRIGGER_RATIO: f64 = 0.9;
/// 自动摘要至少保留窗口的这一比例作为近期历史。
const AUTO_COMPACTION_RETAIN_RATIO: f64 = 0.1;

fn emit_compaction_skipped(on_event: &mut dyn FnMut(AgentEvent), error: &AgentError) {
    on_event(AgentEvent::Diagnostic(AgentDiagnostic::warning(
        diagnostic_code::COMPACTION_SKIPPED,
        format!("automatic context compaction skipped: {error}"),
    )));
}

/// 这次摘要失败能不能跳过、继续发本次请求：只有「摘要内容不可用」和「可重试的暂时失败
/// 已用尽重试预算」可以跳过；永久 provider 失败、取消与存储故障必须向上传播。
fn compaction_may_be_skipped(error: &AgentError) -> bool {
    match error {
        AgentError::InvalidSummary(_) => true,
        AgentError::Provider(provider) => provider.is_retryable(),
        _ => false,
    }
}

impl Agent {
    /// 把剪枝作为「引用原消息」的追加记录落盘，随后从同一份账本重建模型视图。
    /// 剪枝覆盖整个活动历史：超长工具结果不分新旧。
    async fn prune_tool_results(&mut self, cancellation: &CancellationToken) -> Result<bool> {
        let signal = cancellation.clone();
        Self::with_context(&self.session, &mut self.context, move |session, context| {
            let replacements = context.pruned_tool_results(&lock_writer(session));
            let changed = !replacements.is_empty();
            for record in replacements {
                if signal.is_cancelled() {
                    return Err(AgentError::Aborted);
                }
                lock_writer(session).append_record(record)?;
            }
            if changed {
                context.rebuild(&lock_writer(session));
            }
            Ok(changed)
        })
        .await
    }

    /// 摘要先选出历史前缀，再和本轮冻结的系统提示词、工具定义一起组装，
    /// 返回是否提交了摘要。
    async fn compact_with_record(
        &mut self,
        keep_recent_tokens: u64,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Aborted);
        }
        let instructions = self.instruction_prefix();
        // 历史里没有可替换的前缀（内容太少）：本次无需摘要。
        let prefix =
            Self::with_context(&self.session, &mut self.context, move |session, context| {
                Ok(context.compaction_prefix(&lock_writer(session), keep_recent_tokens))
            })
            .await?;
        let Some(prefix) = prefix else {
            return Ok(false);
        };
        let tools = self.registry.provider_schemas();
        let model = self.model.clone();
        let overhead = self.request_overhead_tokens();
        let summary = tokio::task::spawn_blocking(move || {
            PreparedCompaction::new(prefix, instructions, tools, &model, overhead)
        })
        .await
        .expect("image context preparation completes while the runtime is running")?;
        // 请求层已经做过唯一一次 ProviderCallError→AgentError 分类；压缩只传播结果，
        // 不再按取消令牌改写真实失败原因（停止是否被接受由操作层的终态边界裁决）。
        let (response, id) = execute_request(
            self.provider.as_ref(),
            &self.session,
            &mut self.accounting,
            &summary.request,
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
        with_writer_async(&self.session, move |writer| {
            writer.append_compaction_with_id(&id, entry)
        })
        .await?;
        Ok(true)
    }

    /// 已提交的摘要先重建历史；任何已提交的缩减都在继续请求前刷新指令。
    async fn finish_context_reduction(
        &mut self,
        pruned: bool,
        summarized: bool,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
    ) -> Result<bool> {
        if summarized {
            Self::with_context(&self.session, &mut self.context, |session, context| {
                context.rebuild(&lock_writer(session));
                Ok(())
            })
            .await?;
        }
        let changed = pruned || summarized;
        if changed {
            self.refresh_instructions(on_event).await?;
        }
        Ok(changed)
    }

    pub(super) async fn reduce_context_if_needed(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if !self.needs_context_reduction() {
            return Ok(());
        }
        let pruned = self.prune_tool_results(cancellation).await?;
        let summarized = if self.needs_context_reduction() {
            let retain =
                (self.model.context_window() as f64 * AUTO_COMPACTION_RETAIN_RATIO).floor() as u64;
            match self
                .compact_with_record(retain, on_event, cancellation)
                .await
            {
                Ok(summarized) => summarized,
                Err(error) if compaction_may_be_skipped(&error) => {
                    emit_compaction_skipped(on_event, &error);
                    false
                }
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        self.finish_context_reduction(pruned, summarized, on_event)
            .await?;
        Ok(())
    }

    fn needs_context_reduction(&self) -> bool {
        self.context_pressure_tokens()
            >= (self.model.context_window() as f64 * AUTO_COMPACTION_TRIGGER_RATIO).floor() as u64
    }

    /// Provider 明确报告上下文溢出时再次缩减，返回是否提交了缩减结果。
    pub(super) async fn force_compact(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<bool> {
        let pruned = self.prune_tool_results(cancellation).await?;
        let summarized = match self.compact_with_record(0, on_event, cancellation).await {
            Ok(summarized) => summarized,
            // 可跳过的摘要失败只有在剪枝确已提交时才算恢复成功。
            Err(error) if pruned && compaction_may_be_skipped(&error) => {
                emit_compaction_skipped(on_event, &error);
                false
            }
            Err(error) => return Err(error),
        };
        self.finish_context_reduction(pruned, summarized, on_event)
            .await
    }

    /// 手动压缩：跳过压力阈值判断，保留最后一个完整消息或工具单元。
    pub async fn compact_now(
        &mut self,
        on_event: &mut (dyn FnMut(AgentEvent) + Send),
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let loaded = self.config.initial_instructions.take();
        self.apply_instructions(loaded, on_event);
        self.prune_tool_results(cancellation).await?;
        self.compact_with_record(0, on_event, cancellation).await?;
        // 本次手动操作到此结束，下一次执行会重新加载指令。
        Ok(())
    }
}
