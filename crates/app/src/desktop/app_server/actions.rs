use std::sync::Arc;

use agent::agent::UserInput;
use protocol::RpcError;

use super::{AppServer, catalog_error, conversation_error};
use crate::runtime::Conversation;

impl AppServer {
    pub(crate) fn image_data(&self, session_id: &str, image_id: &str) -> Result<String, RpcError> {
        let conversation = self.lock_sessions().get(session_id);
        if let Some(conversation) = conversation {
            return conversation.image_data(image_id).map_err(catalog_error);
        }
        self.catalog.image_data(session_id, image_id).map_err(catalog_error)
    }

    pub(crate) fn submit(&self, params: protocol::SessionTextParams) -> Result<(), RpcError> {
        let (cwd, skill_expansion) = {
            let _lifecycle = self.lock_lifecycle();
            let conversation = self.open_action_conversation(&params.session_id)?;
            let skill_expansion =
                agent::agent::SkillExpansion::from_disabled_modules(&conversation.disabled_modules);
            (conversation.cwd.clone(), skill_expansion)
        };
        let input = UserInput::from_uploads(
            params.text,
            params.images,
            params.files,
            params.annotations,
            std::path::Path::new(&cwd),
            &self.home,
            skill_expansion,
        )
        .map_err(RpcError::new)?;
        self.apply_control(&params.session_id, |conversation| {
            conversation.start(input).map_err(conversation_error)
        })
    }

    pub(crate) fn answer_question(
        &self,
        session_id: &str,
        item_id: &str,
        answers: Vec<protocol::UserQuestionAnswer>,
    ) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| {
            conversation.answer_question(item_id, answers).map_err(conversation_error)
        })
    }

    pub(crate) fn abort(&self, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| conversation.abort().map_err(conversation_error))
    }

    fn apply_control(
        &self,
        session_id: &str,
        apply: impl FnOnce(&Arc<Conversation>) -> Result<(), RpcError>,
    ) -> Result<(), RpcError> {
        let _lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(session_id)?;
        apply(&conversation)
    }

    pub(crate) async fn inspect_context(
        self: &Arc<Self>,
        session_id: &str,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let signal = shutdown.child_token();
        let preview_signal = signal.clone();
        let pending = tokio::task::spawn_blocking(move || {
            let _lifecycle = app.lock_lifecycle();
            let conversation = app.open_action_conversation(&id)?;
            Ok::<_, RpcError>(
                conversation.begin_context_preview(&preview_signal).map(|guard| (conversation, guard)),
            )
        })
        .await
        .expect("conversation lookup completes while the backend is running")?;
        let Some((conversation, guard)) = pending else {
            return Ok(());
        };
        conversation.inspect_context(signal, guard).await.map_err(conversation_error)
    }

    pub(crate) fn compact(&self, session_id: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| conversation.compact().map_err(conversation_error))
    }

    pub(crate) fn rename_session(&self, session_id: &str, name: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| conversation.rename(name).map_err(conversation_error))
    }

    pub(crate) async fn delete_session(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let pending = tokio::task::spawn_blocking(move || {
            let lifecycle = app.lock_lifecycle();
            let Some(conversation) = app.lock_sessions().get(&id) else {
                app.catalog.delete(&id).map_err(catalog_error)?;
                drop(lifecycle);
                app.publish_app_snapshot();
                return Ok(None);
            };
            let deletion = conversation.begin_delete().map_err(conversation_error)?;
            Ok(Some((conversation, deletion)))
        })
        .await
        .expect("delete preparation completes while the backend is running")?;
        let Some((conversation, deletion)) = pending else {
            return Ok(());
        };
        conversation.stop().await.map_err(conversation_error)?;
        let app = Arc::clone(self);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            {
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

    pub(crate) fn update_settings(
        &self,
        session_id: &str,
        selection: &protocol::ModelSelection,
    ) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| {
            conversation.update_settings(selection).map_err(conversation_error)
        })
    }

    pub(crate) async fn stop_background_task(
        self: &Arc<Self>,
        session_id: &str,
        task_id: &str,
    ) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_owned();
        let tasks = tokio::task::spawn_blocking(move || {
            let _lifecycle = app.lock_lifecycle();
            Ok::<_, RpcError>(Arc::clone(app.open_action_conversation(&id)?.bash_tasks()))
        })
        .await
        .expect("background task lookup completes while the backend is running")?;
        tasks.stop(task_id).await.map_err(RpcError::new)
    }
}
