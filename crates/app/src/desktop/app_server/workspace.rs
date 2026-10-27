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
        let sessions_by_workspace = group_threads(&workspaces, threads);
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

    pub async fn remove_workspace(self: &std::sync::Arc<Self>, workspace_root: &str) -> Result<(), RpcError> {
        let app = std::sync::Arc::clone(self);
        let workspace_root = workspace_root.to_owned();
        let close = tokio::task::spawn_blocking(move || {
            let lifecycle = app.lock_lifecycle();
            let workspace = app.workspace(&workspace_root)?;
            let mut sessions = app.lock_sessions();
            let belongs = |conversation: &std::sync::Arc<crate::runtime::Conversation>| {
                singularity_core::same_saved_path(&conversation.thread().cwd, &workspace.root)
            };
            if sessions.values().any(|conversation| belongs(conversation) && conversation.has_work()) {
                return Err(RpcError::new(
                    singularity_protocol::RpcErrorCode::WorkspaceBusy,
                    format!("项目 {} 仍有运行中的任务、后台命令或待处理输入。", workspace.root),
                ));
            }
            app.workspaces.remove(&workspace_root).map_err(workspace_error)?;
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

    pub fn workspace(&self, workspace_root: &str) -> Result<Workspace, RpcError> {
        self.workspaces
            .find(workspace_root)
            .ok_or_else(|| workspace_error(WorkspaceError::NotFound))
    }

    pub fn skills(&self, workspace_root: &str) -> Result<singularity_protocol::SkillCatalog, RpcError> {
        let root = self.workspace(workspace_root)?.root;
        let catalog = singularity_core::skills::SkillCatalog::discover(Path::new(&root), &self.home);
        Ok(singularity_protocol::SkillCatalog {
            skills: catalog
                .skills
                .into_iter()
                .filter(|skill| skill.user_invocable)
                .map(|skill| singularity_protocol::SkillMetadata {
                    name: skill.name,
                    description: skill.description,
                    path: singularity_core::display_path(&skill.path),
                })
                .collect(),
            diagnostics: catalog.diagnostics,
        })
    }

    /// 全局指令是应用数据目录下的 AGENTS.md，项目指令是项目根目录下的 AGENTS.md；文件不存在时内容为空。
    pub fn read_instructions(
        &self,
        workspace_root: Option<&str>,
    ) -> Result<singularity_protocol::TextDocument, RpcError> {
        read_document(&self.instructions_path(workspace_root)?)
    }

    /// 保存后在下一轮任务开始时生效；先写临时文件再替换，读取方不会看到写了一半的内容。
    pub fn save_instructions(
        &self,
        workspace_root: Option<&str>,
        content: &str,
    ) -> Result<singularity_protocol::TextDocument, RpcError> {
        let path = self.instructions_path(workspace_root)?;
        let temporary = path.with_extension(format!("md.{}.tmp", uuid::Uuid::now_v7()));
        std::fs::write(&temporary, content)
            .and_then(|()| std::fs::rename(&temporary, &path))
            .map_err(|error| {
                let _ = std::fs::remove_file(&temporary);
                internal_error(format!("{}: {error}", path.display()))
            })?;
        Ok(singularity_protocol::TextDocument {
            path: singularity_core::display_path(&path),
            content: content.to_owned(),
        })
    }

    fn instructions_path(&self, workspace_root: Option<&str>) -> Result<std::path::PathBuf, RpcError> {
        let directory = match workspace_root {
            Some(root) => std::path::PathBuf::from(self.workspace(root)?.root),
            None => self.home.clone(),
        };
        Ok(directory.join("AGENTS.md"))
    }

    /// 搜索指定项目目录内的文件候选（最多返回 12 条）。
    pub fn file_search(
        &self,
        workspace_root: &str,
        query: &str,
    ) -> Result<Vec<singularity_protocol::FileCandidate>, RpcError> {
        let root = self.workspace(workspace_root)?.root;
        workspace_files::search_files(&root, query).map_err(invalid_request)
    }
}

fn read_document(path: &Path) -> Result<singularity_protocol::TextDocument, RpcError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(internal_error(format!("{}: {error}", path.display()))),
    };
    Ok(singularity_protocol::TextDocument {
        path: singularity_core::display_path(path),
        content,
    })
}

/// 按会话日志里记录的工作目录把任务分到已登记的项目；项目登记不保存任务列表，
/// 每次读取都重新分组。不属于任何项目的任务不显示。
fn group_threads(
    workspaces: &[Workspace],
    threads: Vec<ThreadSummary>,
) -> BTreeMap<String, Vec<ThreadSummary>> {
    let mut grouped: BTreeMap<String, Vec<ThreadSummary>> =
        workspaces.iter().map(|workspace| (workspace.root.to_lowercase(), Vec::new())).collect();
    for thread in threads {
        if let Some(bucket) = grouped.get_mut(&thread.cwd.to_lowercase()) {
            bucket.push(thread);
        }
    }
    grouped
}
