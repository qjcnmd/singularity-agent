use super::*;

impl AppServer {
    pub fn image_data(&self, session_id: &str, image_id: &str) -> Result<String, RpcError> {
        let conversation = self.lock_sessions().get(session_id).cloned();
        if let Some(conversation) = conversation {
            return conversation.image_data(image_id).map_err(catalog_error);
        }
        self.catalog.image_data(session_id, image_id).map_err(catalog_error)
    }

    /// 输入直接进入会话队列；启动和收尾期间同样接受，按顺序执行。
    pub fn submit(self: &Arc<Self>, session_id: &str, input: UserInput) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(session_id)?;
        conversation.start(input).map_err(conversation_error)?;
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

    /// 在生命周期锁内查找任务并执行一个短操作（回答问题、停止）；
    /// 闭包里不能等待 Agent 执行。
    fn apply_control(
        &self,
        session_id: &str,
        apply: impl FnOnce(&Conversation) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(session_id)?;
        apply(&conversation)?;
        Ok(())
    }

    /// 打开任务时查看下一轮会加载的指令和工具；结果通过 context/loaded 事件送达。
    pub async fn inspect_context(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let conversation = tokio::task::spawn_blocking(move || {
            let _lifecycle = app.lock_lifecycle();
            app.open_conversation(&id)
        })
        .await
        .expect("conversation lookup completes while the backend is running")?;
        conversation.inspect_context().await.map_err(internal_error)
    }

    pub fn compact(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        self.open_action_conversation(session_id)?.compact().map_err(conversation_error)
    }

    /// 保存任务展示名称；活动执行继续使用当前写者。
    pub fn rename_session(&self, session_id: &str, name: &str) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(session_id)?;
        conversation.rename(name).map_err(conversation_error)?;
        Ok(())
    }

    /// 停止目标任务并等它结束，再删除日志和附件；等待期间只拒绝该任务的新操作。
    pub async fn delete_session(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let pending = tokio::task::spawn_blocking(move || {
            let _lifecycle = app.lock_lifecycle();
            let conversation = app.lock_sessions().get(&id).cloned();
            match conversation {
                Some(conversation) => {
                    let deletion = conversation.begin_delete().map_err(conversation_error)?;
                    Ok(Some((conversation, deletion)))
                }
                None => {
                    // 未加载的任务直接删除；删除前保持生命周期锁，阻止新建 conversation。
                    app.catalog.delete(&id).map_err(catalog_error)?;
                    Ok(None)
                }
            }
        })
        .await
        .expect("delete preparation completes while the backend is running")?;
        if let Some((conversation, _)) = &pending {
            self.stop_sessions(std::slice::from_ref(conversation)).await?;
        }
        let app = Arc::clone(self);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some((_conversation, deletion)) = pending {
                let _lifecycle = app.lock_lifecycle();
                app.catalog.delete(&id).map_err(catalog_error)?;
                app.lock_sessions().remove(&id);
                deletion.complete();
            }
            app.publish_app_snapshot();
            Ok(())
        })
        .await
        .expect("delete completion finishes while the backend is running")
    }

    pub fn update_settings(
        &self,
        session_id: &str,
        selection: &singularity_protocol::ModelSelection,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(session_id)?;
        conversation.update_settings(selection).map_err(conversation_error)?;
        Ok(())
    }
}

impl AppServer {
    pub async fn stop_background_task(&self, session_id: &str, task_id: &str) -> Result<(), RpcError> {
        let tasks = {
            let _lifecycle = self.lock_lifecycle();
            Arc::clone(self.open_action_conversation(session_id)?.bash_tasks())
        };
        tasks.stop(task_id).await.map(|_| ()).map_err(invalid_request)
    }
}
