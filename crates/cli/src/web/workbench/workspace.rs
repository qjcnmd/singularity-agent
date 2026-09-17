//! 工作台的工作区与目录边界：项目登记、会话目录查询与分组投影。
//!
//! 这里回答的都是「任务属于哪个项目、目录在哪、如何分组、有哪些候选」，
//! 不进入会话执行编排；会话生命周期与事件归并留在父模块。
//! 发布入口与错误映射仍由父模块统一持有，本模块只调用它们。

use std::collections::BTreeMap;
use std::path::Path;

use singularity_protocol::{
    RedactedModelCatalog, RpcError, ThreadSummary, WorkbenchBootstrap, Workspace,
};
use singularity_runtime::WorkspaceError;

use super::{
    Workbench, catalog_error, internal_error, invalid_request, session_occupied,
    session_scope_conflict, workspace_error,
};
use crate::web::workspace_files;

impl Workbench {
    pub fn bootstrap(&self) -> Result<WorkbenchBootstrap, RpcError> {
        // 目录读取在独立短作用域内完成：模型锁不得带入会话锁与页面发布。
        let catalog = {
            let models = self.lock_models();
            models.redacted_catalog()
        };
        self.bootstrap_with_catalog(catalog)
    }

    pub(super) fn bootstrap_with_catalog(
        &self,
        model_catalog: RedactedModelCatalog,
    ) -> Result<WorkbenchBootstrap, RpcError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        // 当前任务目录以 catalog 为唯一权威：冻结历史只服务于执行内容恢复，
        // 不再回填目录摘要。阶段读取只问 Conversation，不取 Slot 状态锁。
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let session_phases = self
            .lock_sessions()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.conversation().phase()))
            .collect();
        let sessions_by_workspace = group_threads(&workspaces, &threads).map_err(internal_error)?;
        Ok(WorkbenchBootstrap {
            session_phases,
            generation: self.generation.clone(),
            revision,
            workspaces,
            sessions_by_workspace,
            model_catalog,
        })
    }

    pub fn add_workspace(&self, root: &str) -> Result<Workspace, RpcError> {
        let workspace = self
            .workspaces
            .add(Path::new(root))
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(workspace)
    }

    pub fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<(), RpcError> {
        self.workspaces
            .rename(workspace_id, name)
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(())
    }

    pub fn remove_workspace(&self, workspace_id: &str) -> Result<(), RpcError> {
        let workspace = self.workspace(workspace_id)?;
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let grouped =
            group_threads(std::slice::from_ref(&workspace), &threads).map_err(internal_error)?;
        for thread in grouped.get(workspace_id).into_iter().flatten() {
            let slot = self.lock_sessions().get(&thread.thread_id).cloned();
            let busy = slot.is_some_and(|slot| session_occupied(slot.conversation()));
            if busy {
                return Err(RpcError::new(
                    singularity_protocol::RpcErrorCode::WorkspaceBusy,
                    format!("项目 {} 仍有活动任务或待处理输入。", workspace.name),
                    "先停止运行并处理待处理输入队列。",
                ));
            }
        }
        self.workspaces
            .remove(workspace_id)
            .map_err(workspace_error)?;
        self.publish_workbench_snapshot();
        Ok(())
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Workspace, RpcError> {
        // 缺失工作区的公开错误与其余工作区操作同源（workspace_error）。
        self.workspaces
            .find(workspace_id)
            .ok_or_else(|| workspace_error(WorkspaceError::NotFound))
    }

    pub fn skills(
        &self,
        workspace_id: &str,
        session_id: Option<&str>,
    ) -> Result<singularity_protocol::SkillCatalog, RpcError> {
        let root = self.scope_root(workspace_id, session_id)?;
        // 技能发现的两个输入都是真实目录：工作区/会话目录与用户主目录。
        let mut catalog =
            singularity_core::skills::SkillCatalog::discover(Path::new(&root), &self.home);
        catalog.skills.retain(|skill| skill.user_invocable);
        Ok(singularity_protocol::SkillCatalog {
            skills: catalog
                .skills
                .into_iter()
                .map(|skill| singularity_protocol::SkillMetadata {
                    name: skill.name,
                    description: skill.description,
                })
                .collect(),
            diagnostics: catalog.diagnostics,
        })
    }

    /// 文件候选查询：范围解析与上限校验都在这里，扫描本身是 workspace_files
    /// 里的有界纯查询。
    pub fn file_search(
        &self,
        workspace_id: &str,
        session_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<singularity_protocol::FileCandidate>, RpcError> {
        page_limit(limit)?;
        let root = self.scope_root(workspace_id, session_id)?;
        workspace_files::search_files(&root, query, limit).map_err(invalid_request)
    }

    /// 查询范围：给了任务就用它的 cwd，否则用项目根。会话目录查询本身不恢复
    /// 会话，也不为拿目录而创建 Conversation 或写日志。
    fn scope_root(&self, workspace_id: &str, session_id: Option<&str>) -> Result<String, RpcError> {
        match session_id {
            Some(id) => self.session_directory(workspace_id, id),
            None => Ok(self.workspace(workspace_id)?.root),
        }
    }

    /// cwd 查询：已打开的任务用其运行态线程，未打开的任务用目录摘要。
    pub fn session_directory(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<String, RpcError> {
        // 先在短作用域内把命中的 slot 克隆出来再 match：全局 map 锁只保护
        // 查找本身，未打开任务的目录读盘不占用其他任务的会话查找。
        let open = self.lock_sessions().get(session_id).cloned();
        let cwd = match open {
            Some(slot) => slot.conversation().thread().cwd,
            None => {
                #[cfg(test)]
                self.run_directory_read_pause();
                self.catalog
                    .read_thread_summary(session_id)
                    .map_err(catalog_error)?
                    .cwd
            }
        };
        verify_workspace_thread(&self.workspace(workspace_id)?, &cwd)?;
        Ok(cwd)
    }
}

/// 分页与候选查询共用的条目上限；两个入口的拒绝形状一致。
pub(super) fn page_limit(limit: usize) -> Result<(), RpcError> {
    if (1..=100).contains(&limit) {
        return Ok(());
    }
    Err(invalid_request("limit must be between 1 and 100"))
}

/// 按 Session ledger 的规范 cwd 把任务分到已登记项目；registry 不缓存会话
/// 关系，因此每次读取都重新投影。
fn group_threads(
    workspaces: &[Workspace],
    threads: &[ThreadSummary],
) -> Result<BTreeMap<String, Vec<ThreadSummary>>, String> {
    // 身份与其分组桶在同一次构造中配对：匹配到的身份必然拥有自己的桶，
    // 不存在「已匹配但缺桶」的分支。
    let mut grouped: BTreeMap<
        String,
        (singularity_core::CanonicalWorkspacePath, Vec<ThreadSummary>),
    > = workspaces
        .iter()
        .map(|workspace| {
            singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
                .map(|identity| (workspace.workspace_id.clone(), (identity, Vec::new())))
        })
        .collect::<Result<_, _>>()?;
    for thread in threads {
        let identity = singularity_core::CanonicalWorkspacePath::from_saved(&thread.cwd)?;
        if let Some((_, bucket)) = grouped
            .values_mut()
            .find(|(workspace, _)| workspace.matches(&identity))
        {
            bucket.push(thread.clone());
        }
    }
    Ok(grouped
        .into_iter()
        .map(|(workspace_id, (_, threads))| (workspace_id, threads))
        .collect())
}

pub(super) fn verify_workspace_thread(workspace: &Workspace, cwd: &str) -> Result<(), RpcError> {
    match singularity_core::saved_directory_matches(&workspace.root, cwd) {
        Ok(true) => Ok(()),
        Ok(false) => Err(session_scope_conflict()),
        Err(message) => Err(internal_error(message)),
    }
}
