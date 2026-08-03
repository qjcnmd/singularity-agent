use crate::path_normalization::canonicalize_path_allow_missing;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct AllowDenyPaths {
    pub allow: HashSet<PathBuf>,
    pub deny: HashSet<PathBuf>,
}

pub(crate) fn compute_allow_paths_for_permissions(
    permissions: &ResolvedWindowsSandboxPermissions,
    command_cwd: &Path,
    env_map: &HashMap<String, String>,
) -> AllowDenyPaths {
    let mut allow: HashSet<PathBuf> = HashSet::new();
    let mut deny: HashSet<PathBuf> = HashSet::new();

    let mut add_allow_path = |p: PathBuf| {
        if p.exists() {
            allow.insert(p);
        }
    };
    for writable_root in permissions.writable_roots_for_cwd(command_cwd, env_map) {
        let canonical = canonicalize_path_allow_missing(&writable_root.root);
        add_allow_path(canonical);
        for read_only_subpath in writable_root.read_only_subpaths {
            let read_only_subpath = canonicalize_path_allow_missing(&read_only_subpath);
            // Existing generated metadata paths are protected; explicit missing deny entries
            // remain in the plan so the ACL setup can materialize them fail-closed.
            deny.insert(read_only_subpath);
        }
    }

    AllowDenyPaths { allow, deny }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::absolute_path::AbsolutePathBuf;
    use crate::permissions::NetworkSandboxPolicy;
    use crate::permissions::PermissionProfile;
    use singularity_core::PROTECTED_METADATA_PATH_NAMES;
    use std::fs;
    use tempfile::TempDir;

    fn workspace_write_profile(
        writable_roots: &[AbsolutePathBuf],
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> PermissionProfile {
        PermissionProfile::workspace_write_with(
            writable_roots,
            NetworkSandboxPolicy::Restricted,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        )
    }

    fn workspace_roots_for(root: &Path) -> Vec<AbsolutePathBuf> {
        vec![AbsolutePathBuf::from_absolute_path(root).expect("absolute workspace root")]
    }

    fn compute_allow_paths(
        permission_profile: &PermissionProfile,
        workspace_roots: &[AbsolutePathBuf],
        command_cwd: &Path,
        env_map: &HashMap<String, String>,
    ) -> AllowDenyPaths {
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                permission_profile,
                workspace_roots,
            )
            .expect("managed permission profile");
        compute_allow_paths_for_permissions(&permissions, command_cwd, env_map)
    }

    fn expected_existing_protected_paths(roots: &[&Path]) -> HashSet<PathBuf> {
        roots
            .iter()
            .flat_map(|root| {
                let canonical = dunce::canonicalize(root).expect("canonical protected root");
                PROTECTED_METADATA_PATH_NAMES
                    .iter()
                    .map(move |name| canonical.join(name))
                    .filter(|path| fs::symlink_metadata(path).is_ok())
            })
            .collect()
    }

    fn expected_all_protected_paths(root: &Path) -> HashSet<PathBuf> {
        let canonical = dunce::canonicalize(root).expect("canonical protected root");
        PROTECTED_METADATA_PATH_NAMES
            .iter()
            .map(|name| canonical.join(name))
            .collect()
    }

    #[test]
    fn includes_additional_writable_roots() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let extra_root = tmp.path().join("extra");
        let _ = fs::create_dir_all(&command_cwd);
        let _ = fs::create_dir_all(&extra_root);

        let writable_roots = vec![AbsolutePathBuf::try_from(extra_root.as_path()).unwrap()];
        let permission_profile = workspace_write_profile(
            &writable_roots,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );

        assert!(
            paths
                .allow
                .contains(&dunce::canonicalize(&command_cwd).unwrap())
        );
        assert!(
            paths
                .allow
                .contains(&dunce::canonicalize(&extra_root).unwrap())
        );
        let expected_deny = expected_existing_protected_paths(&[&command_cwd, &extra_root]);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn uses_runtime_workspace_roots_for_workspace_root() {
        let tmp = TempDir::new().expect("tempdir");
        let workspace_root = tmp.path().join("workspace");
        let command_cwd = workspace_root.join("subdir");
        fs::create_dir_all(&command_cwd).expect("create command cwd");

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        );
        let workspace_roots = workspace_roots_for(workspace_root.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );

        assert!(
            paths
                .allow
                .contains(&dunce::canonicalize(&workspace_root).unwrap())
        );
        assert!(
            !paths
                .allow
                .contains(&dunce::canonicalize(&command_cwd).unwrap())
        );
        let expected_deny = expected_existing_protected_paths(&[&workspace_root]);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn excludes_tmp_env_vars_when_requested() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        let _ = fs::create_dir_all(&command_cwd);
        let _ = fs::create_dir_all(&temp_dir);

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let mut env_map = HashMap::new();
        env_map.insert("TEMP".into(), temp_dir.to_string_lossy().to_string());
        env_map.insert("TMP".into(), temp_dir.to_string_lossy().to_string());
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &env_map,
        );

        assert!(
            paths
                .allow
                .contains(&dunce::canonicalize(&command_cwd).unwrap())
        );
        assert!(
            !paths
                .allow
                .contains(&dunce::canonicalize(&temp_dir).unwrap())
        );
        let expected_deny = expected_existing_protected_paths(&[&command_cwd]);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn includes_tmp_env_vars_when_requested() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let temp_dir = tmp.path().join("temp");
        let _ = fs::create_dir_all(&command_cwd);
        let _ = fs::create_dir_all(&temp_dir);

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ false,
            /*exclude_slash_tmp*/ false,
        );
        let mut env_map = HashMap::new();
        env_map.insert("TEMP".into(), temp_dir.to_string_lossy().to_string());
        env_map.insert("TMP".into(), temp_dir.to_string_lossy().to_string());
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &env_map,
        );

        let expected_allow: HashSet<PathBuf> = [
            dunce::canonicalize(&command_cwd).unwrap(),
            dunce::canonicalize(&temp_dir).unwrap(),
        ]
        .into_iter()
        .collect();

        assert_eq!(expected_allow, paths.allow);
        let mut expected_deny = expected_existing_protected_paths(&[&command_cwd]);
        expected_deny.extend(expected_all_protected_paths(&temp_dir));
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn ignores_unix_slash_tmp_for_windows_allow_roots() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let _ = fs::create_dir_all(&command_cwd);

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );
        let expected_allow: HashSet<PathBuf> = [dunce::canonicalize(&command_cwd).unwrap()]
            .into_iter()
            .collect();

        assert_eq!(expected_allow, paths.allow);
        let expected_deny = expected_existing_protected_paths(&[&command_cwd]);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn denies_git_dir_inside_writable_root() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let git_dir = command_cwd.join(".git");
        let _ = fs::create_dir_all(&git_dir);

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );
        let expected_allow: HashSet<PathBuf> = [dunce::canonicalize(&command_cwd).unwrap()]
            .into_iter()
            .collect();
        let expected_deny = expected_existing_protected_paths(&[&command_cwd]);

        assert_eq!(expected_allow, paths.allow);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn denies_git_file_inside_writable_root() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let git_file = command_cwd.join(".git");
        let _ = fs::create_dir_all(&command_cwd);
        let _ = fs::write(&git_file, "gitdir: .git/worktrees/example");

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );
        let expected_allow: HashSet<PathBuf> = [dunce::canonicalize(&command_cwd).unwrap()]
            .into_iter()
            .collect();
        let expected_deny = expected_existing_protected_paths(&[&command_cwd]);

        assert_eq!(expected_allow, paths.allow);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn denies_singularity_and_agents_inside_writable_root() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let singularity_dir = command_cwd.join(".singularity");
        let agents_dir = command_cwd.join(".agents");
        let _ = fs::create_dir_all(&singularity_dir);
        let _ = fs::create_dir_all(&agents_dir);

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );
        let expected_allow: HashSet<PathBuf> = [dunce::canonicalize(&command_cwd).unwrap()]
            .into_iter()
            .collect();
        let expected_deny = expected_existing_protected_paths(&[&command_cwd]);

        assert_eq!(expected_allow, paths.allow);
        assert_eq!(expected_deny, paths.deny);
    }

    #[test]
    fn skips_missing_default_metadata_dirs() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let _ = fs::create_dir_all(&command_cwd);

        let permission_profile = workspace_write_profile(
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ false,
        );
        let workspace_roots = workspace_roots_for(command_cwd.as_path());

        let paths = compute_allow_paths(
            &permission_profile,
            workspace_roots.as_slice(),
            &command_cwd,
            &HashMap::new(),
        );
        assert_eq!(paths.allow.len(), 1);
        assert!(paths.deny.is_empty());
        for name in PROTECTED_METADATA_PATH_NAMES {
            assert!(!command_cwd.join(name).exists());
        }
    }

    #[test]
    fn preserves_explicit_missing_deny_paths() {
        let tmp = TempDir::new().expect("tempdir");
        let command_cwd = tmp.path().join("workspace");
        let explicit_deny = command_cwd.join("blocked");
        fs::create_dir_all(&command_cwd).expect("create workspace");
        let workspace_root =
            AbsolutePathBuf::from_absolute_path(&command_cwd).expect("absolute workspace root");
        let explicit_deny =
            AbsolutePathBuf::from_absolute_path(&explicit_deny).expect("absolute explicit deny");
        let profile = PermissionProfile::Managed {
            file_system: crate::permissions::ManagedFileSystemPermissions::Restricted {
                entries: vec![
                    crate::permissions::FileSystemSandboxEntry::new(
                        crate::permissions::FileSystemPath::Path {
                            path: workspace_root,
                        },
                        crate::permissions::FileSystemAccessMode::Write,
                    ),
                    crate::permissions::FileSystemSandboxEntry::new(
                        crate::permissions::FileSystemPath::Path {
                            path: explicit_deny.clone(),
                        },
                        crate::permissions::FileSystemAccessMode::Deny,
                    ),
                ],
                glob_scan_max_depth: None,
            },
            network: NetworkSandboxPolicy::Restricted,
        };
        let paths = compute_allow_paths(
            &profile,
            &workspace_roots_for(&command_cwd),
            &command_cwd,
            &HashMap::new(),
        );

        assert!(!explicit_deny.as_path().exists());
        assert!(
            paths
                .deny
                .contains(&canonicalize_path_allow_missing(explicit_deny.as_path()))
        );
    }
}
