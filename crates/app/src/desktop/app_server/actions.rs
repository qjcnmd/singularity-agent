use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent::agent::UserInput;
use protocol::{RpcError, SessionChangedFilesResult, SessionFileDiffResult, SessionReadResult};

use super::{AppServer, SessionDeleteGuard, catalog_error, conversation_error, session_busy};
use crate::desktop::git_diff;
use crate::runtime::{ContextPreview, Conversation};

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
            let lifecycle = self.lock_lifecycle();
            let conversation = self.open_workspace_conversation(&lifecycle, &params.session_id)?;
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
        let lifecycle = self.lock_lifecycle();
        let conversation = self.open_workspace_conversation(&lifecycle, &params.session_id)?;
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
        let lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(&lifecycle, session_id)?;
        apply(&conversation)
    }

    pub(crate) async fn inspect_context(
        self: &Arc<Self>,
        session_id: &str,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Result<(), RpcError> {
        let signal = shutdown.child_token();
        loop {
            if signal.is_cancelled() {
                return Ok(());
            }
            let app = Arc::clone(self);
            let id = session_id.to_string();
            let preview_signal = signal.clone();
            let (conversation, attempt) = tokio::task::spawn_blocking(move || {
                let lifecycle = app.lock_lifecycle();
                let conversation = app.open_workspace_conversation(&lifecycle, &id)?;
                let attempt = conversation.begin_context_preview(&preview_signal);
                Ok::<_, RpcError>((conversation, attempt))
            })
            .await
            .expect("conversation lookup completes while the backend is running")?;
            match attempt {
                ContextPreview::Skip => return Ok(()),
                ContextPreview::Busy => conversation.wait_context_preview(&signal).await,
                ContextPreview::Run(guard) => {
                    return conversation.inspect_context(signal, guard).await.map_err(conversation_error);
                }
            }
        }
    }

    pub(crate) fn compact(&self, session_id: &str) -> Result<(), RpcError> {
        let lifecycle = self.lock_lifecycle();
        let conversation = self.open_workspace_conversation(&lifecycle, session_id)?;
        conversation.compact().map_err(conversation_error)
    }

    pub(crate) fn rename_session(&self, session_id: &str, name: &str) -> Result<(), RpcError> {
        self.apply_control(session_id, |conversation| conversation.rename(name).map_err(conversation_error))
    }

    pub(crate) async fn delete_session(self: &Arc<Self>, session_id: &str) -> Result<(), RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_string();
        let (open_session, reservation) = tokio::task::spawn_blocking(move || {
            let mut lifecycle = app.lock_lifecycle();
            if lifecycle.is_deleting(&id) {
                return Err(session_busy());
            }
            let open_session = if let Some(conversation) = app.lock_sessions().get(&id) {
                let deletion = conversation.begin_delete().map_err(conversation_error)?;
                Some((conversation, deletion))
            } else {
                None
            };
            let reservation = SessionDeleteGuard::reserve(&app, &mut lifecycle, &id)?;
            Ok((open_session, reservation))
        })
        .await
        .expect("delete preparation completes while the backend is running")?;
        if let Some((conversation, _)) = &open_session {
            conversation.stop().await.map_err(conversation_error)?;
        }
        let app = Arc::clone(self);
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            app.catalog.delete(&id).map_err(catalog_error)?;
            {
                let mut lifecycle = app.lock_lifecycle();
                if let Some((_, deletion)) = open_session {
                    app.lock_sessions().remove(&id);
                    deletion.complete();
                }
                app.hub.remove_session(&id);
                reservation.release_locked(&mut lifecycle);
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
            let lifecycle = app.lock_lifecycle();
            Ok::<_, RpcError>(Arc::clone(app.open_action_conversation(&lifecycle, &id)?.bash_tasks()))
        })
        .await
        .expect("background task lookup completes while the backend is running")?;
        tasks.stop(task_id).await.map_err(RpcError::new)
    }

    pub(crate) fn changed_files(
        &self,
        params: protocol::SessionChangedFilesParams,
    ) -> Result<SessionChangedFilesResult, RpcError> {
        let cwd = self.resolve_diff_cwd(params.session_id.as_deref(), params.workspace_root.as_deref())?;
        let files = git_diff::changed_files(&cwd, params.include_diffs).map_err(RpcError::new)?;
        Ok(SessionChangedFilesResult { files })
    }

    pub(crate) fn file_diff(
        &self,
        params: protocol::SessionFileDiffParams,
    ) -> Result<SessionFileDiffResult, RpcError> {
        let cwd = self.resolve_diff_cwd(params.session_id.as_deref(), params.workspace_root.as_deref())?;
        let rel_path = workspace_relative_path(&cwd, &params.path)?;
        let output = git_diff::file_diff(&cwd, &rel_path).map_err(RpcError::new)?;
        Ok(SessionFileDiffResult { path: rel_path, output })
    }

    pub(crate) fn rewind_session(
        &self,
        params: protocol::SessionRewindParams,
    ) -> Result<SessionReadResult, RpcError> {
        let resubmit = if let Some(input) = params.resubmit {
            let conversation = {
                let lifecycle = self.lock_lifecycle();
                self.open_workspace_conversation(&lifecycle, &params.session_id)?
            };
            let skill_expansion =
                agent::agent::SkillExpansion::from_disabled_modules(&conversation.disabled_modules);
            let parsed_input = UserInput::from_uploads(
                input.text,
                input.images,
                input.files,
                input.annotations,
                Path::new(&conversation.cwd),
                &self.home,
                skill_expansion,
            )
            .map_err(RpcError::new)?;
            let (selection, context) = conversation.prepare_resubmit_context().map_err(conversation_error)?;
            Some((parsed_input, selection, context))
        } else {
            None
        };
        let lifecycle = self.lock_lifecycle();
        let conversation = self.open_workspace_conversation(&lifecycle, &params.session_id)?;
        conversation
            .rewind(&params.message_id, resubmit)
            .map(|read| *read)
            .map_err(conversation_error)
    }

    fn resolve_diff_cwd(
        &self,
        session_id: Option<&str>,
        workspace_root: Option<&str>,
    ) -> Result<PathBuf, RpcError> {
        if let Some(root) = workspace_root {
            return Ok(PathBuf::from(self.workspace(root)?.root));
        }
        if let Some(id) = session_id {
            return self.session_cwd(id);
        }
        Err(RpcError::new("缺少项目路径或会话标识。"))
    }

    fn session_cwd(&self, session_id: &str) -> Result<PathBuf, RpcError> {
        let lifecycle = self.lock_lifecycle();
        if lifecycle.is_deleting(session_id) {
            return Err(session_busy());
        }
        if let Some(conversation) = self.lock_sessions().get(session_id) {
            if conversation.is_deleting() {
                return Err(session_busy());
            }
            return Ok(PathBuf::from(&conversation.cwd));
        }
        drop(lifecycle);
        self.catalog.session_cwd(session_id).map_err(catalog_error)
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
