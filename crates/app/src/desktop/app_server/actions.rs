use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent::agent::UserInput;
use protocol::{ChangedFile, RpcError, SessionFileDiffResult, SessionReadResult};

use super::{AppServer, SessionDeleteGuard, catalog_error, conversation_error};
use crate::desktop::git_diff;
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
        let (_, input) = self.parse_session_input(&params.session_id, params.message)?;
        let mut sessions = self.lock_sessions();
        let conversation = self.open_conversation(&mut sessions, &params.session_id, true)?;
        conversation.start(input).map_err(conversation_error)
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
        let mut sessions = self.lock_sessions();
        let conversation = self.open_conversation(&mut sessions, session_id, false)?;
        apply(&conversation)
    }

    pub(crate) async fn inspect_context(
        self: &Arc<Self>,
        session_id: &str,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Result<(), RpcError> {
        if shutdown.is_cancelled() {
            return Ok(());
        }
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let conversation = tokio::task::spawn_blocking(move || {
            let mut sessions = app.lock_sessions();
            app.open_conversation(&mut sessions, &id, true)
        })
        .await
        .expect("conversation lookup completes while the backend is running")?;
        conversation.inspect_context(shutdown).await.map_err(conversation_error)
    }

    pub(crate) fn compact(&self, session_id: &str) -> Result<(), RpcError> {
        let mut sessions = self.lock_sessions();
        let conversation = self.open_conversation(&mut sessions, session_id, true)?;
        conversation.compact().map_err(conversation_error)
    }

    pub(crate) fn rename_session(&self, session_id: &str, name: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| conversation.rename(name).map_err(conversation_error))
    }

    pub(crate) async fn delete_session(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let reservation = tokio::task::spawn_blocking(move || {
            let mut sessions = app.lock_sessions();
            SessionDeleteGuard::reserve(&app, &mut sessions, &id)
        })
        .await
        .expect("delete preparation completes while the backend is running")?;
        if let Some(conversation) = reservation.conversation() {
            conversation.stop().await.map_err(conversation_error)?;
        }
        let app = Arc::clone(self);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            app.catalog.delete(&id).map_err(catalog_error)?;
            {
                let mut sessions = app.lock_sessions();
                reservation.complete_locked(&mut sessions);
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
            let mut sessions = app.lock_sessions();
            Ok::<_, RpcError>(Arc::clone(app.open_conversation(&mut sessions, &id, false)?.bash_tasks()))
        })
        .await
        .expect("background task lookup completes while the backend is running")?;
        tasks.stop(task_id).await.map_err(RpcError::new)
    }

    pub(crate) fn changed_files(
        &self,
        params: protocol::SessionChangedFilesParams,
    ) -> Result<Vec<ChangedFile>, RpcError> {
        let workspace = self.workspace(&params.workspace_root)?;
        git_diff::changed_files(Path::new(&workspace.root), params.include_diffs).map_err(RpcError::new)
    }

    pub(crate) fn file_diff(
        &self,
        params: protocol::SessionFileDiffParams,
    ) -> Result<SessionFileDiffResult, RpcError> {
        let workspace = self.workspace(&params.workspace_root)?;
        let cwd = Path::new(&workspace.root);
        let rel_path = workspace_relative_path(cwd, &params.path)?;
        let output = git_diff::file_diff(cwd, &rel_path).map_err(RpcError::new)?;
        Ok(SessionFileDiffResult { path: rel_path, output })
    }

    pub(crate) fn rewind_session(
        &self,
        params: protocol::SessionRewindParams,
    ) -> Result<SessionReadResult, RpcError> {
        let (cwd, input) = self.parse_session_input(&params.session_id, params.message)?;
        app_core::load_agent_instructions(Path::new(&cwd), &self.home).map_err(RpcError::new)?;
        let mut sessions = self.lock_sessions();
        let conversation = self.open_conversation(&mut sessions, &params.session_id, true)?;
        conversation
            .rewind(&params.message_id, input)
            .map(|read| *read)
            .map_err(conversation_error)
    }

    fn parse_session_input(
        &self,
        session_id: &str,
        input: protocol::MessageInput,
    ) -> Result<(String, UserInput), RpcError> {
        let conversation = {
            let mut sessions = self.lock_sessions();
            self.open_conversation(&mut sessions, session_id, true)?
        };
        let skill_expansion =
            agent::agent::SkillExpansion::from_disabled_modules(&conversation.disabled_modules);
        let parsed = UserInput::from_uploads(
            input.text,
            input.images,
            input.files,
            input.annotations,
            Path::new(&conversation.cwd),
            &self.home,
            skill_expansion,
        )
        .map_err(RpcError::new)?;
        Ok((conversation.cwd.clone(), parsed))
    }
}

fn workspace_relative_path(cwd: &Path, requested: &str) -> Result<String, RpcError> {
    let trimmed = requested.trim();
    let target_path = Path::new(trimmed);
    let relative = if target_path.is_absolute() {
        let base_display = app_core::display_path(cwd);
        let target_display = app_core::display_path(target_path);
        let mut base_iter = Path::new(&base_display).components();
        let mut target_iter = Path::new(&target_display).components();
        loop {
            match (base_iter.next(), target_iter.next()) {
                (None, Some(first)) => {
                    let mut rel = PathBuf::from(first.as_os_str());
                    for rest in target_iter {
                        rel.push(rest.as_os_str());
                    }
                    break rel;
                }
                (Some(b), Some(t)) if same_component(b, t) => {}
                _ => return Err(RpcError::new(format!("文件不在当前项目目录内：{trimmed}"))),
            }
        }
    } else {
        target_path.to_path_buf()
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => return Err(RpcError::new(format!("无效的项目内相对路径：{trimmed}"))),
        }
    }
    if parts.is_empty() {
        return Err(RpcError::new("文件路径不能为空。"));
    }
    Ok(parts.join("/"))
}

fn same_component(left: Component<'_>, right: Component<'_>) -> bool {
    match (left, right) {
        (Component::RootDir, Component::RootDir) => true,
        (Component::Prefix(a), Component::Prefix(b)) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        (Component::Normal(a), Component::Normal(b)) => {
            a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        }
        _ => false,
    }
}
