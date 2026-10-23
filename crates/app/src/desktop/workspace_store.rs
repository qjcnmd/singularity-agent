//! 已添加项目的列表，保存在数据目录的 `workspaces.json`。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use singularity_protocol::Workspace;

const WORKSPACE_REGISTRY_FILE_NAME: &str = "workspaces.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    workspaces: Vec<Workspace>,
}

/// 输入无效、项目不存在、保存失败三类错误；RPC 入口据此给出不同的处理建议。
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("项目不存在。")]
    NotFound,
    #[error("failed to update workspace registry {}: {source}", path.display())]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 工作区登记的写入口；只有文件更新成功后才提交内存状态。
pub struct WorkspaceStore {
    path: PathBuf,
    state: Mutex<RegistryFile>,
}

impl WorkspaceStore {
    /// 打开已有登记文件；文件不存在时从空登记开始，其他读取错误直接返回。
    pub fn open(home: &Path) -> Result<Self, String> {
        singularity_core::create_data_dir(home)?;
        let path = home.join(WORKSPACE_REGISTRY_FILE_NAME);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("workspace registry is invalid: {error}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RegistryFile::default(),
            Err(error) => {
                return Err(format!("failed to read workspace registry {}: {error}", path.display()));
            }
        };
        Ok(Self { path, state: Mutex::new(state) })
    }

    /// 返回当前登记的独立快照。
    pub fn list(&self) -> Vec<Workspace> {
        self.lock().workspaces.clone()
    }

    /// 按目录查找已登记的项目。
    pub fn find(&self, workspace_root: &str) -> Option<Workspace> {
        let key = singularity_core::CanonicalWorkspacePath::from_saved(workspace_root).ok()?;
        self.lock()
            .workspaces
            .iter()
            .find(|workspace| workspace.root.to_lowercase() == key.key())
            .cloned()
    }

    /// 登记一个已存在的目录；同一目录（忽略大小写）已登记时报错。
    pub fn add(&self, root: &Path) -> Result<Workspace, WorkspaceError> {
        let canonical =
            singularity_core::canonicalize_workspace(root).map_err(WorkspaceError::InvalidInput)?;
        self.update(|registry| {
            if registry
                .workspaces
                .iter()
                .any(|workspace| workspace.root.to_lowercase() == canonical.key())
            {
                return Err(WorkspaceError::InvalidInput("此项目已经添加。".into()));
            }
            let workspace = Workspace { root: canonical.display().to_string() };
            registry.workspaces.push(workspace.clone());
            Ok(workspace)
        })
    }

    /// 移除登记；目录中的文件及会话历史由各自所有者管理。
    pub fn remove(&self, workspace_root: &str) -> Result<(), WorkspaceError> {
        let key = singularity_core::CanonicalWorkspacePath::from_saved(workspace_root)
            .map_err(WorkspaceError::InvalidInput)?;
        self.update(|registry| {
            let position = registry
                .workspaces
                .iter()
                .position(|workspace| workspace.root.to_lowercase() == key.key())
                .ok_or(WorkspaceError::NotFound)?;
            registry.workspaces.remove(position);
            Ok(())
        })
    }

    // 磁盘写入成功后才更新内存中的项目列表；持锁串行更新，避免并发修改互相覆盖。
    fn update<T>(
        &self,
        edit: impl FnOnce(&mut RegistryFile) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let mut registry = self.lock();
        let mut next = registry.clone();
        let result = edit(&mut next)?;
        singularity_core::write_json_atomic(&self.path, &next)
            .map_err(|source| WorkspaceError::Storage { path: self.path.clone(), source })?;
        *registry = next;
        Ok(result)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryFile> {
        self.state.lock().expect("workspace registry lock poisoned (fail-stop)")
    }
}
