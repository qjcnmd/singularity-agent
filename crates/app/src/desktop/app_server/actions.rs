use super::*;

impl AppServer {
    pub fn image_data(&self, session_id: &str, image_id: &str) -> Result<String, RpcError> {
        self.catalog.image_data(session_id, image_id).map_err(catalog_error)
    }

    /// 启动后台执行并等待本次输入保存；RPC 阻塞线程只等本地提交，不等待模型或工具。
    /// 返回失败时前端保留草稿，执行终态仍通过会话事件发布。
    pub fn submit(self: &Arc<Self>, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        singularity_runtime::validate_input(&input).map_err(control_error)?;
        // 查找或创建 slot 和建立执行预订属于同一段生命周期交接，归档或移除插不进这两步之间。
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_action_slot(session_id)?;
            let reservation = slot.conversation().reserve_start(input).map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
        let item_id = reservation.submitted_item_id().expect("reserve_start binds submitted input");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.spawn_operation(session_id, slot, reservation, Some(SubmissionReceipt { item_id, sender }));
        receiver.recv().expect("operation reports submitted input commit or failure")
    }

    pub fn answer_question(
        &self,
        session_id: &str,
        item_id: &str,
        answers: Vec<singularity_protocol::UserQuestionAnswer>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        let mut state = slot.lock_state();
        slot.conversation().answer_question(item_id, answers).map_err(invalid_request)?;
        self.publish_session_locked(session_id, &slot, &mut state);
        Ok(())
    }

    pub fn steer(&self, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        self.apply_control(session_id, move |conversation| conversation.steer(input))
    }

    pub fn abort(&self, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, Conversation::abort)
    }

    /// 会话查找和控制接受共用生命周期锁，公开投影和发布共用 SlotState 顺序。
    /// 闭包里只做 Conversation 的短控制操作，不能覆盖 Agent 执行或调用事件 sink。
    fn apply_control<T>(
        &self,
        session_id: &str,
        apply: impl FnOnce(&Conversation) -> Result<T, ConversationControlError>,
    ) -> Result<T, RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        let mut state = slot.lock_state();
        let result = apply(slot.conversation()).map_err(control_error)?;
        self.publish_session_locked(session_id, &slot, &mut state);
        Ok(result)
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

    pub fn compact(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_action_slot(session_id)?;
            let reservation = slot.conversation().reserve_compaction().map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, |state, history| {
            state.begin_compaction(history, now_iso());
        })?;
        self.spawn_operation(session_id, slot, reservation, None);
        Ok(())
    }

    /// 保存任务展示名称；活动执行继续使用当前写者。
    pub fn rename_session(&self, session_id: &str, name: &str) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        slot.conversation().rename(name).map_err(conversation_error)?;
        drop(lifecycle);
        self.publish_app_snapshot();
        Ok(())
    }

    /// 停止目标任务并等到执行结算，再移动日志；等待期间只限制该任务的新操作。
    pub fn archive_session(&self, session_id: &str) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.lock_sessions().get(session_id).cloned();
        let Some(slot) = slot else {
            // 尚未加载的任务没有执行要等待；移动完成前保持生命周期锁，阻止新建 slot。
            self.catalog.archive(session_id).map_err(catalog_error)?;
            drop(lifecycle);
            self.publish_app_snapshot();
            return Ok(());
        };
        if slot.is_archiving() {
            return Err(session_busy());
        }
        let _archive = slot.begin_archive();
        drop(lifecycle);
        self.runtime_handle.block_on(self.stop_sessions(std::slice::from_ref(&slot)));
        let lifecycle = self.lock_lifecycle();
        slot.conversation()
            .with_idle_writer(|| self.catalog.archive(session_id).map_err(catalog_error))
            .map_err(conversation_error)??;
        self.lock_sessions().remove(session_id);
        drop(lifecycle);
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn update_settings(&self, session_id: &str, selector: &str) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        slot.conversation().update_settings(selector).map_err(conversation_error)?;
        self.bump_and_emit_session(session_id, &slot);
        Ok(())
    }
}
