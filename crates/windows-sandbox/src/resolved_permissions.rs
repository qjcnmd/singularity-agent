use crate::absolute_path::AbsolutePathBuf;
use crate::path_normalization::canonicalize_path_allow_missing;
use crate::permissions::FileSystemPath;
use crate::permissions::FileSystemSandboxEntry;
use crate::permissions::FileSystemSandboxKind;
use crate::permissions::FileSystemSandboxPolicy;
use crate::permissions::NetworkSandboxPolicy;
use crate::permissions::PermissionProfile;
use anyhow::Result;
use singularity_core::PROTECTED_METADATA_PATH_NAMES;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

/// Windows-local view of the runtime permission profile.
///
/// Most Windows sandbox code needs resolved runtime permissions plus a few
/// Windows-specific path conventions, not the user/config-facing
/// `PermissionProfile` enum itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWindowsSandboxPermissions {
    file_system: FileSystemSandboxPolicy,
    network: NetworkSandboxPolicy,
    protect_workspace_metadata: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowsWritableRoot {
    pub(crate) root: PathBuf,
    pub(crate) read_only_subpaths: Vec<PathBuf>,
}

/// Restricted-token family needed to enforce a Windows permission profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsSandboxTokenMode {
    ReadOnlyCapability,
    WritableRootsCapability,
}

/// Chooses the restricted-token family needed for a managed permission profile.
pub fn token_mode_for_permission_profile(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    cwd: &Path,
    env_map: &HashMap<String, String>,
) -> Result<WindowsSandboxTokenMode> {
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )?;
    if permissions.file_system.has_full_disk_write_access() {
        anyhow::bail!(
            "permission profile requests full-disk filesystem writes, which cannot be enforced by the Windows sandbox"
        );
    }
    if permissions.writable_roots_for_cwd(cwd, env_map).is_empty() {
        Ok(WindowsSandboxTokenMode::ReadOnlyCapability)
    } else {
        Ok(WindowsSandboxTokenMode::WritableRootsCapability)
    }
}

impl ResolvedWindowsSandboxPermissions {
    pub fn try_from_permission_profile(permission_profile: &PermissionProfile) -> Result<Self> {
        if !matches!(permission_profile, PermissionProfile::Managed { .. }) {
            anyhow::bail!(
                "only managed permission profiles can be enforced by the Windows sandbox"
            );
        }
        let (file_system, network) = permission_profile.to_runtime_permissions();
        if !matches!(file_system.kind, FileSystemSandboxKind::Restricted) {
            anyhow::bail!(
                "only restricted managed filesystem permissions can be enforced by the Windows sandbox"
            );
        }
        Ok(Self {
            file_system,
            network,
            protect_workspace_metadata: true,
        })
    }

    /// Resolves a managed permission profile and binds symbolic `:workspace_roots`
    /// entries to the workspace roots supplied by the caller.
    pub fn try_from_permission_profile_for_workspace_roots(
        permission_profile: &PermissionProfile,
        workspace_roots: &[AbsolutePathBuf],
    ) -> Result<Self> {
        Self::try_from_permission_profile_for_workspace_roots_with_protected_metadata(
            permission_profile,
            workspace_roots,
            true,
        )
    }

    /// Resolves a managed profile while controlling only generated workspace metadata defaults.
    ///
    /// Explicit deny entries remain part of the resolved filesystem policy.
    pub fn try_from_permission_profile_for_workspace_roots_with_protected_metadata(
        permission_profile: &PermissionProfile,
        workspace_roots: &[AbsolutePathBuf],
        protect_workspace_metadata: bool,
    ) -> Result<Self> {
        let mut permissions = Self::try_from_permission_profile(permission_profile)?;
        permissions.file_system = permissions
            .file_system
            .materialize_project_roots_with_workspace_roots(workspace_roots);
        permissions.protect_workspace_metadata = protect_workspace_metadata;
        Ok(permissions)
    }

    pub fn supports_restricted_token_fallback(&self) -> bool {
        self.network.is_enabled() && self.has_full_disk_read_access()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn should_apply_network_block(&self) -> bool {
        !self.network.is_enabled()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn network_policy(&self) -> NetworkSandboxPolicy {
        self.network
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn is_enforceable_by_windows_sandbox(&self) -> bool {
        matches!(self.file_system.kind, FileSystemSandboxKind::Restricted)
    }

    pub(crate) fn has_full_disk_read_access(&self) -> bool {
        self.file_system.has_full_disk_read_access()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn include_platform_defaults(&self) -> bool {
        self.file_system.include_platform_defaults()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn readable_roots_for_cwd(&self, cwd: &Path) -> Vec<PathBuf> {
        self.file_system
            .get_readable_roots_with_cwd(cwd)
            .into_iter()
            .map(AbsolutePathBuf::into_path_buf)
            .collect()
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn uses_write_capabilities_for_cwd(
        &self,
        cwd: &Path,
        env_map: &HashMap<String, String>,
    ) -> bool {
        !self.writable_roots_for_cwd(cwd, env_map).is_empty()
    }

    pub(crate) fn writable_roots_for_cwd(
        &self,
        cwd: &Path,
        env_map: &HashMap<String, String>,
    ) -> Vec<WindowsWritableRoot> {
        let mut file_system = self.file_system.clone();
        // Default metadata protections apply only when their objects already exist. Explicit
        // non-write entries keep their original fail-closed missing-path behavior.
        file_system.remove_skip_missing_path_entries();
        file_system
            .entries
            .retain(|FileSystemSandboxEntry { path, .. }| {
                !matches!(
                    path,
                    FileSystemPath::Special {
                        value: crate::permissions::FileSystemSpecialPath::Tmpdir
                            | crate::permissions::FileSystemSpecialPath::SlashTmp,
                    }
                )
            });

        let mut roots = file_system
            .get_writable_roots_with_cwd_and_protected_metadata(
                cwd,
                self.protect_workspace_metadata,
            )
            .into_iter()
            .map(|root| WindowsWritableRoot {
                root: root.root.into_path_buf(),
                read_only_subpaths: root
                    .read_only_subpaths
                    .into_iter()
                    .map(AbsolutePathBuf::into_path_buf)
                    .collect(),
            })
            .collect::<Vec<_>>();

        if self.has_writable_tmpdir_entry() {
            roots.extend(windows_temp_env_roots(env_map).into_iter().map(|root| {
                let read_only_subpaths = if self.protect_workspace_metadata {
                    PROTECTED_METADATA_PATH_NAMES
                        .iter()
                        .map(|name| root.join(name))
                        .collect()
                } else {
                    Vec::new()
                };
                WindowsWritableRoot {
                    root,
                    read_only_subpaths,
                }
            }));
        }

        roots
    }

    fn has_writable_tmpdir_entry(&self) -> bool {
        self.file_system
            .entries
            .iter()
            .any(|FileSystemSandboxEntry { path, access, .. }| {
                matches!(
                    path,
                    FileSystemPath::Special {
                        value: crate::permissions::FileSystemSpecialPath::Tmpdir,
                    }
                ) && access.can_write()
            })
    }
}

fn windows_temp_env_roots(env_map: &HashMap<String, String>) -> Vec<PathBuf> {
    let isolated_cargo_target = env_map.get("CARGO_TARGET_DIR").map(PathBuf::from);
    ["TEMP", "TMP"]
        .into_iter()
        .filter_map(|key| {
            env_map
                .get(key)
                .map(|value| PathBuf::from(value.as_str()))
                .or_else(|| std::env::var_os(key).map(PathBuf::from))
        })
        .filter(|path| path.is_absolute())
        .flat_map(|temp_root| {
            let mut roots = vec![temp_root.clone()];
            if let Some(target) = isolated_cargo_target.as_deref() {
                if let Some(cache_root) = existing_isolated_cargo_cache_root(&temp_root, target) {
                    roots.push(cache_root);
                }
            }
            roots
        })
        .collect()
}

/// Returns the nearest existing cache ancestor for a structurally valid isolated Cargo target.
fn existing_isolated_cargo_cache_root(temp_root: &Path, target: &Path) -> Option<PathBuf> {
    if !target.is_absolute() {
        return None;
    }

    let temp_root = canonicalize_path_allow_missing(temp_root);
    let target = canonicalize_path_allow_missing(target);
    let cache_root = temp_root.join("singularity-tool-cache");
    let cargo_root = cache_root.join("cargo");
    let mut relative = target.strip_prefix(&cargo_root).ok()?.components();
    let digest = relative.next()?.as_os_str().to_str()?;
    if relative.next().is_some()
        || digest.len() != 64
        || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }

    [cargo_root, cache_root]
        .into_iter()
        .find(|root| root.is_dir())
        .map(|root| canonicalize_path_allow_missing(&root))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    use crate::path_normalization::canonicalize_path_allow_missing;
    use crate::permissions::FileSystemAccessMode;
    use crate::permissions::FileSystemSandboxEntry;
    use crate::permissions::FileSystemSpecialPath;
    use crate::permissions::ManagedFileSystemPermissions;
    use crate::permissions::project_roots_glob_pattern;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    fn workspace_roots_for(root: &Path) -> Vec<AbsolutePathBuf> {
        vec![AbsolutePathBuf::from_absolute_path(root).expect("absolute workspace root")]
    }

    #[test]
    fn permission_profile_workspace_write_uses_windows_temp_env_vars() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let mut env_map = HashMap::new();
        env_map.insert("TEMP".to_string(), temp_dir.to_string_lossy().to_string());
        env_map.insert("TMP".to_string(), temp_dir.to_string_lossy().to_string());

        let permissions = ResolvedWindowsSandboxPermissions::try_from_permission_profile(
            &PermissionProfile::workspace_write(),
        )
        .expect("managed permission profile");
        let roots = permissions
            .writable_roots_for_cwd(&cwd, &env_map)
            .into_iter()
            .map(|root| root.root)
            .collect::<std::collections::HashSet<_>>();

        let expected_roots = [
            temp_dir,
            dunce::canonicalize(&cwd).expect("canonicalize cwd"),
        ]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();

        assert_eq!(expected_roots, roots);
    }

    #[test]
    fn permission_profile_workspace_write_includes_existing_isolated_cargo_cache_root() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        let cargo_root = temp_dir.join("singularity-tool-cache").join("cargo");
        let target_dir =
            cargo_root.join("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&target_dir).expect("create isolated cargo target");

        let env_map = HashMap::from([
            ("TEMP".to_string(), temp_dir.to_string_lossy().to_string()),
            (
                "CARGO_TARGET_DIR".to_string(),
                target_dir.to_string_lossy().to_string(),
            ),
        ]);

        let permissions = ResolvedWindowsSandboxPermissions::try_from_permission_profile(
            &PermissionProfile::workspace_write(),
        )
        .expect("managed permission profile");
        let roots = permissions
            .writable_roots_for_cwd(&cwd, &env_map)
            .into_iter()
            .map(|root| root.root)
            .collect::<Vec<_>>();

        assert!(
            roots.contains(&dunce::canonicalize(&cargo_root).expect("canonical cargo root")),
            "existing isolated cargo cache root must be writable: {roots:?}"
        );
    }

    #[test]
    fn permission_profile_workspace_write_rejects_non_isolated_cargo_targets() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        let cache_root = temp_dir.join("singularity-tool-cache");
        let cargo_root = cache_root.join("cargo");
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&cargo_root).expect("create cargo cache root");

        let cases = [
            (
                "target outside TEMP",
                tmp.path().join("outside").join("cargo").join(digest),
            ),
            ("wrong digest", cargo_root.join("not-a-digest")),
            ("nested target", cargo_root.join(digest).join("nested")),
        ];
        let permissions = ResolvedWindowsSandboxPermissions::try_from_permission_profile(
            &PermissionProfile::workspace_write(),
        )
        .expect("managed permission profile");
        let expected_cache_root = dunce::canonicalize(&cache_root).expect("canonical cache root");
        let expected_cargo_root = dunce::canonicalize(&cargo_root).expect("canonical cargo root");

        for (case, target_dir) in cases {
            std::fs::create_dir_all(&target_dir).expect("create candidate target");
            let env_map = HashMap::from([
                ("TEMP".to_string(), temp_dir.to_string_lossy().to_string()),
                (
                    "CARGO_TARGET_DIR".to_string(),
                    target_dir.to_string_lossy().to_string(),
                ),
            ]);
            let roots = permissions
                .writable_roots_for_cwd(&cwd, &env_map)
                .into_iter()
                .map(|root| root.root)
                .collect::<Vec<_>>();

            assert!(
                !roots.contains(&expected_cargo_root),
                "{case} must not add cargo root: {roots:?}"
            );
            assert!(
                !roots.contains(&expected_cache_root),
                "{case} must not add cache root: {roots:?}"
            );
        }
    }

    #[test]
    fn trusted_preparation_omits_generated_metadata_but_keeps_explicit_denies() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let explicit_git_deny = cwd.join(".git");
        let mut entries = FileSystemSandboxPolicy::workspace_write(&[], true, true).entries;
        entries.push(FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: AbsolutePathBuf::from_absolute_path(&explicit_git_deny)
                    .expect("absolute explicit deny"),
            },
            access: FileSystemAccessMode::Deny,
            missing_path_behavior: None,
        });
        let profile = PermissionProfile::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries,
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        };
        let permissions = ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots_with_protected_metadata(
            &profile,
            workspace_roots_for(&cwd).as_slice(),
            false,
        )
        .expect("trusted preparation permissions");

        let root = permissions
            .writable_roots_for_cwd(&cwd, &HashMap::new())
            .into_iter()
            .find(|root| root.root == dunce::canonicalize(&cwd).expect("canonical cwd"))
            .expect("workspace writable root");

        #[cfg(windows)]
        let expected_git_deny = canonicalize_path_allow_missing(&explicit_git_deny);
        #[cfg(not(windows))]
        let expected_git_deny = explicit_git_deny;
        assert_eq!(root.read_only_subpaths, vec![expected_git_deny]);
        assert!(!root.read_only_subpaths.contains(&cwd.join(".agents")));
        assert!(!root.read_only_subpaths.contains(&cwd.join(".singularity")));
    }

    #[test]
    fn permission_profile_workspace_root_uses_runtime_workspace_roots() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace_root = tmp.path().join("workspace");
        let command_cwd = workspace_root.join("subdir");
        std::fs::create_dir_all(&command_cwd).expect("create command cwd");

        let permission_profile = PermissionProfile::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: vec![FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                    },
                    access: FileSystemAccessMode::Write,
                    missing_path_behavior: None,
                }],
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        };
        let workspace_roots = workspace_roots_for(workspace_root.as_path());
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                workspace_roots.as_slice(),
            )
            .expect("managed permission profile");

        let roots = permissions
            .writable_roots_for_cwd(&command_cwd, &HashMap::new())
            .into_iter()
            .map(|root| root.root)
            .collect::<Vec<_>>();

        assert_eq!(
            roots,
            vec![dunce::canonicalize(&workspace_root).expect("canonical workspace root")]
        );
    }

    #[test]
    fn permission_profile_workspace_roots_expand_all_runtime_workspace_roots() {
        let tmp = TempDir::new().expect("tempdir");
        let first = AbsolutePathBuf::from_absolute_path(tmp.path().join("first"))
            .expect("absolute first root");
        let second = AbsolutePathBuf::from_absolute_path(tmp.path().join("second"))
            .expect("absolute second root");
        let permission_profile = PermissionProfile::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                        },
                        access: FileSystemAccessMode::Write,
                        missing_path_behavior: None,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::project_roots(Some(".git".into())),
                        },
                        access: FileSystemAccessMode::Deny,
                        missing_path_behavior: None,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: project_roots_glob_pattern(Path::new("**/*.env")),
                        },
                        access: FileSystemAccessMode::Deny,
                        missing_path_behavior: None,
                    },
                ],
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        };

        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                &[first.clone(), second.clone()],
            )
            .expect("managed permission profile");

        assert_eq!(
            permissions.file_system,
            FileSystemSandboxPolicy::restricted(vec![
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: first.clone(),
                    },
                    access: FileSystemAccessMode::Write,
                    missing_path_behavior: None,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: second.clone(),
                    },
                    access: FileSystemAccessMode::Write,
                    missing_path_behavior: None,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: first.join(".git"),
                    },
                    access: FileSystemAccessMode::Deny,
                    missing_path_behavior: None,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::Path {
                        path: second.join(".git"),
                    },
                    access: FileSystemAccessMode::Deny,
                    missing_path_behavior: None,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: AbsolutePathBuf::resolve_path_against_base(
                            "**/*.env",
                            first.as_path(),
                        )
                        .to_string_lossy()
                        .into_owned(),
                    },
                    access: FileSystemAccessMode::Deny,
                    missing_path_behavior: None,
                },
                FileSystemSandboxEntry {
                    path: FileSystemPath::GlobPattern {
                        pattern: AbsolutePathBuf::resolve_path_against_base(
                            "**/*.env",
                            second.as_path(),
                        )
                        .to_string_lossy()
                        .into_owned(),
                    },
                    access: FileSystemAccessMode::Deny,
                    missing_path_behavior: None,
                },
            ])
        );
    }

    #[test]
    fn token_mode_for_profile_without_writable_roots_uses_readonly_capability() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let workspace_roots = workspace_roots_for(cwd.as_path());

        let token_mode = token_mode_for_permission_profile(
            &PermissionProfile::read_only(),
            workspace_roots.as_slice(),
            &cwd,
            &HashMap::new(),
        )
        .expect("token mode");

        assert_eq!(WindowsSandboxTokenMode::ReadOnlyCapability, token_mode);
    }

    #[test]
    fn token_mode_for_profile_with_writable_roots_uses_write_capabilities() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let workspace_roots = workspace_roots_for(cwd.as_path());

        let token_mode = token_mode_for_permission_profile(
            &PermissionProfile::workspace_write(),
            workspace_roots.as_slice(),
            &cwd,
            &HashMap::new(),
        )
        .expect("token mode");

        assert_eq!(WindowsSandboxTokenMode::WritableRootsCapability, token_mode);
    }

    #[test]
    fn permission_profile_rejects_disabled_profiles() {
        let err = ResolvedWindowsSandboxPermissions::try_from_permission_profile(
            &PermissionProfile::Disabled,
        )
        .expect_err("disabled profile should not resolve for sandbox enforcement");

        assert!(
            err.to_string()
                .contains("only managed permission profiles can be enforced")
        );
    }

    #[test]
    fn permission_profile_rejects_unrestricted_managed_filesystem() {
        let permission_profile = PermissionProfile::Managed {
            file_system: ManagedFileSystemPermissions::Unrestricted,
            network: NetworkSandboxPolicy::Restricted,
        };

        let err =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile(&permission_profile)
                .expect_err("unrestricted profile should not resolve for sandbox enforcement");

        assert!(
            err.to_string()
                .contains("only restricted managed filesystem permissions can be enforced")
        );
    }

    #[test]
    fn token_mode_rejects_full_disk_write_entries() {
        let tmp = TempDir::new().expect("tempdir");
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let permission_profile = PermissionProfile::Managed {
            file_system: ManagedFileSystemPermissions::Restricted {
                entries: vec![FileSystemSandboxEntry {
                    path: FileSystemPath::Special {
                        value: FileSystemSpecialPath::Root,
                    },
                    access: FileSystemAccessMode::Write,
                    missing_path_behavior: None,
                }],
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        };
        let workspace_roots = workspace_roots_for(cwd.as_path());

        let err = token_mode_for_permission_profile(
            &permission_profile,
            workspace_roots.as_slice(),
            &cwd,
            &HashMap::new(),
        )
        .expect_err("full disk writes should not resolve to a token mode");

        assert!(
            err.to_string()
                .contains("full-disk filesystem writes, which cannot be enforced")
        );
    }
}
