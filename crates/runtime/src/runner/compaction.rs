use super::*;

impl TurnRunner {
    /// 独立压缩占用会话直到摘要、新定义和操作结果保存完毕，不接受用户停止。
    pub(crate) async fn compact_thread(
        self: &Arc<Self>,
        thread: &Thread,
        writer: SessionWriter,
        sink: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<singularity_agent::agent::CompactionOutcome, TurnRunError> {
        let runner = Arc::clone(self);
        let start_thread = thread.clone();
        let mut agent = tokio::task::spawn_blocking(move || {
            let agent = runner.prepare_agent(&start_thread, SteeringInbox::default_handle(), writer)?;
            let agent = if runner.user_questions {
                agent.with_user_questions(Arc::new(singularity_agent::agent::UserQuestions::default()))
            } else {
                agent
            };
            Ok::<_, TurnRunError>(agent)
        })
        .await
        .expect("compaction preparation completes while the runtime is running")?;
        let mut events = AgentEventProjector::new(thread.thread_id.clone(), None);
        let mut on_event = |event| events.project(sink, event);
        let outcome = agent.compact_now(&mut on_event).await;
        if let singularity_agent::agent::CompactionOutcome::Failed(error) = &outcome
            && error.cause() == TurnFailureCause::Store
        {
            return Err(TurnRunError::Execution(error.detail()));
        }
        Ok(outcome)
    }
}
