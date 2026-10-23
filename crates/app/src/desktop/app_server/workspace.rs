//! 项目登记、按项目分组任务，以及移除项目时释放相关资源。

use std::collections::BTreeMap;
use std::path::Path;

use singularity_protocol::{AppBootstrap, RedactedModelCatalog, RpcError, ThreadSummary, Workspace};

use super::{AppServer, catalog_error, internal_error, invalid_request, workspace_error};
use crate::desktop::workspace_files;
use crate::desktop::workspace_store::WorkspaceError;

impl AppServer {
    pub fn bootstrap(&self) -> Result<AppBootstrap, RpcError> {
        let _publication = self.lock_app_publication();
        let catalog = self.models.redacted_catalog();
        self.bootstrap_with_catalog(catalog)
    }

    /// 调用方持有发布锁，读取配置、生成快照和取得事件序号不会与保存配置交错。
    pub(super) fn bootstrap_with_catalog(
        &self,
        model_catalog: RedactedModelCatalog,
    ) -> Result<AppBootstrap, RpcError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let session_states: BTreeMap<_, _> = self
            .lock_sessions()
            .iter()
            .map(|(id, conversation)| (id.clone(), conversation.status()))
            .collect();
        let sessions_by_workspace = group_threads(&workspaces, threads).map_err(internal_error)?;
        Ok(AppBootstrap {
            user_home: std::env::home_dir().and_then(|home| home.into_os_string().into_string().ok()),
            session_states,
            revision,
            workspaces,
            sessions_by_workspace,
            model_catalog,
        })
    }

    pub fn add_workspace(&self, root: &str) -> Result<Workspace, RpcError> {
        let workspace = self.workspaces.add(Path::new(root)).map_err(workspace_error)?;
        self.publish_app_snapshot();
        Ok(workspace)
    }

    pub async fn remove_workspace(self: &std::sync::Arc<Self>, workspace_id: &str) -> Result<(), RpcError> {
        let app = std::sync::Arc::clone(self);
        let workspace_id = workspace_id.to_owned();
        let close = tokio::task::spawn_blocking(move || {
            let lifecycle = app.lock_lifecycle();
            let workspace = app.workspace(&workspace_id)?;
            let mut sessions = app.lock_sessions();
            let belongs = |conversation: &std::sync::Arc<crate::runtime::Conversation>| {
                matches!(
                    singularity_core::saved_directory_matches(&workspace.root, &conversation.thread().cwd),
                    Ok(true)
                )
            };
            if sessions.values().any(|conversation| belongs(conversation) && conversation.has_work()) {
                return Err(RpcError::new(
                    singularity_protocol::RpcErrorCode::WorkspaceBusy,
                    format!("项目 {} 仍有运行中的任务、后台命令或待处理输入。", workspace.root),
                    "等待处理完成，或停止后台命令并删除相关任务后重试。",
                ));
            }
            app.workspaces.remove(&workspace_id).map_err(workspace_error)?;
            sessions.retain(|_, conversation| {
                if belongs(conversation) {
                    conversation.set_closing(true);
                    false
                } else {
                    true
                }
            });
            drop(sessions);
            let close = app.mcp.disconnect_directory(Path::new(&workspace.root));
            drop(lifecycle);
            app.publish_app_snapshot();
            Ok(close)
        })
        .await
        .expect("workspace removal worker completes")?;
        close.await;
        Ok(())
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Workspace, RpcError> {
        self.workspaces
            .find(workspace_id)
            .ok_or_else(|| workspace_error(WorkspaceError::NotFound))
    }

    pub fn skills(&self, workspace_id: &str) -> Result<singularity_protocol::SkillCatalog, RpcError> {
        let root = self.workspace(workspace_id)?.root;
        let catalog = singularity_core::skills::SkillCatalog::discover(Path::new(&root), &self.home);
        Ok(singularity_protocol::SkillCatalog {
            skills: catalog
                .skills
                .into_iter()
                .filter(|skill| skill.user_invocable)
                .map(|skill| singularity_protocol::SkillMetadata {
                    name: skill.name,
                    description: skill.description,
                })
                .collect(),
            diagnostics: catalog.diagnostics,
        })
    }

    /// 查询已登记工作区内的文件候选；候选数量由补全界面的调用者决定。
    pub fn file_search(
        &self,
        workspace_id: &str,
        query: &str,
    ) -> Result<Vec<singularity_protocol::FileCandidate>, RpcError> {
        let root = self.workspace(workspace_id)?.root;
        workspace_files::search_files(&root, query).map_err(invalid_request)
    }
}

/// 按会话日志里记录的工作目录把任务分到已登记的项目；项目登记不保存任务列表，
/// 每次读取都重新分组。不属于任何项目的任务不显示。
fn group_threads(
    workspaces: &[Workspace],
    threads: Vec<ThreadSummary>,
) -> Result<BTreeMap<String, Vec<ThreadSummary>>, String> {
    let mut grouped: BTreeMap<String, (singularity_core::CanonicalWorkspacePath, Vec<ThreadSummary>)> =
        workspaces
            .iter()
            .map(|workspace| {
                singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
                    .map(|identity| (workspace.workspace_id.clone(), (identity, Vec::new())))
            })
            .collect::<Result<_, _>>()?;
    for thread in threads {
        let identity = singularity_core::CanonicalWorkspacePath::from_saved(&thread.cwd)?;
        if let Some((_, bucket)) = grouped.values_mut().find(|(workspace, _)| workspace == &identity) {
            bucket.push(thread);
        }
    }
    Ok(grouped.into_iter().map(|(workspace_id, (_, threads))| (workspace_id, threads)).collect())
}
