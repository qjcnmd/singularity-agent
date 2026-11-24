use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use agent::agent::UserInput;
use protocol::{RpcError, SessionChangedFilesResult, SessionFileDiffResult, SessionReadResult};

use super::{
    AppServer, WorkspaceRewindGuard, catalog_error, conversation_error, ensure_workspace_not_rewinding,
    session_busy,
};
use crate::runtime::{ContextPreview, Conversation, SnapshotTarget};

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
        let lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(&params.session_id)?;
        ensure_workspace_not_rewinding(&lifecycle, Path::new(&conversation.cwd))?;
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
        let _lifecycle = self.lock_lifecycle();
        let conversation = self.open_action_conversation(session_id)?;
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
                let _lifecycle = app.lock_lifecycle();
                let conversation = app.open_action_conversation(&id)?;
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
                app.hub.remove_session(&id);
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
                app.hub.remove_session(&id);
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

    pub(crate) fn changed_files(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
    ) -> Result<SessionChangedFilesResult, RpcError> {
        let target = self.snapshot_target(session_id, turn_id)?;
        let Some(base_tree) = target.base_tree else {
            return Ok(SessionChangedFilesResult { files: Vec::new() });
        };
        let files = agent::session::snapshot::diff_summary(&target.snapshot_dir, &target.cwd, &base_tree)
            .map_err(RpcError::new)?;
        Ok(SessionChangedFilesResult { files })
    }

    pub(crate) fn file_diff(
        &self,
        session_id: &str,
        path: &str,
        turn_id: Option<&str>,
    ) -> Result<SessionFileDiffResult, RpcError> {
        let target = self.snapshot_target(session_id, turn_id)?;
        let Some(base_tree) = target.base_tree else {
            return Ok(SessionFileDiffResult {
                path: path.to_owned(),
                output: "当前任务暂无文件快照。".into(),
            });
        };
        let rel_path = snapshot_relative_path(&target.cwd, path)?;
        let output =
            agent::session::snapshot::file_diff(&target.snapshot_dir, &target.cwd, &base_tree, &rel_path)
                .map_err(RpcError::new)?;
        Ok(SessionFileDiffResult { path: rel_path, output })
    }

    pub(crate) async fn rewind_session(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: &str,
        restore_files: bool,
    ) -> Result<SessionReadResult, RpcError> {
        let app = Arc::clone(self);
        let id = session_id.to_owned();
        let target_turn_id = turn_id.to_owned();
        let (conversation, guard, plan, _workspace_guard) = tokio::task::spawn_blocking(move || {
            let mut lifecycle = app.lock_lifecycle();
            let conversation = app.open_action_conversation(&id)?;
            let cwd = Path::new(&conversation.cwd);
            if restore_files {
                ensure_workspace_not_rewinding(&lifecycle, cwd)?;
                let target_key = app_core::workspace_key(cwd);
                let sibling_busy = app.lock_sessions().conversations().any(|(other_id, other)| {
                    other_id != id
                        && app_core::workspace_key(Path::new(&other.cwd)) == target_key
                        && other.has_active_execution()
                });
                if sibling_busy {
                    return Err(RpcError::new(
                        "当前项目还有其他正在运行的任务或后台命令，请等待结束后再还原文件。",
                    ));
                }
            }
            let (guard, plan) =
                conversation.begin_rewind(&target_turn_id, restore_files).map_err(conversation_error)?;
            let workspace_guard = plan
                .restore_tree
                .is_some()
                .then(|| WorkspaceRewindGuard::register(&app, &mut lifecycle, cwd));
            Ok::<_, RpcError>((conversation, guard, plan, workspace_guard))
        })
        .await
        .expect("rewind preparation completes while the runtime is running")?;

        let backup = if let Some(target_tree) = plan.restore_tree {
            let snapshot_dir = plan.snapshot_dir.clone();
            let cwd = plan.cwd.clone();
            let backup = tokio::task::spawn_blocking(move || {
                agent::session::snapshot::restore(&snapshot_dir, &cwd, &target_tree)
            })
            .await
            .expect("snapshot restore completes while the runtime is running")
            .map_err(RpcError::new)?;
            Some(backup)
        } else {
            None
        };

        let target_turn_id = turn_id.to_owned();
        let commit = tokio::task::spawn_blocking(move || conversation.commit_rewind(&target_turn_id, guard))
            .await
            .expect("rewind commit completes while the runtime is running");
        match commit {
            Ok(result) => Ok(result),
            Err(error) => {
                if let Some(backup) = backup
                    && let Err(rollback_error) = tokio::task::spawn_blocking(move || backup.rollback())
                        .await
                        .expect("snapshot rollback completes while the runtime is running")
                {
                    tracing::warn!(error = %rollback_error, "failed to roll back workspace after rewind commit failure");
                }
                Err(conversation_error(error))
            }
        }
    }

    fn snapshot_target(&self, session_id: &str, turn_id: Option<&str>) -> Result<SnapshotTarget, RpcError> {
        let conversation = self.lock_sessions().get(session_id);
        if let Some(conversation) = conversation {
            if conversation.is_deleting() {
                return Err(session_busy());
            }
            return Ok(conversation.snapshot_target(turn_id));
        }
        self.catalog.snapshot_target(session_id, turn_id).map_err(catalog_error)
    }
}

fn snapshot_relative_path(cwd: &Path, requested: &str) -> Result<String, RpcError> {
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
