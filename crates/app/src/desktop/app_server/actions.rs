use super::*;

impl AppServer {
    pub fn image_data(&self, session_id: &str, image_id: &str) -> Result<String, RpcError> {
        let slot = self.lock_sessions().get(session_id).cloned();
        if let Some(image) = slot.and_then(|slot| slot.conversation().pending_image(image_id)) {
            return Ok(image.data_url());
        }
        self.catalog
            .image_data(session_id, image_id)
            .map_err(catalog_error)
    }

    /// 接受输入并启动后台执行；返回成功表示已接受，终态通过会话事件发布。
    pub fn submit(self: &Arc<Self>, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        singularity_runtime::validate_input(&input).map_err(control_error)?;
        // 查找或创建 slot 和建立执行预订属于同一段生命周期交接，归档或移除插不进这两步之间。
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(session_id)?;
            let reservation = slot
                .conversation()
                .reserve_start(input)
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
        self.spawn_operation(session_id, slot, reservation);
        Ok(())
    }

    pub fn answer_question(
        &self,
        session_id: &str,
        item_id: &str,
        answers: Vec<singularity_protocol::UserQuestionAnswer>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(session_id)?;
        let mut state = slot.lock_state();
        slot.conversation()
            .answer_question(item_id, answers)
            .map_err(invalid_request)?;
        self.publish_session_locked(session_id, &slot, &mut state);
        Ok(())
    }

    pub fn steer(&self, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        self.apply_control(session_id, move |conversation| conversation.steer(input))
    }

    pub fn follow_up(&self, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        self.apply_control(session_id, move |conversation| {
            conversation.submit_follow_up(input)
        })
    }

    pub fn queue_withdraw(&self, session_id: &str, control_id: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| {
            conversation.withdraw_follow_up(control_id)
        })
    }

    pub fn queue_replace(
        &self,
        session_id: &str,
        control_id: &str,
        input: UserInput,
    ) -> Result<(), RpcError> {
        self.apply_control(session_id, move |conversation| {
            conversation.replace_follow_up(control_id, input)
        })
    }

    /// 立即发送：`control_id` 指定要提升的那一条，省略就提升队列里全部待处理
    /// 输入。要提升哪些由队列 owner 在临界区里读，前端不用照自己的快照逐条请求。
    pub fn queue_send_now(
        self: &Arc<Self>,
        session_id: &str,
        control_id: Option<&str>,
    ) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(session_id)?;
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
                self.begin_operation(session_id, &slot, SlotState::begin_turn)?;
                self.spawn_operation(session_id, slot, reservation);
                Ok(())
            }
        }
    }

    pub fn abort(&self, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, Conversation::abort)
    }

    /// 会话查找和控制接受共用生命周期锁，公开投影和发布共用 SlotState 顺序。
    /// 闭包里只做 Conversation 的短控制操作，不能覆盖 Agent 执行或调用事件 sink。
    fn apply_control(
        &self,
        session_id: &str,
        apply: impl FnOnce(&Conversation) -> Result<(), ConversationControlError>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(session_id)?;
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

    pub fn compact(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let (slot, reservation) = {
            let _lifecycle = self.lock_lifecycle();
            let slot = self.open_slot(session_id)?;
            let reservation = slot
                .conversation()
                .reserve_compaction()
                .map_err(conversation_error)?;
            (slot, reservation)
        };
        self.begin_operation(session_id, &slot, |state, history| {
            state.begin_compaction(history, now_iso());
        })?;
        self.spawn_operation(session_id, slot, reservation);
        Ok(())
    }

    /// 保存任务展示名称；活动执行继续使用当前写者。
    pub fn rename_session(&self, session_id: &str, name: &str) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let slot = self.open_slot(session_id)?;
        slot.conversation()
            .rename(name)
            .map_err(conversation_error)?;
        drop(lifecycle);
        self.publish_app_snapshot();
        Ok(())
    }

    /// 归档无活动操作或待处理输入的任务；历史文件保留在归档目录。
    pub fn archive_session(&self, session_id: &str) -> Result<(), RpcError> {
        // 占用检查、持久归档和注销 slot 必须在同一个临界区里，否则归档完的旧 slot 还会被启动。
        let lifecycle = self.lock_lifecycle();
        let slot = self.lock_sessions().get(session_id).cloned();
        if let Some(slot) = slot {
            slot.conversation()
                .with_idle_writer(|| {
                    if slot.conversation().is_occupied() {
                        return Err(session_busy());
                    }
                    self.catalog.archive(session_id).map_err(catalog_error)
                })
                .map_err(conversation_error)??;
        } else {
            self.catalog.archive(session_id).map_err(catalog_error)?;
        }
        self.lock_sessions().remove(session_id);
        drop(lifecycle);
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn update_settings(&self, session_id: &str, selector: &str) -> Result<(), RpcError> {
        let slot = {
            let _lifecycle = self.lock_lifecycle();
            self.open_slot(session_id)?
        };
        slot.conversation()
            .update_settings(selector)
            .map_err(conversation_error)?;
        self.bump_and_emit_session(session_id, &slot);
        Ok(())
    }
}
