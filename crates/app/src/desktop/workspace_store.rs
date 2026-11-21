//! 已添加项目的列表，保存在数据目录的 `workspaces.json`。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use protocol::Workspace;
use serde::{Deserialize, Serialize};

const WORKSPACE_REGISTRY_FILE_NAME: &str = "workspaces.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    workspaces: Vec<Workspace>,
}

/// 项目登记的读取、校验与保存错误；RPC 与启动入口直接返回原因。
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkspaceError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("项目不存在或已移除。")]
    NotFound,
    #[error("failed to read workspace registry {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace registry {} is invalid: {source}", path.display())]
    InvalidRegistry {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to update workspace registry {}: {source}", path.display())]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// 工作区登记的写入口；只有文件更新成功后才提交内存状态。
pub(crate) struct WorkspaceStore {
    path: PathBuf,
    state: Mutex<RegistryFile>,
}

impl WorkspaceStore {
    /// 打开已有登记文件；文件不存在时从空登记开始，其他读取错误直接返回。
    pub(crate) fn open(home: &Path) -> Result<Self, WorkspaceError> {
        let path = home.join(WORKSPACE_REGISTRY_FILE_NAME);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|source| WorkspaceError::InvalidRegistry { path: path.clone(), source })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RegistryFile::default(),
            Err(source) => return Err(WorkspaceError::Read { path, source }),
        };
        Ok(Self { path, state: Mutex::new(state) })
    }

    pub(crate) fn list(&self) -> Vec<Workspace> {
        self.lock().workspaces.clone()
    }

    pub(crate) fn find(&self, workspace_root: &str) -> Option<Workspace> {
        self.lock()
            .workspaces
            .iter()
            .find(|workspace| app_core::same_saved_path(&workspace.root, workspace_root))
            .cloned()
    }

    /// 登记一个已存在的目录；同一目录（忽略大小写）已登记时报错。
    pub(crate) fn add(&self, root: &Path) -> Result<Workspace, WorkspaceError> {
        let canonical = app_core::canonicalize_workspace(root).map_err(WorkspaceError::InvalidInput)?;
        self.update(|registry| {
            if registry
                .workspaces
                .iter()
                .any(|workspace| app_core::same_saved_path(&workspace.root, canonical.display()))
            {
                return Err(WorkspaceError::InvalidInput("此项目已经添加。".into()));
            }
            let workspace = Workspace { root: canonical.display().to_string() };
            registry.workspaces.push(workspace.clone());
            Ok(workspace)
        })
    }

    /// 移除登记；目录中的文件及会话历史由各自所有者管理。
    pub(crate) fn remove(&self, workspace_root: &str) -> Result<(), WorkspaceError> {
        self.update(|registry| {
            let position = registry
                .workspaces
                .iter()
                .position(|workspace| app_core::same_saved_path(&workspace.root, workspace_root))
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
        app_core::write_json_atomic(&self.path, &next)
            .map_err(|source| WorkspaceError::Storage { path: self.path.clone(), source })?;
        *registry = next;
        Ok(result)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryFile> {
        self.state.lock().expect("workspace registry lock poisoned")
    }
}
