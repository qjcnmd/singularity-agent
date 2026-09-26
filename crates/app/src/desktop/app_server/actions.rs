use super::*;

impl AppServer {
    pub fn submit(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        if text.trim().is_empty() {
            return Err(invalid_request("任务内容不能为空。"));
        }
        // 查找或创建 slot 和建立执行预订属于同一段生命周期交接，归档或移除插不进这两步之间。
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(workspace_id, session_id)?;
            let selector = slot.conversation().thread().model;
            self.validate_model_selector(selector.as_deref())?;
            let reservation = slot
                .conversation()
                .reserve_start()
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
        self.spawn_operation(session_id, slot, reservation, Operation::Turn(text));
        Ok(())
    }

    pub fn steer(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, move |conversation| {
            conversation.steer(text).map(|_| ())
        })
    }

    pub fn follow_up(
        &self,
        workspace_id: &str,
        session_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, move |conversation| {
            conversation.submit_follow_up(text).map(|_| ())
        })
    }

    pub fn queue_withdraw(
        &self,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, |conversation| {
            conversation.withdraw_follow_up(control_id).map(|_| ())
        })
    }

    pub fn queue_replace(
        &self,
        workspace_id: &str,
        session_id: &str,
        control_id: &str,
        text: String,
    ) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, move |conversation| {
            conversation.replace_follow_up(control_id, text).map(|_| ())
        })
    }

    /// 立即发送：`control_id` 指定要提升的那一条，省略就提升队列里全部待处理
    /// 输入。要提升哪些由队列 owner 在临界区里读，前端不用照自己的快照逐条请求。
    pub fn queue_send_now(
        self: &Arc<Self>,
        workspace_id: &str,
        session_id: &str,
        control_id: Option<&str>,
    ) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        // 和 worker 的事件、结算共用同一把 SlotState 锁：控制从 Conversation
        // 转到公开投影并发布完之前，结算不能插进来把旧回执盖掉。
        let mut state = slot.lock_state();
        let promoted = slot
            .conversation()
            .promote_pending(control_id)
            .map_err(control_error)?;
        match promoted {
            FollowUpPromotion::Empty => Ok(()),
            FollowUpPromotion::Injected => {
                self.publish_session_locked(session_id, &slot, &mut state);
                Ok(())
            }
            FollowUpPromotion::Reserved { reservation } => {
                // 预订成立就等于独占了该会话；先放开 slot 锁去取 history，再按同样的顺序提交。
                drop(state);
                // 生命周期交接已经由预订做完，后面的读盘和启动不再占全局临界区。
                drop(lifecycle);
                // 只有真要启动新一轮时才解析未来的模型配置：往当前轮注入和
                // 空队列 no-op 都不受这个 selector 影响。校验失败时，预订 guard
                // 的 Drop 会把已提升的输入按接受顺序放回队列，输入不会丢。
                self.validate_model_selector(slot.conversation().thread().model.as_deref())?;
                self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
                self.spawn_operation(session_id, slot, reservation, Operation::Promoted);
                Ok(())
            }
        }
    }

    pub fn abort(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(workspace_id, session_id, Conversation::abort)
    }

    /// 范围校验和控制接受共用生命周期锁，公开投影和发布共用 SlotState 顺序。
    /// 闭包里只做 Conversation 的短控制操作，不能覆盖 Agent 执行或调用事件 sink。
    fn apply_control(
        &self,
        workspace_id: &str,
        session_id: &str,
        apply: impl FnOnce(&Conversation) -> Result<(), ConversationControlError>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        let mut state = slot.lock_state();
        apply(slot.conversation()).map_err(control_error)?;
        self.publish_session_locked(session_id, &slot, &mut state);
        Ok(())
    }

    /// 预订成立后先读 history，再在同一把状态锁里初始化并发布这次操作的投影。
    fn begin_operation(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        begin: impl FnOnce(&mut SlotState, Arc<singularity_runtime::ThreadSnapshot>),
    ) -> Result<(), RpcError> {
        let history = self.read_persisted_history(slot)?;
        let mut state = slot.lock_state();
        begin(&mut state, history);
        self.publish_session_locked(session_id, slot, &mut state);
        Ok(())
    }

    pub(super) fn publish_session_locked(
        &self,
        session_id: &str,
        slot: &ConversationSlot,
        state: &mut SlotState,
    ) {
        state.bump_revision();
        self.emit(StreamEvent::SessionChanged {
            session_id: session_id.to_string(),
            payload: slot.runtime_from(state),
        });
    }

    pub fn compact(self: &Arc<Self>, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(workspace_id, session_id)?;
            let reservation = slot
                .conversation()
                .reserve_compaction()
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, |state, history| {
            state.begin_compaction(history, now_iso());
        })?;
        self.spawn_operation(session_id, slot, reservation, Operation::Compaction);
        Ok(())
    }

    pub fn rename_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        name: &str,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation().phase() != SessionPhase::Idle {
            return Err(session_busy());
        }
        self.catalog
            .rename(session_id, name)
            .map_err(catalog_error)?;
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn archive_session(&self, workspace_id: &str, session_id: &str) -> Result<(), RpcError> {
        // 占用检查、持久归档和注销 slot 必须在同一个临界区里，否则归档完的旧 slot 还会被启动。
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(workspace_id, session_id)?;
        if slot.conversation().is_occupied() {
            return Err(session_busy());
        }
        #[cfg(test)]
        take_pause(&self.archive_check_pause);
        self.catalog.archive(session_id).map_err(catalog_error)?;
        self.lock_sessions().remove(session_id);
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn update_settings(
        &self,
        workspace_id: &str,
        session_id: &str,
        selector: &str,
    ) -> Result<(), RpcError> {
        let slot = {
            let _lifecycle = self.lock_lifecycle();
            self.open_slot(workspace_id, session_id)?
        };
        slot.conversation()
            .update_settings(selector)
            .map_err(conversation_error)?;
        self.bump_and_emit_session(session_id, &slot);
        Ok(())
    }
}
