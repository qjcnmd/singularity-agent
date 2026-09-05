//! 工作台 Workspace 登记事实的 owner-only 持久化。

use std::collections::BTreeMap;
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

#[derive(Clone)]
pub struct WorkspaceStore {
    path: PathBuf,
    state: Arc<Mutex<RegistryFile>>,
}

impl WorkspaceStore {
    pub fn open(home: &Path) -> Result<Self, String> {
        singularity_core::create_owner_only_dir(home)?;
        let path = home.join(WORKBENCH_FILE_NAME);
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                singularity_core::ensure_owner_only_file(&path)?;
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
        &self,
        threads: &[ThreadSummary],
    ) -> Result<BTreeMap<String, Vec<ThreadSummary>>, String> {
        let workspaces = self.list();
        let identities = workspaces
            .iter()
            .map(|workspace| {
                singularity_core::canonicalize_workspace(&workspace.root)
                    .map(|identity| (workspace.workspace_id.clone(), identity))
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut grouped = workspaces
            .iter()
            .map(|workspace| (workspace.workspace_id.clone(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        for thread in threads {
            let identity = singularity_core::canonicalize_workspace(&thread.cwd)
                .map_err(|error| error.to_string())?;
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

    pub fn add(&self, root: &Path) -> Result<Workspace, String> {
        let canonical =
            singularity_core::canonicalize_workspace(root).map_err(|error| error.to_string())?;
        let mut registry = self.lock();
        for workspace in &registry.workspaces {
            let existing = singularity_core::canonicalize_workspace(&workspace.root)
                .map_err(|error| error.to_string())?;
            if existing.matches(&canonical) {
                return Err("workspace is already registered".to_string());
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
        if let Err(error) = self.persist(&registry) {
            registry.workspaces.pop();
            return Err(error);
        }
        Ok(workspace)
    }

    pub fn remove(&self, workspace_id: &str) -> Result<Workspace, String> {
        let mut registry = self.lock();
        let position = registry
            .workspaces
            .iter()
            .position(|workspace| workspace.workspace_id == workspace_id)
            .ok_or_else(|| "workspace was not found".to_string())?;
        let removed = registry.workspaces.remove(position);
        if let Err(error) = self.persist(&registry) {
            registry.workspaces.insert(position, removed);
            return Err(error);
        }
        Ok(removed)
    }

    fn persist(&self, registry: &RegistryFile) -> Result<(), String> {
        let mut bytes = serde_json::to_vec_pretty(registry)
            .map_err(|error| format!("failed to serialize workbench registry: {error}"))?;
        bytes.push(b'\n');
        singularity_core::atomic_replace_bytes(&self.path, &bytes).map_err(|error| {
            format!(
                "failed to update workbench registry {}: {error}",
                self.path.display()
            )
        })?;
        singularity_core::ensure_owner_only_file(&self.path)
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
    for workspace in &mut registry.workspaces {
        Uuid::parse_str(&workspace.workspace_id)
            .map_err(|_| "workbench registry contains an invalid workspace id".to_string())?;
        let canonical = singularity_core::canonicalize_workspace(&workspace.root)
            .map_err(|error| error.to_string())?;
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
