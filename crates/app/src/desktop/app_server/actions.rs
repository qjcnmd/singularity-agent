use super::*;

#[cfg(test)]
mod tests;

impl AppServer {
    pub fn image_data(&self, session_id: &str, image_id: &str) -> Result<String, RpcError> {
        let slot = self.lock_sessions().get(session_id).cloned();
        if let Some(data) = slot.and_then(|slot| slot.conversation().pending_image(image_id)) {
            return Ok(data);
        }
        self.catalog.image_data(session_id, image_id).map_err(catalog_error)
    }

    /// 输入直接进入会话队列；启动和收尾期间同样接受，按顺序执行。
    pub fn submit(self: &Arc<Self>, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        let mut state = slot.lock_state();
        let reservation = slot.conversation().submit(&input).map_err(conversation_error)?;
        if reservation.is_some() {
            state.begin_turn(slot.conversation().usage());
        }
        self.publish_session_locked(session_id, &slot, &mut state);
        drop(state);
        if let Some(reservation) = reservation {
            self.spawn_operation(session_id, slot, reservation);
        }
        Ok(())
    }

    pub fn answer_question(
        &self,
        session_id: &str,
        item_id: &str,
        answers: Vec<singularity_protocol::UserQuestionAnswer>,
    ) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| {
            conversation.answer_question(item_id, answers).map_err(invalid_request)
        })
    }

    pub fn abort(&self, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| conversation.abort().map_err(conversation_error))
    }

    /// 会话查找和控制接受共用生命周期锁，公开投影和发布共用 SlotState 顺序。
    /// 闭包里只做 Conversation 的短控制操作，不覆盖 Agent 执行或事件 sink。
    fn apply_control(
        &self,
        session_id: &str,
        apply: impl FnOnce(&Conversation) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        let mut state = slot.lock_state();
        apply(slot.conversation())?;
        self.publish_session_locked(session_id, &slot, &mut state);
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
            let mut state = slot.lock_state();
            if slot.conversation().is_occupied() {
                return Err(session_busy());
            }
            // 与提交共用锁顺序，固定执行前快照。
            let reservation = slot.conversation().reserve_compaction().map_err(conversation_error)?;
            state.begin_turn(slot.conversation().usage());
            self.publish_session_locked(session_id, &slot, &mut state);
            drop(state);
            (slot, reservation)
        };
        self.spawn_operation(session_id, slot, reservation);
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
    pub async fn archive_session(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let pending = tokio::task::spawn_blocking(move || {
            let _lifecycle = app.lock_lifecycle();
            let slot = app.lock_sessions().get(&id).cloned();
            match slot {
                Some(slot) => {
                    if slot.is_archiving() {
                        return Err(session_busy());
                    }
                    let archive = slot.begin_archive();
                    Ok(Some((slot, archive)))
                }
                None => {
                    // 未加载的任务直接归档；移动前保持生命周期锁，阻止新建 slot。
                    app.catalog.archive(&id).map_err(catalog_error)?;
                    Ok(None)
                }
            }
        })
        .await
        .expect("archive preparation completes while the backend is running")?;
        if let Some((slot, _)) = &pending {
            self.stop_sessions(std::slice::from_ref(slot)).await?;
        }
        let app = Arc::clone(self);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some((_slot, _archive)) = pending {
                let _lifecycle = app.lock_lifecycle();
                app.catalog.archive(&id).map_err(catalog_error)?;
                app.lock_sessions().remove(&id);
            }
            app.publish_app_snapshot();
            Ok(())
        })
        .await
        .expect("archive completion finishes while the backend is running")
    }

    pub fn update_settings(&self, session_id: &str, selector: &str) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let slot = self.open_action_slot(session_id)?;
        if let Some(content) = slot.conversation().update_settings(selector).map_err(conversation_error)? {
            self.on_turn_event(session_id, &slot, TurnEvent::SettingsUpdated { turn_id: None, content });
        }
        self.bump_and_emit_session(session_id, &slot);
        Ok(())
    }
}
