use crate::absolute_path::AbsolutePathBuf;
#[cfg(windows)]
use crate::path_normalization::canonicalize_path_allow_missing;
use serde::Deserialize;
use serde::Serialize;
use singularity_core::PROTECTED_METADATA_PATH_NAMES;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;

const PROJECT_ROOTS_GLOB_PATTERN_PREFIX: &str = "singularity-project-roots://";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkSandboxPolicy {
    #[default]
    Restricted,
    Enabled,
}

impl NetworkSandboxPolicy {
    pub fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileSystemAccessMode {
    Read,
    Write,
    Deny,
}

impl FileSystemAccessMode {
    pub fn can_read(self) -> bool {
        !matches!(self, Self::Deny)
    }

    pub fn can_write(self) -> bool {
        matches!(self, Self::Write)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSystemSpecialPath {
    Root,
    Minimal,
    ProjectRoots {
        subpath: Option<PathBuf>,
    },
    Tmpdir,
    SlashTmp,
    Unknown {
        path: String,
        subpath: Option<PathBuf>,
    },
}

impl FileSystemSpecialPath {
    pub fn project_roots(subpath: Option<PathBuf>) -> Self {
        Self::ProjectRoots { subpath }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileSystemPath<PathType = AbsolutePathBuf> {
    Path { path: PathType },
    GlobPattern { pattern: String },
    Special { value: FileSystemSpecialPath },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileSystemSandboxEntry<PathType = AbsolutePathBuf> {
    pub path: FileSystemPath<PathType>,
    pub access: FileSystemAccessMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FileSystemSandboxKind {
    #[default]
    Restricted,
    Unrestricted,
    ExternalSandbox,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSystemSandboxPolicy {
    pub kind: FileSystemSandboxKind,
    pub glob_scan_max_depth: Option<usize>,
    pub entries: Vec<FileSystemSandboxEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritableRoot {
    pub root: AbsolutePathBuf,
    pub read_only_subpaths: Vec<AbsolutePathBuf>,
    pub protected_metadata_names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManagedFileSystemPermissions<PathType = AbsolutePathBuf> {
    Restricted {
        entries: Vec<FileSystemSandboxEntry<PathType>>,
        glob_scan_max_depth: Option<NonZeroUsize>,
    },
    Unrestricted,
}

impl ManagedFileSystemPermissions {
    pub fn to_sandbox_policy(&self) -> FileSystemSandboxPolicy {
        match self {
            Self::Restricted {
                entries,
                glob_scan_max_depth,
            } => FileSystemSandboxPolicy {
                kind: FileSystemSandboxKind::Restricted,
                glob_scan_max_depth: glob_scan_max_depth.map(usize::from),
                entries: entries.clone(),
            },
            Self::Unrestricted => FileSystemSandboxPolicy::unrestricted(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionProfile<PathType = AbsolutePathBuf> {
    Managed {
        file_system: ManagedFileSystemPermissions<PathType>,
        network: NetworkSandboxPolicy,
    },
    Disabled,
    External {
        network: NetworkSandboxPolicy,
    },
}

impl Default for PermissionProfile {
    fn default() -> Self {
        Self::read_only()
    }
}

impl PermissionProfile {
    pub fn read_only() -> Self {
        Self::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: FileSystemSandboxPolicy::read_only().entries,
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        }
    }

    pub fn workspace_write() -> Self {
        Self::workspace_write_with(&[], NetworkSandboxPolicy::Restricted, false, false)
    }

    pub fn workspace_write_with(
        writable_roots: &[AbsolutePathBuf],
        network: NetworkSandboxPolicy,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Self {
        let policy = FileSystemSandboxPolicy::workspace_write(
            writable_roots,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        );
        Self::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: policy.entries,
                glob_scan_max_depth: policy.glob_scan_max_depth.and_then(NonZeroUsize::new),
            },
            network,
        }
    }

    pub fn to_runtime_permissions(&self) -> (FileSystemSandboxPolicy, NetworkSandboxPolicy) {
        match self {
            Self::Managed {
                file_system,
                network,
            } => (file_system.to_sandbox_policy(), *network),
            Self::Disabled => (
                FileSystemSandboxPolicy::unrestricted(),
                NetworkSandboxPolicy::Enabled,
            ),
            Self::External { network } => (FileSystemSandboxPolicy::external_sandbox(), *network),
        }
    }
}

impl Default for FileSystemSandboxPolicy {
    fn default() -> Self {
        Self::read_only()
    }
}

impl FileSystemSandboxPolicy {
    pub fn read_only() -> Self {
        Self::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::Root,
            },
            access: FileSystemAccessMode::Read,
        }])
    }

    pub fn unrestricted() -> Self {
        Self {
            kind: FileSystemSandboxKind::Unrestricted,
            glob_scan_max_depth: None,
            entries: Vec::new(),
        }
    }

    pub fn external_sandbox() -> Self {
        Self {
            kind: FileSystemSandboxKind::ExternalSandbox,
            glob_scan_max_depth: None,
            entries: Vec::new(),
        }
    }

    pub fn restricted(entries: Vec<FileSystemSandboxEntry>) -> Self {
        Self {
            kind: FileSystemSandboxKind::Restricted,
            glob_scan_max_depth: None,
            entries,
        }
    }

    pub fn workspace_write(
        writable_roots: &[AbsolutePathBuf],
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> Self {
        let mut entries = Self::read_only().entries;
        entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::project_roots(None),
            },
            access: FileSystemAccessMode::Write,
        });
        entries.extend(
            writable_roots
                .iter()
                .cloned()
                .map(|path| FileSystemSandboxEntry {
                    path: FileSystemPath::Path { path },
                    access: FileSystemAccessMode::Write,
                }),
        );
        if !exclude_tmpdir_env_var {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::Tmpdir,
                },
                access: FileSystemAccessMode::Write,
            });
        }
        if !exclude_slash_tmp {
            entries.push(FileSystemSandboxEntry {
                path: FileSystemPath::Special {
                    value: FileSystemSpecialPath::SlashTmp,
                },
                access: FileSystemAccessMode::Write,
            });
        }
        Self::restricted(entries)
    }

    pub fn has_full_disk_read_access(&self) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Unrestricted)
            || self.has_root_access(FileSystemAccessMode::can_read)
    }

    pub fn has_full_disk_write_access(&self) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Unrestricted)
            || self.has_root_access(FileSystemAccessMode::can_write)
    }

    pub fn include_platform_defaults(&self) -> bool {
        !self.has_full_disk_read_access()
    }

    fn has_root_access(&self, predicate: impl Fn(FileSystemAccessMode) -> bool) -> bool {
        matches!(self.kind, FileSystemSandboxKind::Restricted)
            && self.entries.iter().any(|entry| {
                matches!(
                    &entry.path,
                    FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    } if predicate(entry.access)
                )
            })
    }

    pub fn materialize_project_roots_with_workspace_roots(
        mut self,
        workspace_roots: &[AbsolutePathBuf],
    ) -> Self {
        if workspace_roots.is_empty() {
            return self;
        }
        let mut materialized = Vec::new();
        for entry in self.entries {
            match entry.path {
                FileSystemPath::Special {
                    value: FileSystemSpecialPath::ProjectRoots { subpath },
                } => {
                    materialized.extend(workspace_roots.iter().map(|root| {
                        FileSystemSandboxEntry {
                            path: FileSystemPath::Path {
                                path: subpath
                                    .as_ref()
                                    .map_or_else(|| root.clone(), |subpath| root.join(subpath)),
                            },
                            access: entry.access,
                        }
                    }));
                }
                FileSystemPath::GlobPattern { pattern }
                    if pattern.starts_with(PROJECT_ROOTS_GLOB_PATTERN_PREFIX) =>
                {
                    let subpath = &pattern[PROJECT_ROOTS_GLOB_PATTERN_PREFIX.len()..];
                    materialized.extend(workspace_roots.iter().map(|root| {
                        FileSystemSandboxEntry {
                            path: FileSystemPath::GlobPattern {
                                pattern: AbsolutePathBuf::resolve_path_against_base(subpath, root)
                                    .to_string_lossy()
                                    .into_owned(),
                            },
                            access: entry.access,
                        }
                    }));
                }
                path => materialized.push(FileSystemSandboxEntry {
                    path,
                    access: entry.access,
                }),
            }
        }
        self.entries = materialized;
        self
    }

    pub fn get_readable_roots_with_cwd(&self, cwd: &Path) -> Vec<AbsolutePathBuf> {
        self.resolved_exact_entries(cwd)
            .into_iter()
            .filter(|entry| entry.access.can_read())
            .map(|entry| entry.path)
            .collect::<Vec<_>>()
            .pipe(dedup_paths)
    }

    pub fn get_writable_roots_with_cwd(&self, cwd: &Path) -> Vec<WritableRoot> {
        self.get_writable_roots_with_cwd_and_protected_metadata(cwd, true)
    }

    pub(crate) fn get_writable_roots_with_cwd_and_protected_metadata(
        &self,
        cwd: &Path,
        protect_workspace_metadata: bool,
    ) -> Vec<WritableRoot> {
        let resolved = self.resolved_exact_entries(cwd);
        let write_roots = resolved
            .iter()
            .filter(|entry| entry.access.can_write())
            .map(|entry| canonical_or_lexical(entry.path.as_path()))
            .collect::<Vec<_>>();
        let denied = resolved
            .iter()
            .filter(|entry| entry.access == FileSystemAccessMode::Deny)
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();

        dedup_paths(write_roots)
            .into_iter()
            .map(|root| {
                let mut read_only_subpaths = denied
                    .iter()
                    .filter(|path| path.as_path().starts_with(root.as_path()))
                    .cloned()
                    .collect::<Vec<_>>();
                if protect_workspace_metadata {
                    read_only_subpaths.extend(
                        PROTECTED_METADATA_PATH_NAMES
                            .iter()
                            .map(|name| root.join(name)),
                    );
                }
                WritableRoot {
                    root,
                    read_only_subpaths: dedup_paths(read_only_subpaths),
                    protected_metadata_names: if protect_workspace_metadata {
                        PROTECTED_METADATA_PATH_NAMES
                            .iter()
                            .map(|name| (*name).to_string())
                            .collect()
                    } else {
                        Vec::new()
                    },
                }
            })
            .collect()
    }

    pub fn get_unreadable_roots_with_cwd(&self, cwd: &Path) -> Vec<AbsolutePathBuf> {
        let root = absolute_root_path_for_cwd(cwd);
        self.resolved_exact_entries(cwd)
            .into_iter()
            .filter(|entry| entry.access == FileSystemAccessMode::Deny)
            .filter(|entry| entry.path != root)
            .filter(|entry| !self.can_read_path_with_cwd(entry.path.as_path(), cwd))
            .map(|entry| entry.path)
            .collect::<Vec<_>>()
            .pipe(dedup_paths)
    }

    pub fn get_unreadable_globs_with_cwd(&self, cwd: &Path) -> Vec<String> {
        let mut patterns = self
            .entries
            .iter()
            .filter(|entry| entry.access == FileSystemAccessMode::Deny)
            .filter_map(|entry| match &entry.path {
                FileSystemPath::GlobPattern { pattern } => Some(
                    AbsolutePathBuf::resolve_path_against_base(pattern, cwd)
                        .to_string_lossy()
                        .into_owned(),
                ),
                FileSystemPath::Path { .. } | FileSystemPath::Special { .. } => None,
            })
            .collect::<Vec<_>>();
        patterns.sort();
        patterns.dedup();
        patterns
    }

    pub fn can_read_path_with_cwd(&self, path: &Path, cwd: &Path) -> bool {
        if matches!(self.kind, FileSystemSandboxKind::Unrestricted) {
            return true;
        }
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return false;
        }
        self.effective_access(path, cwd)
            .is_some_and(FileSystemAccessMode::can_read)
    }

    fn effective_access(&self, path: &Path, cwd: &Path) -> Option<FileSystemAccessMode> {
        let mut matches = Vec::new();
        for entry in self.resolved_exact_entries(cwd) {
            if path.starts_with(entry.path.as_path()) {
                matches.push((entry.path.components().count(), entry.access));
            }
        }
        for entry in &self.entries {
            if let FileSystemPath::GlobPattern { pattern } = &entry.path
                && glob_matches(pattern, path, cwd)
            {
                matches.push((usize::MAX, entry.access));
            }
        }
        matches
            .into_iter()
            .max_by_key(|(specificity, access)| (*specificity, *access))
            .map(|(_, access)| access)
    }

    fn resolved_exact_entries(&self, cwd: &Path) -> Vec<ResolvedEntry> {
        if !matches!(self.kind, FileSystemSandboxKind::Restricted) {
            return Vec::new();
        }
        self.entries
            .iter()
            .filter_map(|entry| {
                resolve_exact_path(&entry.path, cwd).map(|path| ResolvedEntry {
                    path: canonical_or_lexical(path.as_path()),
                    access: entry.access,
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    path: AbsolutePathBuf,
    access: FileSystemAccessMode,
}

pub fn project_roots_glob_pattern(subpath: &Path) -> String {
    format!("{PROJECT_ROOTS_GLOB_PATTERN_PREFIX}{}", subpath.display())
}

pub struct ReadDenyMatcher {
    denied_roots: Vec<AbsolutePathBuf>,
    denied_globs: Vec<globset::GlobMatcher>,
}

impl ReadDenyMatcher {
    pub fn try_new(policy: &FileSystemSandboxPolicy, cwd: &Path) -> Result<Option<Self>, String> {
        let denied_roots = policy.get_unreadable_roots_with_cwd(cwd);
        let denied_globs = policy
            .get_unreadable_globs_with_cwd(cwd)
            .into_iter()
            .map(|pattern| {
                globset::GlobBuilder::new(&pattern)
                    .case_insensitive(cfg!(windows))
                    .literal_separator(false)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| format!("invalid deny-read glob pattern {pattern:?}: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if denied_roots.is_empty() && denied_globs.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            denied_roots,
            denied_globs,
        }))
    }

    pub fn is_read_denied(&self, path: &Path) -> bool {
        let canonical_path = canonical_or_path_buf(path);
        self.denied_roots
            .iter()
            .any(|root| canonical_path.starts_with(root.as_path()))
            || self
                .denied_globs
                .iter()
                .any(|pattern| pattern.is_match(path))
    }
}

fn resolve_exact_path(path: &FileSystemPath, cwd: &Path) -> Option<AbsolutePathBuf> {
    match path {
        FileSystemPath::Path { path } => Some(path.clone()),
        FileSystemPath::GlobPattern { .. } => None,
        FileSystemPath::Special { value } => match value {
            FileSystemSpecialPath::Root => Some(absolute_root_path_for_cwd(cwd)),
            FileSystemSpecialPath::Minimal => None,
            FileSystemSpecialPath::ProjectRoots { subpath } => {
                Some(AbsolutePathBuf::resolve_path_against_base(
                    subpath.as_deref().unwrap_or_else(|| Path::new(".")),
                    cwd,
                ))
            }
            FileSystemSpecialPath::Tmpdir => {
                AbsolutePathBuf::from_absolute_path(std::env::temp_dir()).ok()
            }
            FileSystemSpecialPath::SlashTmp => {
                AbsolutePathBuf::from_absolute_path(PathBuf::from(r"C:\tmp")).ok()
            }
            FileSystemSpecialPath::Unknown { .. } => None,
        },
    }
}

fn absolute_root_path_for_cwd(cwd: &Path) -> AbsolutePathBuf {
    let absolute = AbsolutePathBuf::from_absolute_path(cwd).expect("cwd should resolve absolutely");
    let mut components = absolute.components();
    let mut root = PathBuf::new();
    if let Some(prefix) = components.next() {
        root.push(prefix.as_os_str());
    }
    if let Some(component) = components.next()
        && matches!(component, std::path::Component::RootDir)
    {
        root.push(component.as_os_str());
    }
    AbsolutePathBuf::from_absolute_path(root).expect("filesystem root should be absolute")
}

fn canonical_or_lexical(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(canonical_or_path_buf(path))
        .expect("resolved path should be absolute")
}

fn canonical_or_path_buf(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        canonicalize_path_allow_missing(path)
    }
    #[cfg(not(windows))]
    {
        dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }
}

fn dedup_paths(mut paths: Vec<AbsolutePathBuf>) -> Vec<AbsolutePathBuf> {
    paths.sort();
    paths.dedup();
    paths
}

fn glob_matches(pattern: &str, path: &Path, cwd: &Path) -> bool {
    let pattern = AbsolutePathBuf::resolve_path_against_base(pattern, cwd)
        .to_string_lossy()
        .into_owned();
    glob::Pattern::new(&pattern).is_ok_and(|pattern| {
        pattern.matches_path_with(
            path,
            glob::MatchOptions {
                case_sensitive: !cfg!(windows),
                require_literal_separator: false,
                require_literal_leading_dot: false,
            },
        )
    })
}

trait Pipe: Sized {
    fn pipe<T>(self, function: impl FnOnce(Self) -> T) -> T {
        function(self)
    }
}

impl<T> Pipe for T {}
