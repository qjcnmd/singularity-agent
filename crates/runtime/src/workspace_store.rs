//! 工作台 Workspace 登记事实的 owner-only 持久化。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use singularity_protocol::Workspace;
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
    #[error("failed to update workbench registry {}: {source}", path.display())]
    Storage {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub struct WorkspaceStore {
    path: PathBuf,
    state: Mutex<RegistryFile>,
}

impl WorkspaceStore {
    pub fn open(home: &Path) -> Result<Self, String> {
        singularity_core::create_data_dir(home)?;
        let path = home.join(WORKBENCH_FILE_NAME);
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                singularity_core::ensure_regular_file(&path)?;
                let mut parsed: RegistryFile = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("workbench registry is invalid: {error}"))?;
                normalize_registry(&mut parsed);
                parsed
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
            state: Mutex::new(state),
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

    /// 改名不影响 root 与会话归属。名称只用于展示：工作区身份由 workspace_id 与
    /// root 决定，因此与 add 一致地允许重名。
    pub fn rename(&self, workspace_id: &str, name: &str) -> Result<(), WorkspaceError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(WorkspaceError::InvalidInput("项目名称不能为空。".into()));
        }
        self.update(|registry| {
            let workspace = registry
                .workspaces
                .iter_mut()
                .find(|item| item.workspace_id == workspace_id)
                .ok_or(WorkspaceError::NotFound)?;
            workspace.name = name.to_string();
            Ok(())
        })
    }

    pub fn remove(&self, workspace_id: &str) -> Result<(), WorkspaceError> {
        self.update(|registry| {
            let position = registry
                .workspaces
                .iter()
                .position(|workspace| workspace.workspace_id == workspace_id)
                .ok_or(WorkspaceError::NotFound)?;
            registry.workspaces.remove(position);
            Ok(())
        })
    }

    // 仅在持久化成功后才发布编辑后的 registry。把整个读/改/写操作串行化，
    // 可防止并发变更丢失。
    fn update<T>(
        &self,
        edit: impl FnOnce(&mut RegistryFile) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let mut registry = self.lock();
        let mut next = registry.clone();
        let result = edit(&mut next)?;
        // 登记表全是本进程构造的字符串与列表，序列化不会失败；仍然保留来源，
        // 让它与原子替换失败共用同一个「登记表写不出去」出口。
        let mut bytes =
            serde_json::to_vec_pretty(&next).map_err(|error| WorkspaceError::Storage {
                path: self.path.clone(),
                source: std::io::Error::other(error),
            })?;
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

/// 登记表只承载展示事实：打开时把已保存的 root 归一为唯一显示形状。
/// 字段形状（id 是否 UUID、是否有重复、名字是否为空）不在这里校验——它只影响
/// 这一条项目的显示，不足以让整个工作台拒绝启动。
fn normalize_registry(registry: &mut RegistryFile) {
    for workspace in &mut registry.workspaces {
        if let Ok(canonical) = singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
        {
            workspace.root = canonical.display().to_string();
        }
    }
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
        reopened.remove(&added.workspace_id).expect("remove");
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
            store.add(other.path()).map(|_| ()),
        ] {
            let error = result.expect_err("persistence must fail");
            assert!(matches!(error, WorkspaceError::Storage { .. }));
            assert!(std::error::Error::source(&error).is_some());
            assert_eq!(store.list(), vec![workspace.clone()]);
        }
    }

    #[test]
    fn project_names_are_display_only_so_rename_matches_add() {
        let home = tempfile::tempdir().expect("home");
        let first = tempfile::tempdir().expect("first project");
        let second = tempfile::tempdir().expect("second project");
        let store = WorkspaceStore::open(home.path()).expect("open registry");
        let one = store.add(first.path()).expect("add first");
        let two = store.add(second.path()).expect("add second");
        // 两个不同 root 的末级目录名相同（tempdir 前缀一致），add 允许。
        store.rename(&one.workspace_id, "shared").expect("rename");
        store
            .rename(&two.workspace_id, "shared")
            .expect("a display name is not a global key");
        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|workspace| workspace.name == "shared"));
        assert!(store.rename(&one.workspace_id, "   ").is_err());
        assert!(matches!(
            store.rename("00000000-0000-0000-0000-000000000000", "x"),
            Err(WorkspaceError::NotFound)
        ));
    }

    #[test]
    fn a_registry_with_hand_edited_field_shapes_still_opens() {
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project");
        // 展示数据不做字段形状校验：重复 id、非 UUID id 与空名字都只影响显示，
        // 不足以让工作台拒绝启动。
        std::fs::write(
            home.path().join(WORKBENCH_FILE_NAME),
            serde_json::json!({
                "version": 2,
                "workspaces": [
                    {"workspaceId": "not-a-uuid", "name": "", "root": project.path().to_string_lossy()},
                    {"workspaceId": "not-a-uuid", "name": "重复", "root": project.path().to_string_lossy()},
                ],
            })
            .to_string(),
        )
        .expect("write registry");
        let store = WorkspaceStore::open(home.path()).expect("display data must not block startup");
        assert_eq!(store.list().len(), 2);
    }
}
