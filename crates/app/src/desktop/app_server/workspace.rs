use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use protocol::{AppBootstrap, RedactedModelCatalog, RpcError, ThreadSummary, Workspace};

use super::{AppServer, catalog_error, ensure_workspace_not_rewinding};
use crate::desktop::workspace_store::WorkspaceError;

impl AppServer {
    pub(crate) fn bootstrap(&self) -> Result<AppBootstrap, RpcError> {
        let _publication = self.lock_app_publication();
        let catalog = self.models.redacted_catalog();
        self.bootstrap_with_catalog(catalog)
    }

    pub(super) fn bootstrap_with_catalog(
        &self,
        model_catalog: RedactedModelCatalog,
    ) -> Result<AppBootstrap, RpcError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let session_states: BTreeMap<_, _> = self
            .lock_sessions()
            .conversations()
            .map(|(id, conversation)| (id.to_owned(), conversation.status()))
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

    pub(crate) fn add_workspace(&self, root: &str) -> Result<Workspace, RpcError> {
        let workspace = self.workspaces.add(Path::new(root)).map_err(RpcError::new)?;
        self.publish_app_snapshot();
        Ok(workspace)
    }

    pub(crate) async fn remove_workspace(
        self: &std::sync::Arc<Self>,
        workspace_root: &str,
    ) -> Result<(), RpcError> {
        let app = std::sync::Arc::clone(self);
        let workspace_root = workspace_root.to_owned();
        let close = tokio::task::spawn_blocking(move || {
            let lifecycle = app.lock_lifecycle();
            let workspace = app.workspace(&workspace_root)?;
            ensure_workspace_not_rewinding(&lifecycle, Path::new(&workspace.root))?;
            let mut sessions = app.lock_sessions();
            let belongs = |conversation: &std::sync::Arc<crate::runtime::Conversation>| {
                app_core::same_saved_path(&conversation.cwd, &workspace.root)
            };
            if sessions.conversations().any(|(_, conversation)| {
                belongs(conversation) && (conversation.has_work() || conversation.is_deleting())
            }) {
                return Err(RpcError::new(format!(
                    "项目 {} 仍有运行中的任务、后台命令或待处理输入。",
                    workspace.root
                )));
            }
            app.workspaces.remove(&workspace.root).map_err(RpcError::new)?;
            sessions.retain(|id, conversation| {
                if belongs(conversation) {
                    conversation.mark_closing();
                    app.hub.remove_session(id);
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

    pub(crate) fn workspace(&self, workspace_root: &str) -> Result<Workspace, RpcError> {
        self.workspaces
            .find(workspace_root)
            .ok_or_else(|| RpcError::new(WorkspaceError::NotFound))
    }

    pub(crate) fn skills(&self, workspace_root: &str) -> Result<protocol::SkillCatalog, RpcError> {
        let root = self.workspace(workspace_root)?.root;
        let cwd = Path::new(&root);
        self.ensure_workspace_not_rewinding(cwd)?;
        let catalog = app_core::skills::SkillCatalog::discover(cwd, &self.home);
        Ok(protocol::SkillCatalog {
            skills: catalog
                .skills
                .into_iter()
                .filter(|skill| skill.user_invocable)
                .map(|skill| protocol::SkillMetadata {
                    name: skill.name,
                    description: skill.description,
                    path: app_core::display_path(&skill.path),
                })
                .collect(),
            diagnostics: catalog.diagnostics,
        })
    }

    pub(crate) fn context_preview(
        &self,
        workspace_root: &str,
    ) -> Result<Vec<protocol::ContextModule>, RpcError> {
        let root = self.workspace(workspace_root)?.root;
        let cwd = Path::new(&root);
        self.ensure_workspace_not_rewinding(cwd)?;
        let instructions = app_core::load_agent_instructions(cwd, &self.home).map_err(RpcError::new)?;
        let skills = app_core::skills::SkillCatalog::discover(cwd, &self.home);
        let servers = self.mcp.list(cwd).map_err(RpcError::new)?;
        let web = self.models.web_search_access().map(std::sync::Arc::new);
        Ok(agent::agent::modules::preview(&root, instructions, skills, servers, web))
    }

    pub(crate) fn read_instructions(
        &self,
        workspace_root: Option<&str>,
    ) -> Result<protocol::TextDocument, RpcError> {
        let dir = self.scope_directory(workspace_root)?;
        if workspace_root.is_some() {
            self.ensure_workspace_not_rewinding(&dir)?;
        }
        let path = dir.join("AGENTS.md");
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(RpcError::new(format!("{}: {error}", path.display()))),
        };
        Ok(protocol::TextDocument {
            path: app_core::display_path(&path),
            content,
        })
    }

    pub(crate) fn save_instructions(
        &self,
        workspace_root: Option<&str>,
        content: &str,
    ) -> Result<protocol::TextDocument, RpcError> {
        let (_lifecycle, dir) = match workspace_root {
            Some(workspace_root) => {
                let lifecycle = self.lock_lifecycle();
                let dir = PathBuf::from(self.workspace(workspace_root)?.root);
                ensure_workspace_not_rewinding(&lifecycle, &dir)?;
                (Some(lifecycle), dir)
            }
            None => (None, self.home.clone()),
        };
        let path = dir.join("AGENTS.md");
        app_core::write_bytes_atomic(&path, content.as_bytes())
            .map_err(|error| RpcError::new(format!("{}: {error}", path.display())))?;
        Ok(protocol::TextDocument {
            path: app_core::display_path(&path),
            content: content.to_owned(),
        })
    }

    pub(crate) fn read_text_file(&self, cwd: &str, path: &str) -> Result<protocol::TextDocument, RpcError> {
        let resolved = crate::desktop::file_view::resolve_path(cwd, path);
        let lifecycle = self.lock_lifecycle();
        ensure_workspace_not_rewinding(&lifecycle, Path::new(cwd))?;
        ensure_workspace_not_rewinding(&lifecycle, &resolved)?;
        let canonical = std::fs::canonicalize(&resolved).ok();
        let target = if let Some(canonical) = canonical.as_deref() {
            ensure_workspace_not_rewinding(&lifecycle, canonical)?;
            canonical
        } else {
            if let Some(hint) = canonical_ancestor_hint(&resolved) {
                ensure_workspace_not_rewinding(&lifecycle, &hint)?;
            }
            &resolved
        };
        let mut document = crate::desktop::file_view::read_text_file(target)?;
        document.path = app_core::display_path(&resolved);
        Ok(document)
    }
}

fn canonical_ancestor_hint(path: &Path) -> Option<PathBuf> {
    for ancestor in path.ancestors().skip(1) {
        if let Ok(canonical) = std::fs::canonicalize(ancestor)
            && let Ok(tail) = path.strip_prefix(ancestor)
        {
            return Some(canonical.join(tail));
        }
    }
    None
}

fn group_threads(
    workspaces: &[Workspace],
    threads: Vec<ThreadSummary>,
) -> BTreeMap<String, Vec<ThreadSummary>> {
    let mut grouped: BTreeMap<String, Vec<ThreadSummary>> = workspaces
        .iter()
        .map(|workspace| (app_core::saved_path_key(&workspace.root), Vec::new()))
        .collect();
    for thread in threads {
        if let Some(bucket) = grouped.get_mut(&app_core::saved_path_key(&thread.cwd)) {
            bucket.push(thread);
        }
    }
    grouped
}
