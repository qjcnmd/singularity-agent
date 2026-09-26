//! 工作台的工作区和目录边界：项目登记、会话目录查询和分组投影。
//!
//! 这里只回答「任务属于哪个项目、目录在哪、怎么分组、有哪些候选」，不碰会话执行编排；
//! 会话生命周期、事件归并、发布入口和错误映射都由父模块持有，本模块只管调用。

use std::collections::BTreeMap;
use std::path::Path;

use singularity_protocol::{
    AppBootstrap, RedactedModelCatalog, RpcError, ThreadSummary, Workspace,
};
use singularity_runtime::WorkspaceError;

use super::{
    AppServer, catalog_error, internal_error, invalid_request, session_scope_conflict,
    workspace_error,
};
use crate::desktop::workspace_files;

impl AppServer {
    pub fn bootstrap(&self) -> Result<AppBootstrap, RpcError> {
        // 目录读取放在一个独立的小作用域里完成：模型锁不能被带进会话锁和页面发布。
        let catalog = {
            let models = self.lock_models();
            models.redacted_catalog()
        };
        self.bootstrap_with_catalog(catalog)
    }

    pub(super) fn bootstrap_with_catalog(
        &self,
        model_catalog: RedactedModelCatalog,
    ) -> Result<AppBootstrap, RpcError> {
        let revision = self.revision();
        let workspaces = self.workspaces.list();
        // 任务目录以 catalog 为唯一权威：冻结的 history 只用来恢复执行内容，
        // 不再回填目录摘要。阶段直接问 Conversation，不取 Slot 状态锁。
        let threads = self.catalog.list_threads().map_err(catalog_error)?;
        let session_phases = self
            .lock_sessions()
            .iter()
            .map(|(id, slot)| (id.clone(), slot.conversation().phase()))
            .collect();
        let sessions_by_workspace = group_threads(&workspaces, &threads).map_err(internal_error)?;
        Ok(AppBootstrap {
            user_home: singularity_core::HomeEnv::from_process()
                .os_home
                .and_then(|home| home.into_string().ok()),
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
        self.publish_app_snapshot();
        Ok(workspace)
    }

    pub fn rename_workspace(&self, workspace_id: &str, name: &str) -> Result<(), RpcError> {
        self.workspaces
            .rename(workspace_id, name)
            .map_err(workspace_error)?;
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn remove_workspace(&self, workspace_id: &str) -> Result<(), RpcError> {
        let workspace = self.workspace(workspace_id)?;
        // 占用情况只看已登记的 slot，不靠可能失败、可能不全的磁盘目录枚举：会话属于谁由
        // 它的规范 cwd 决定，忙不忙由它的运行阶段和待处理输入决定。生命周期临界区和启动
        // 占用共用同一条边界，检查和注销之间插不进新的占用。
        let _lifecycle = self.lock_lifecycle();
        let busy = self.lock_sessions().values().any(|slot| {
            // 归属只做布尔判断，用的是和打开任务时同一条目录比较规则。
            let belongs = matches!(
                singularity_core::saved_directory_matches(
                    &workspace.root,
                    &slot.conversation().thread().cwd,
                ),
                Ok(true)
            );
            belongs && slot.conversation().is_occupied()
        });
        if busy {
            return Err(RpcError::new(
                singularity_protocol::RpcErrorCode::WorkspaceBusy,
                format!("项目 {} 仍有活动任务或待处理输入。", workspace.name),
                "先停止运行并处理待处理输入队列。",
            ));
        }
        self.workspaces
            .remove(workspace_id)
            .map_err(workspace_error)?;
        self.publish_app_snapshot();
        Ok(())
    }

    pub fn workspace(&self, workspace_id: &str) -> Result<Workspace, RpcError> {
        // 工作区不存在时，对外错误和其他工作区操作走同一个来源（workspace_error）。
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

    /// 文件候选查询：范围解析和上限校验都在这里做，扫描本身是 workspace_files
    /// 里那个有界的纯查询。
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

    /// 查询范围：给了任务就用它的 cwd，没给就用项目根。查目录这件事不会顺手恢复
    /// 会话，也不会为了拿目录去建 Conversation 或写日志。
    fn scope_root(&self, workspace_id: &str, session_id: Option<&str>) -> Result<String, RpcError> {
        match session_id {
            Some(id) => self.session_directory(workspace_id, id),
            None => Ok(self.workspace(workspace_id)?.root),
        }
    }

    /// 查 cwd：已经打开的任务用它运行中的线程，没打开的任务读目录摘要。
    pub fn session_directory(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<String, RpcError> {
        // 先在一个短作用域里把命中的 slot 克隆出来再 match：全局 map 锁只保护
        // 这次查找，未打开任务的读盘不会挡住其他任务的会话查找。
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

/// 分页和候选查询共用的条目上限；两个入口拒绝时给出的错误形状一致。
pub(super) fn page_limit(limit: usize) -> Result<(), RpcError> {
    if (1..=100).contains(&limit) {
        return Ok(());
    }
    Err(invalid_request("limit must be between 1 and 100"))
}

/// 按 Session ledger 里的规范 cwd 把任务分到已登记项目；registry 不缓存会话
/// 关系，所以每次读取都重新投影一遍。
fn group_threads(
    workspaces: &[Workspace],
    threads: &[ThreadSummary],
) -> Result<BTreeMap<String, Vec<ThreadSummary>>, String> {
    // 身份和它的分组桶在同一次构造里配好：匹配上的身份必然有自己的桶，
    // 不会出现「匹配到了却没有桶」的情况。
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
