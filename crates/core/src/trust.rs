//! 项目信任决策（对齐 Pi `project-trust.js` 语义）。
//!
//! Pi 语义：项目无信任资源（AGENTS.md）→ 直接信任；`trust.json` 有记录 → 用记录；
//! 无记录 → 按 `defaultProjectTrust`（默认 ask）；ask 无交互 UI → 不信任。
//! 信任只影响项目指令加载，不拒绝运行。

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::project_instructions::{
    PROJECT_INSTRUCTIONS_FILE_NAME, PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME,
};

/// 未显式设置 `SINGULARITY_HOME` 时使用的用户级数据子目录名。
const USER_SINGULARITY_DIR_NAME: &str = ".singularity";
/// trust.json 的稳定文件名。
const TRUST_STORAGE_FILE_NAME: &str = "trust.json";
/// trust.json 的 schema 版本。
const TRUST_STORAGE_VERSION: u32 = 1;

/// 项目无记录时的默认信任策略（对齐 Pi `defaultProjectTrust`，默认 ask）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrustDefault {
    Always,
    Never,
    #[default]
    Ask,
}

/// 项目信任解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustResolution {
    /// 加载项目指令。
    Trusted,
    /// 不加载项目指令（不拒绝运行）。
    NotTrusted,
    /// ask 未决且存在交互 UI：调用方需向用户询问后写回决策。
    AskNeeded,
}

/// 默认项目信任策略（对齐 Pi `defaultProjectTrust: "ask"`）。
pub const DEFAULT_PROJECT_TRUST: TrustDefault = TrustDefault::Ask;

/// `trust.json` 的持久化内容（`{ "version": 1, "projects": { "<canonical_cwd>": bool } }`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustStorage {
    #[serde(default = "default_trust_version")]
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, bool>,
}

fn default_trust_version() -> u32 {
    TRUST_STORAGE_VERSION
}

/// `<singularity_home>/trust.json` 存储的项目信任决策集合。
///
/// 所有查询/写入键均为 canonical 路径；写入原子完成（临时文件 + rename）。
#[derive(Debug, Clone)]
pub struct TrustDecisions {
    home: PathBuf,
    storage: TrustStorage,
}

impl Default for TrustDecisions {
    fn default() -> Self {
        Self {
            home: PathBuf::new(),
            storage: TrustStorage::default(),
        }
    }
}

impl TrustDecisions {
    /// 从 `<home>/trust.json` 加载；文件缺失、损坏或未知 schema 时返回空决策集（fail-soft）。
    pub fn load(home: &Path) -> Self {
        let mut decisions = Self {
            home: home.to_path_buf(),
            storage: TrustStorage::default(),
        };
        if let Some(storage) = fs::read(home.join(TRUST_STORAGE_FILE_NAME))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        {
            decisions.storage = storage;
        }
        decisions
    }

    /// 查询项目记录的决定；无记录返回 `None`。
    pub fn get(&self, path: &Path) -> Option<bool> {
        self.storage
            .projects
            .get(&canonical_trust_key(path))
            .copied()
    }

    /// 设置项目决定并原子写回存储。
    pub fn set(&mut self, path: &Path, trusted: bool) -> io::Result<()> {
        self.storage
            .projects
            .insert(canonical_trust_key(path), trusted);
        self.save()
    }

    /// 清除项目记录（恢复为默认 ask 流程）并原子写回存储。
    pub fn remove(&mut self, path: &Path) -> io::Result<()> {
        self.storage.projects.remove(&canonical_trust_key(path));
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        fs::create_dir_all(&self.home)?;
        let path = self.home.join(TRUST_STORAGE_FILE_NAME);
        let bytes =
            serde_json::to_vec_pretty(&self.storage).expect("trust storage always serializes");
        let temporary = self.home.join(format!("{TRUST_STORAGE_FILE_NAME}.tmp"));
        fs::write(&temporary, bytes)?;
        match fs::rename(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }
}

/// 解析用户级 singularity 数据目录：显式 `SINGULARITY_HOME`，否则 `$HOME/.singularity`
/// （与 model crate 的用户配置目录语义一致）。
pub fn user_singularity_home() -> Option<PathBuf> {
    let explicit_home = std::env::var_os("SINGULARITY_HOME");
    let home = explicit_home
        .clone()
        .or_else(|| std::env::var_os("USERPROFILE"))
        .or_else(|| std::env::var_os("HOME"))?;
    let home = PathBuf::from(home);
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return None;
    }
    if explicit_home.is_some() {
        Some(home)
    } else {
        Some(home.join(USER_SINGULARITY_DIR_NAME))
    }
}

/// 项目目录是否存在信任资源（AGENTS.md 或 AGENTS.override.md）。
pub fn has_project_trust_resource(cwd: &Path) -> bool {
    [PROJECT_INSTRUCTIONS_FILE_NAME, PROJECT_INSTRUCTIONS_OVERRIDE_FILE_NAME]
        .iter()
        .any(|name| cwd.join(name).is_file())
}

/// 对齐 Pi `resolveProjectTrusted` 顺序解析项目信任：
///
/// 1. 项目无信任资源（AGENTS.md）→ 直接信任（无需询问）；
/// 2. `trust.json` 有记录 → 用记录；
/// 3. 无记录 → 默认策略：always 信任 / never 不信任 / ask 由调用方询问
///    （无交互 UI 时按不信任处理）。
pub fn resolve_project_trusted(
    cwd: &Path,
    decisions: &TrustDecisions,
    default_trust: TrustDefault,
    has_interactive_ui: bool,
) -> TrustResolution {
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if !has_project_trust_resource(&cwd) {
        return TrustResolution::Trusted;
    }
    if let Some(recorded) = decisions.get(&cwd) {
        return if recorded {
            TrustResolution::Trusted
        } else {
            TrustResolution::NotTrusted
        };
    }
    match default_trust {
        TrustDefault::Always => TrustResolution::Trusted,
        TrustDefault::Never => TrustResolution::NotTrusted,
        TrustDefault::Ask if has_interactive_ui => TrustResolution::AskNeeded,
        TrustDefault::Ask => TrustResolution::NotTrusted,
    }
}

fn canonical_trust_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}
