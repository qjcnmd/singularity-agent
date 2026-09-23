//! 工作台 Workspace 登记事实的持久化（仅属主可读写）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use singularity_protocol::Workspace;
use uuid::Uuid;

const REGISTRY_VERSION: u16 = 1;
const WORKSPACE_REGISTRY_FILE_NAME: &str = "workspaces.json";

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

/// 登记操作的错误分三类：输入有问题、项目不存在、持久化失败；入口据此选择恢复提示。
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

pub struct WorkspaceStore {
    path: PathBuf,
    state: Mutex<RegistryFile>,
}

impl WorkspaceStore {
    pub fn open(home: &Path) -> Result<Self, String> {
        singularity_core::create_data_dir(home)?;
        let path = home.join(WORKSPACE_REGISTRY_FILE_NAME);
        let state = match std::fs::read(&path) {
            Ok(bytes) => {
                singularity_core::ensure_regular_file(&path)?;
                let mut parsed: RegistryFile = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("workspace registry is invalid: {error}"))?;
                if parsed.version != REGISTRY_VERSION {
                    return Err(format!(
                        "unsupported workspace registry version: {}",
                        parsed.version
                    ));
                }
                normalize_registry(&mut parsed);
                parsed
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RegistryFile::default(),
            Err(error) => {
                return Err(format!(
                    "failed to read workspace registry {}: {error}",
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

    /// 改名不影响 root，也不影响会话归属。名称只用于展示：工作区身份由
    /// workspace_id 和 root 决定，所以和 add 一样允许重名。
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

    // 只有持久化成功之后才发布编辑后的 registry。整个读/改/写过程串行化，
    // 避免并发修改互相覆盖。
    fn update<T>(
        &self,
        edit: impl FnOnce(&mut RegistryFile) -> Result<T, WorkspaceError>,
    ) -> Result<T, WorkspaceError> {
        let mut registry = self.lock();
        let mut next = registry.clone();
        let result = edit(&mut next)?;
        // 登记表里全是本进程构造的字符串和列表，序列化不会失败；这里仍然保留
        // 来源，让它和原子替换失败共用同一个「登记表写不出去」的出口。
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

/// 登记表只承载展示用的事实：打开时把已保存的 root 归一成唯一的显示形状。
/// 字段形状（id 是不是 UUID、有没有重复、名字是不是空）不在这里校验——它们只
/// 影响这一条项目的显示，不值得让整个工作台拒绝启动。
fn normalize_registry(registry: &mut RegistryFile) {
    for workspace in &mut registry.workspaces {
        if let Ok(canonical) = singularity_core::CanonicalWorkspacePath::from_saved(&workspace.root)
        {
            workspace.root = canonical.display().to_string();
        }
    }
}
