//! 工作台 Workspace 登记事实的 owner-only 持久化。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use singularity_protocol::{ThreadSummary, Workspace};
use uuid::Uuid;

const REGISTRY_VERSION: u16 = 1;
pub const WORKBENCH_FILE_NAME: &str = "workbench.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryFile {
    version: u16,
    workspaces: Vec<Workspace>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            workspaces: Vec::new(),
        }
    }
}

/// 登记操作区分输入问题、项目缺失与持久化失败，供入口选择恢复提示。
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("项目不存在。")]
    NotFound,
    #[error("failed to serialize workbench registry: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("failed to update workbench registry {}: {source}", path.display())]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone)]
pub struct WorkspaceStore {
    path: PathBuf,
    state: Arc<Mutex<RegistryFile>>,
}

impl WorkspaceStore {
    pub fn open(home: &Path) -> Result<Self, String> {
        singularity_core::create_data_dir(home)?;
        let path = home.join(WORKBENCH_FILE_NAME);
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                singularity_core::ensure_regular_file(&path)?;
                let parsed: RegistryFile = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("workbench registry is invalid: {error}"))?;
                if parsed.version != REGISTRY_VERSION {
                    return Err(format!(
                        "unsupported workbench registry version {} at {}",
                        parsed.version,
                        path.display()
                    ));
                }
                validate_registry(parsed)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RegistryFile::default(),
            Err(error) => {
                return Err(format!(
                    "failed to read workbench registry {}: {error}",
                    path.display()
                ));
            }
        };
        Ok(Self {
            path,
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub fn list(&self) -> Vec<Workspace> {
        self.lock().workspaces.clone()
    }

    pub fn find(&self, workspace_id: &str) -> Option<Workspace> {
        self.lock()
            .workspaces
            .iter()
            .find(|workspace| workspace.workspace_id == workspace_id)
            .cloned()
    }

    /// 每次读取都按 Session ledger 的规范 cwd 投影分组；registry 不缓存会话关系。
    pub fn group_threads(
        workspaces: &[Workspace],
        threads: &[ThreadSummary],
    ) -> Result<BTreeMap<String, Vec<ThreadSummary>>, String> {
        let identities = workspaces
            .iter()
            .map(|workspace| {
                singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
                    .map(|identity| (workspace.workspace_id.clone(), identity))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut grouped = workspaces
            .iter()
            .map(|workspace| (workspace.workspace_id.clone(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        for thread in threads {
            let identity = singularity_core::CanonicalWorkspacePath::from_saved(&thread.cwd)?;
            if let Some((workspace_id, _)) = identities
                .iter()
                .find(|(_, workspace)| workspace.matches(&identity))
            {
                let Some(bucket) = grouped.get_mut(workspace_id) else {
                    return Err("registered workspace projection is missing".to_string());
                };
                bucket.push(thread.clone());
            }
        }
        Ok(grouped)
    }

    pub fn add(&self, root: &Path) -> Result<Workspace, WorkspaceError> {
        let canonical =
            singularity_core::canonicalize_workspace(root).map_err(WorkspaceError::InvalidInput)?;
        self.update(|registry| {
            for workspace in &registry.workspaces {
                let existing =
                    singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
                        .map_err(WorkspaceError::InvalidInput)?;
                if existing.matches(&canonical) {
                    return Err(WorkspaceError::InvalidInput("此项目已经添加。".into()));
                }
            }
            let name = canonical
                .as_path()
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .unwrap_or(canonical.display())
                .to_string();
            let workspace = Workspace {
                workspace_id: Uuid::new_v4().to_string(),
                name,
                root: canonical.display().to_string(),
            };
            registry.workspaces.push(workspace.clone());
            Ok(workspace)
        })
    }

    /// Rename a registered workspace without changing its root or session membership.
    pub fn rename(&self, workspace_id: &str, name: &str) -> Result<Workspace, WorkspaceError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(WorkspaceError::InvalidInput("项目名称不能为空。".into()));
        }
        self.update(|registry| {
            if registry
                .workspaces
                .iter()
                .any(|item| item.workspace_id != workspace_id && item.name == name)
            {
                return Err(WorkspaceError::InvalidInput("已有同名项目。".into()));
            }
            let workspace = registry
                .workspaces
                .iter_mut()
                .find(|item| item.workspace_id == workspace_id)
                .ok_or(WorkspaceError::NotFound)?;
            workspace.name = name.to_string();
            Ok(workspace.clone())
        })
    }

    pub fn remove(&self, workspace_id: &str) -> Result<Workspace, WorkspaceError> {
        self.update(|registry| {
            let position = registry
                .workspaces
                .iter()
                .position(|workspace| workspace.workspace_id == workspace_id)
                .ok_or(WorkspaceError::NotFound)?;
            Ok(registry.workspaces.remove(position))
        })
    }

    // Publish the edited registry only after persistence succeeds. Serializing the
    // whole read/edit/write operation prevents concurrent changes from being lost.
    fn update<T>(
        &self,
        edit: impl FnOnce(&mut RegistryFile) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let mut registry = self.lock();
        let mut next = registry.clone();
        let result = edit(&mut next)?;
        let mut bytes = serde_json::to_vec_pretty(&next)?;
        bytes.push(b'\n');
        singularity_core::atomic_replace_bytes(&self.path, &bytes).map_err(|source| {
            WorkspaceError::Storage {
                path: self.path.clone(),
                source,
            }
        })?;
        *registry = next;
        Ok(result)
    }

    #[allow(clippy::expect_used)]
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryFile> {
        self.state
            .lock()
            .expect("workspace registry lock poisoned (fail-stop)")
    }
}

fn validate_registry(mut registry: RegistryFile) -> Result<RegistryFile, String> {
    let mut identities = Vec::new();
    let mut ids = HashSet::new();
    for workspace in &mut registry.workspaces {
        let id = Uuid::parse_str(&workspace.workspace_id)
            .map_err(|_| "workbench registry contains an invalid workspace id".to_string())?;
        if !ids.insert(id) {
            return Err("workbench registry contains a duplicate workspace id".into());
        }
        let canonical = singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)?;
        if identities
            .iter()
            .any(|existing: &singularity_core::CanonicalWorkspacePath| existing.matches(&canonical))
        {
            return Err("workbench registry contains a duplicate workspace root".to_string());
        }
        workspace.root = canonical.display().to_string();
        if workspace.name.trim().is_empty() {
            return Err("workbench registry contains an empty workspace name".to_string());
        }
        identities.push(canonical);
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn registry_reopens_and_rejects_duplicate_roots() {
        let home = tempfile::tempdir().expect("temp home");
        let workspace = tempfile::tempdir().expect("temp workspace");
        let store = WorkspaceStore::open(home.path()).expect("open registry");
        let added = store.add(workspace.path()).expect("add workspace");
        assert!(store.add(&workspace.path().join(".")).is_err());
        drop(store);

        let reopened = WorkspaceStore::open(home.path()).expect("reopen registry");
        assert_eq!(reopened.list(), vec![added.clone()]);
        assert_eq!(reopened.remove(&added.workspace_id).expect("remove"), added);
        assert!(reopened.list().is_empty());
        let restored = reopened.add(workspace.path()).expect("re-add workspace");
        assert_ne!(restored.workspace_id, added.workspace_id);
    }

    #[test]
    fn failed_updates_preserve_the_registered_projects() {
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project");
        let other = tempfile::tempdir().expect("other project");
        let store = WorkspaceStore::open(home.path()).expect("store");
        let workspace = store.add(project.path()).expect("add");
        let path = home.path().join(WORKBENCH_FILE_NAME);
        std::fs::rename(&path, home.path().join("saved.json")).expect("preserve registry");
        std::fs::create_dir(&path).expect("block replacement with a directory");
        for result in [
            store.rename(&workspace.workspace_id, "updated"),
            store.remove(&workspace.workspace_id),
            store.add(other.path()),
        ] {
            let error = result.expect_err("persistence must fail");
            assert!(matches!(error, WorkspaceError::Storage { .. }));
            assert!(std::error::Error::source(&error).is_some());
            assert_eq!(store.list(), vec![workspace.clone()]);
        }
    }

    #[test]
    fn registry_rejects_reused_workspace_ids() {
        let home = tempfile::tempdir().expect("home");
        let id = Uuid::new_v4().to_string();
        let registry = RegistryFile {
            version: REGISTRY_VERSION,
            workspaces: ["first", "second"]
                .map(|name| Workspace {
                    workspace_id: id.clone(),
                    name: name.into(),
                    root: singularity_core::display_path(&home.path().join(name)),
                })
                .to_vec(),
        };
        singularity_core::atomic_replace_bytes(
            &home.path().join(WORKBENCH_FILE_NAME),
            &serde_json::to_vec(&registry).expect("serialize"),
        )
        .expect("write registry");
        let error = match WorkspaceStore::open(home.path()) {
            Ok(_) => panic!("duplicate IDs must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("duplicate workspace id"));
    }

    #[test]
    fn unknown_registry_version_fails_closed() {
        let home = tempfile::tempdir().expect("temp home");
        std::fs::write(
            home.path().join(WORKBENCH_FILE_NAME),
            br#"{"version":2,"workspaces":[]}"#,
        )
        .expect("write registry");
        assert!(WorkspaceStore::open(home.path()).is_err());
    }
}
