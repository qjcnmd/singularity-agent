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

pub(crate) struct WorkspaceStore {
    path: PathBuf,
    state: Mutex<RegistryFile>,
}

impl WorkspaceStore {
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
