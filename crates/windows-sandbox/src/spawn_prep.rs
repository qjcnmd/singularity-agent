use crate::absolute_path::AbsolutePathBuf;
use crate::acl::add_allow_ace;
use crate::acl::add_deny_write_ace;
use crate::acl::allow_null_device;
use crate::acl::ensure_allow_write_aces;
use crate::allow::AllowDenyPaths;
use crate::allow::compute_allow_paths_for_permissions;
use crate::cap::workspace_cap_sid_for_cwd;
use crate::cap::workspace_write_cap_sid_for_root;
use crate::cap::workspace_write_root_contains_path;
use crate::cap::workspace_write_root_overlaps_path;
use crate::cap::workspace_write_root_specificity;
use crate::deny_read_acl::ensure_missing_protected_path_materialized;
use crate::deny_read_acl::plan_deny_read_acl_paths;
use crate::deny_read_state::sync_persistent_deny_read_acls;
use crate::env::apply_no_network_to_env;
use crate::env::ensure_non_interactive_pager;
use crate::env::inherit_path_env;
use crate::env::normalize_null_device_env;
use crate::logging::log_start;
use crate::path_normalization::canonicalize_path;
use crate::path_normalization::lexical_path_key;
use crate::path_safety::ensure_case_insensitive_acl_path;
use crate::permissions::PermissionProfile;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::sandbox_utils::ensure_sandbox_home_exists;
use crate::sandbox_utils::inject_git_safe_directory;
use crate::setup::effective_write_roots_for_permissions;
use crate::token::LocalSid;
use crate::token::create_readonly_token_with_cap;
use crate::token::create_workspace_write_token_with_caps_from;
use crate::token::get_current_token_for_restriction;
use crate::token::get_logon_sid_bytes;
use crate::workspace_acl::is_command_cwd_root;
use crate::workspace_acl::protect_workspace_agents_dir;
use crate::workspace_acl::protect_workspace_singularity_dir;
use anyhow::Result;
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::HANDLE;

pub(crate) struct SpawnContext {
    pub(crate) permissions: ResolvedWindowsSandboxPermissions,
    pub(crate) current_dir: PathBuf,
    pub(crate) logs_base_dir: Option<PathBuf>,
    pub(crate) uses_write_capabilities: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SpawnPrepOptions {
    pub(crate) inherit_path: bool,
    pub(crate) add_git_safe_directory: bool,
}

pub(crate) struct RestrictedTokenSecurity {
    pub(crate) h_token: HANDLE,
    pub(crate) readonly_sid: Option<LocalSid>,
    pub(crate) readonly_sid_str: Option<String>,
    pub(crate) write_root_sids: Vec<RootCapabilitySid>,
}

pub(crate) struct RootCapabilitySid {
    pub(crate) root: PathBuf,
    pub(crate) sid: LocalSid,
    pub(crate) sid_str: String,
}

pub(crate) struct RestrictedTokenAclSids<'a> {
    pub(crate) readonly_sid: Option<&'a LocalSid>,
    pub(crate) readonly_sid_str: Option<&'a str>,
    pub(crate) write_root_sids: &'a [RootCapabilitySid],
}

/// Fully validated ACL inputs consumed by restricted-token security setup.
pub(crate) struct RestrictedTokenAclPlan {
    allow: Vec<PathBuf>,
    deny: Vec<PathBuf>,
    deny_read: Vec<PathBuf>,
}

fn prepare_spawn_context_common(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    sandbox_home: &Path,
    cwd: &Path,
    env_map: &mut HashMap<String, String>,
    command: &[String],
    options: SpawnPrepOptions,
) -> Result<SpawnContext> {
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            workspace_roots,
        )?;

    normalize_null_device_env(env_map);
    ensure_non_interactive_pager(env_map);
    if options.inherit_path {
        inherit_path_env(env_map);
    }
    if options.add_git_safe_directory {
        inject_git_safe_directory(env_map, cwd);
    }

    ensure_sandbox_home_exists(sandbox_home)?;
    let sandbox_base = sandbox_home.join(".sandbox");
    std::fs::create_dir_all(&sandbox_base)?;
    let logs_base_dir = Some(sandbox_base);
    log_start(command, logs_base_dir.as_deref());

    let uses_write_capabilities = permissions.uses_write_capabilities_for_cwd(cwd, env_map);

    Ok(SpawnContext {
        permissions,
        current_dir: cwd.to_path_buf(),
        logs_base_dir,
        uses_write_capabilities,
    })
}

pub(crate) fn prepare_restricted_token_spawn_context(
    permission_profile: &PermissionProfile,
    workspace_roots: &[AbsolutePathBuf],
    sandbox_home: &Path,
    cwd: &Path,
    env_map: &mut HashMap<String, String>,
    command: &[String],
    options: SpawnPrepOptions,
) -> Result<SpawnContext> {
    let common = prepare_spawn_context_common(
        permission_profile,
        workspace_roots,
        sandbox_home,
        cwd,
        env_map,
        command,
        options,
    )?;
    if common.permissions.should_apply_network_block() {
        apply_no_network_to_env(env_map)?;
    }
    Ok(common)
}

pub(crate) fn prepare_restricted_token_security(
    uses_write_capabilities: bool,
    sandbox_home: &Path,
    cwd: &Path,
    capability_roots: impl IntoIterator<Item = PathBuf>,
) -> Result<RestrictedTokenSecurity> {
    let (h_token, readonly_sid, readonly_sid_str, write_root_sids) = unsafe {
        if uses_write_capabilities {
            let write_root_sids = root_capability_sids(sandbox_home, cwd, capability_roots)?;
            if write_root_sids.is_empty() {
                anyhow::bail!("workspace-write sandbox has no writable root capability SIDs");
            }
            let base = get_current_token_for_restriction()?;
            let cap_ptrs: Vec<*mut c_void> = write_root_sids
                .iter()
                .map(|root| root.sid.as_ptr())
                .collect();
            let h_token = create_workspace_write_token_with_caps_from(base, cap_ptrs.as_slice());
            CloseHandle(base);
            let h_token = h_token?;
            (h_token, None, None, write_root_sids)
        } else {
            let sid_str = workspace_cap_sid_for_cwd(sandbox_home, cwd)?;
            let psid = LocalSid::from_string(&sid_str)?;
            let (h_token, _psid) = create_readonly_token_with_cap(psid.as_ptr())?;
            (h_token, Some(psid), Some(sid_str), Vec::new())
        }
    };

    Ok(RestrictedTokenSecurity {
        h_token,
        readonly_sid,
        readonly_sid_str,
        write_root_sids,
    })
}

pub(crate) fn restricted_token_capability_roots(
    permissions: &ResolvedWindowsSandboxPermissions,
    current_dir: &Path,
    env_map: &HashMap<String, String>,
    sandbox_home: &Path,
) -> Vec<PathBuf> {
    let allow_paths = compute_allow_paths_for_permissions(permissions, current_dir, env_map)
        .allow
        .into_iter()
        .collect::<Vec<_>>();
    if permissions.uses_write_capabilities_for_cwd(current_dir, env_map) {
        effective_write_roots_for_permissions(
            permissions,
            current_dir,
            env_map,
            sandbox_home,
            Some(allow_paths.as_slice()),
        )
    } else {
        allow_paths
    }
}

pub(crate) fn root_capability_sids(
    sandbox_home: &Path,
    cwd: &Path,
    allow_paths: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<RootCapabilitySid>> {
    let mut roots: Vec<PathBuf> = allow_paths.into_iter().collect();
    roots.sort_by_key(|root| canonicalize_path(root.as_path()));
    roots.dedup_by(|a, b| canonicalize_path(a.as_path()) == canonicalize_path(b.as_path()));

    // Validate the complete compound request before the first capability SID is persisted.
    ensure_case_insensitive_acl_path(cwd)?;
    for root in &roots {
        ensure_case_insensitive_acl_path(root)?;
    }

    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        let sid_str = workspace_write_cap_sid_for_root(sandbox_home, cwd, &root)?;
        let sid = LocalSid::from_string(&sid_str)?;
        out.push(RootCapabilitySid { root, sid, sid_str });
    }
    Ok(out)
}

fn matching_root_capability<'a>(
    path: &Path,
    root_sids: &'a [RootCapabilitySid],
) -> Option<&'a RootCapabilitySid> {
    root_sids
        .iter()
        .filter(|root_sid| workspace_write_root_contains_path(&root_sid.root, path))
        .max_by_key(|root_sid| workspace_write_root_specificity(&root_sid.root))
}

fn deny_root_capabilities_for_path<'a>(
    path: &Path,
    root_sids: &'a [RootCapabilitySid],
) -> Vec<&'a RootCapabilitySid> {
    let matching_root_sids = root_sids
        .iter()
        .filter(|root_sid| workspace_write_root_overlaps_path(&root_sid.root, path))
        .collect::<Vec<_>>();
    if matching_root_sids.is_empty() {
        root_sids.iter().collect()
    } else {
        matching_root_sids
    }
}

pub(crate) fn allow_null_device_for_workspace_write(is_workspace_write: bool) {
    if !is_workspace_write {
        return;
    }

    unsafe {
        if let Ok(base) = get_current_token_for_restriction() {
            if let Ok(bytes) = get_logon_sid_bytes(base) {
                let mut bytes = bytes;
                let psid = bytes.as_mut_ptr() as *mut c_void;
                allow_null_device(psid);
            }
            CloseHandle(base);
        }
    }
}

pub(crate) fn plan_restricted_token_acl_rules(
    permissions: &ResolvedWindowsSandboxPermissions,
    current_dir: &Path,
    env_map: &HashMap<String, String>,
    additional_deny_read_paths: &[PathBuf],
    additional_deny_write_paths: &[PathBuf],
) -> Result<RestrictedTokenAclPlan> {
    let AllowDenyPaths { allow, mut deny } =
        compute_allow_paths_for_permissions(permissions, current_dir, env_map);
    deny.extend(additional_deny_write_paths.iter().cloned());
    let deny_read = plan_deny_read_acl_paths(additional_deny_read_paths)?;
    for path in &allow {
        ensure_case_insensitive_acl_path(path)?;
    }
    for path in &deny {
        ensure_case_insensitive_acl_path(path)?;
    }
    Ok(RestrictedTokenAclPlan {
        allow: allow.into_iter().collect(),
        deny: deny.into_iter().collect(),
        deny_read,
    })
}

pub(crate) fn apply_restricted_token_acl_rules(
    plan: &RestrictedTokenAclPlan,
    sandbox_home: &Path,
    current_dir: &Path,
    acl_sids: RestrictedTokenAclSids<'_>,
) -> Result<()> {
    let mut materialized = HashMap::new();
    for path in &plan.deny {
        // Explicit carveouts must exist before the command starts so the sandbox cannot create
        // them under a writable parent first. The helper rejects reparse-point ancestors.
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                materialized.insert(
                    lexical_path_key(path),
                    ensure_missing_protected_path_materialized(path)?,
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    unsafe {
        if let Some(readonly_sid) = acl_sids.readonly_sid {
            for p in &plan.allow {
                add_allow_ace(p, readonly_sid.as_ptr())?;
            }
        } else {
            for p in &plan.allow {
                let Some(root_sid) = matching_root_capability(p, acl_sids.write_root_sids) else {
                    continue;
                };
                ensure_allow_write_aces(p, &[root_sid.sid.as_ptr()])?;
            }
        }
        for p in &plan.deny {
            for root_sid in deny_root_capabilities_for_path(p, acl_sids.write_root_sids) {
                if let Some(materialized) = materialized.get(&lexical_path_key(p)) {
                    materialized.add_deny_write_ace(root_sid.sid.as_ptr())?;
                } else {
                    add_deny_write_ace(p, root_sid.sid.as_ptr())?;
                }
            }
        }
        if !plan.deny_read.is_empty() {
            if let Some(readonly_sid) = acl_sids.readonly_sid {
                let Some(readonly_sid_str) = acl_sids.readonly_sid_str else {
                    anyhow::bail!("readonly capability SID string missing");
                };
                sync_persistent_deny_read_acls(
                    sandbox_home,
                    readonly_sid_str,
                    &plan.deny_read,
                    readonly_sid.as_ptr(),
                )?;
            } else {
                for root_sid in acl_sids.write_root_sids {
                    sync_persistent_deny_read_acls(
                        sandbox_home,
                        &root_sid.sid_str,
                        &plan.deny_read,
                        root_sid.sid.as_ptr(),
                    )?;
                }
            }
        }
        for root_sid in acl_sids.write_root_sids {
            allow_null_device(root_sid.sid.as_ptr());
        }
        if let Some(readonly_sid) = acl_sids.readonly_sid {
            allow_null_device(readonly_sid.as_ptr());
        }
        if !acl_sids.write_root_sids.is_empty()
            && let Some(workspace_sid) =
                matching_root_capability(current_dir, acl_sids.write_root_sids)
        {
            let canonical_cwd = canonicalize_path(current_dir);
            if is_command_cwd_root(&workspace_sid.root, &canonical_cwd) {
                protect_workspace_singularity_dir(current_dir, workspace_sid.sid.as_ptr())?;
                protect_workspace_agents_dir(current_dir, workspace_sid.sid.as_ptr())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SpawnPrepOptions;
    use super::deny_root_capabilities_for_path;
    use super::prepare_restricted_token_spawn_context;
    use super::prepare_spawn_context_common;
    use super::restricted_token_capability_roots;
    use super::root_capability_sids;
    use crate::absolute_path::AbsolutePathBuf;
    use crate::cap::cap_sid_file;
    use crate::cap::load_or_create_cap_sids;
    use crate::cap::workspace_write_cap_sid_for_root;
    use crate::path_safety::CaseSensitivityTestOutcome;
    use crate::path_safety::ProtectedMetadataError;
    use crate::path_safety::override_case_sensitivity_for_test;
    use crate::permissions::NetworkSandboxPolicy;
    use crate::permissions::PermissionProfile;
    use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::path::Path;
    use tempfile::TempDir;

    fn workspace_profile(
        network_policy: NetworkSandboxPolicy,
        writable_roots: &[AbsolutePathBuf],
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    ) -> PermissionProfile {
        PermissionProfile::workspace_write_with(
            writable_roots,
            network_policy,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        )
    }

    fn workspace_roots_for(root: &Path) -> Vec<AbsolutePathBuf> {
        vec![AbsolutePathBuf::from_absolute_path(root).expect("absolute workspace root")]
    }

    fn should_apply_network_block(permission_profile: &PermissionProfile) -> bool {
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            permission_profile,
            &[],
        )
        .expect("managed permission profile")
        .should_apply_network_block()
    }

    #[test]
    fn no_network_env_rewrite_applies_for_workspace_write() {
        assert!(should_apply_network_block(
            &PermissionProfile::workspace_write()
        ));
    }

    #[test]
    fn no_network_env_rewrite_skips_when_network_access_is_allowed() {
        assert!(!should_apply_network_block(&workspace_profile(
            NetworkSandboxPolicy::Enabled,
            &[],
            /*exclude_tmpdir_env_var*/ false,
            /*exclude_slash_tmp*/ false,
        )));
    }

    #[test]
    fn restricted_token_spawn_env_applies_offline_network_rewrite() {
        let sandbox_home = TempDir::new().expect("tempdir");
        let cwd = TempDir::new().expect("tempdir");
        let mut env_map = HashMap::new();
        let workspace_roots = workspace_roots_for(cwd.path());

        let _context = prepare_restricted_token_spawn_context(
            &PermissionProfile::workspace_write(),
            workspace_roots.as_slice(),
            sandbox_home.path(),
            cwd.path(),
            &mut env_map,
            &["cmd.exe".to_string()],
            SpawnPrepOptions {
                inherit_path: true,
                add_git_safe_directory: false,
            },
        )
        .expect("restricted-token env prep");

        assert_eq!(env_map.get("SBX_NONET_ACTIVE"), Some(&"1".to_string()));
        assert_eq!(
            env_map.get("HTTP_PROXY"),
            Some(&"http://127.0.0.1:9".to_string())
        );
    }

    #[test]
    fn common_spawn_env_keeps_network_env_unchanged() {
        let sandbox_home = TempDir::new().expect("tempdir");
        let cwd = TempDir::new().expect("tempdir");
        let mut env_map = HashMap::from([(
            "HTTP_PROXY".to_string(),
            "http://user.proxy:8080".to_string(),
        )]);
        let workspace_roots = workspace_roots_for(cwd.path());

        let context = prepare_spawn_context_common(
            &PermissionProfile::workspace_write(),
            workspace_roots.as_slice(),
            sandbox_home.path(),
            cwd.path(),
            &mut env_map,
            &["cmd.exe".to_string()],
            SpawnPrepOptions {
                inherit_path: true,
                add_git_safe_directory: true,
            },
        )
        .expect("preserve existing env prep");
        assert!(context.uses_write_capabilities);

        assert_eq!(env_map.get("SBX_NONET_ACTIVE"), None);
        assert_eq!(
            env_map.get("HTTP_PROXY"),
            Some(&"http://user.proxy:8080".to_string())
        );
    }

    #[test]
    fn restricted_token_capability_roots_use_runtime_workspace_roots_for_workspace_root() {
        let tmp = TempDir::new().expect("tempdir");
        let sandbox_home = tmp.path().join("singularity-home");
        let workspace_root = tmp.path().join("workspace");
        let command_cwd = workspace_root.join("subdir");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
        std::fs::create_dir_all(&command_cwd).expect("create command cwd");

        let permission_profile = workspace_profile(
            NetworkSandboxPolicy::Restricted,
            &[],
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        );
        let workspace_roots = workspace_roots_for(workspace_root.as_path());
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                workspace_roots.as_slice(),
            )
            .expect("managed permission profile");

        let roots = restricted_token_capability_roots(
            &permissions,
            &command_cwd,
            &HashMap::new(),
            &sandbox_home,
        );

        assert_eq!(
            roots,
            vec![dunce::canonicalize(&workspace_root).expect("canonical workspace root")]
        );
    }

    #[test]
    fn root_capability_sids_only_include_active_roots() {
        let temp = TempDir::new().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let active_root = temp.path().join("active-root");
        let stale_root = temp.path().join("stale-root");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&active_root).expect("create active root");
        std::fs::create_dir_all(&stale_root).expect("create stale root");

        let stale_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &stale_root)
            .expect("stale sid");
        let active_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &active_root)
            .expect("active sid");
        let workspace_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let caps = load_or_create_cap_sids(&sandbox_home).expect("load caps");

        let sid_strs = root_capability_sids(
            &sandbox_home,
            &workspace,
            vec![workspace.clone(), active_root],
        )
        .expect("root capabilities")
        .into_iter()
        .map(|root_sid| root_sid.sid_str)
        .collect::<Vec<_>>();

        assert_eq!(sid_strs.len(), 2);
        assert!(sid_strs.contains(&workspace_sid));
        assert!(sid_strs.contains(&active_sid));
        assert!(!sid_strs.contains(&stale_sid));
        assert!(!sid_strs.contains(&caps.workspace));
    }

    #[test]
    fn root_capability_batch_rejects_before_persisting_any_sid() {
        let temp = TempDir::new().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let ordinary_root = temp.path().join("a-ordinary-root");
        let rejected_root = temp.path().join("z-rejected-root");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&ordinary_root).expect("create ordinary root");
        std::fs::create_dir_all(&rejected_root).expect("create rejected root");
        let _case_sensitive = override_case_sensitivity_for_test(
            &rejected_root,
            CaseSensitivityTestOutcome::CaseSensitive,
        );

        let error = match root_capability_sids(
            &sandbox_home,
            &workspace,
            vec![ordinary_root, rejected_root.clone()],
        ) {
            Ok(_) => panic!("compound capability request must fail before SID persistence"),
            Err(error) => error,
        };

        assert_eq!(
            error.downcast_ref::<ProtectedMetadataError>(),
            Some(&ProtectedMetadataError::CaseSensitiveDirectoryUnsupported {
                path: rejected_root,
            })
        );
        assert!(
            !cap_sid_file(&sandbox_home).exists(),
            "failed batch must not persist a SID for an earlier root"
        );
    }

    #[test]
    fn restricted_token_deny_path_includes_nested_active_root_sid() {
        let temp = TempDir::new().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let protected_dir = workspace.join(".singularity");
        let nested_root = protected_dir.join("nested-root");
        let unrelated_root = temp.path().join("unrelated-root");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&nested_root).expect("create nested root");
        std::fs::create_dir_all(&unrelated_root).expect("create unrelated root");

        let workspace_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &workspace)
            .expect("workspace sid");
        let nested_sid = workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &nested_root)
            .expect("nested sid");
        let unrelated_sid =
            workspace_write_cap_sid_for_root(&sandbox_home, &workspace, &unrelated_root)
                .expect("unrelated sid");
        let root_sids = root_capability_sids(
            &sandbox_home,
            &workspace,
            vec![workspace.clone(), nested_root, unrelated_root],
        )
        .expect("root capabilities");

        let deny_sid_strs = deny_root_capabilities_for_path(&protected_dir, &root_sids)
            .into_iter()
            .map(|root_sid| root_sid.sid_str.clone())
            .collect::<Vec<_>>();

        assert_eq!(deny_sid_strs, vec![workspace_sid, nested_sid]);
        assert!(!deny_sid_strs.contains(&unrelated_sid));
    }

    #[test]
    fn restricted_token_capability_roots_use_effective_write_roots() {
        let temp = TempDir::new().expect("tempdir");
        let sandbox_home = temp.path().join("singularity-home");
        let workspace = temp.path().join("workspace");
        let active_root = temp.path().join("active-root");
        let sandbox_root = sandbox_home.join(".sandbox");
        std::fs::create_dir_all(&sandbox_home).expect("create singularity home");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&active_root).expect("create active root");
        std::fs::create_dir_all(&sandbox_root).expect("create sandbox root");

        let writable_roots = vec![
            AbsolutePathBuf::try_from(active_root.as_path()).expect("active root"),
            AbsolutePathBuf::try_from(sandbox_home.as_path()).expect("singularity home"),
            AbsolutePathBuf::try_from(sandbox_root.as_path()).expect("sandbox root"),
        ];
        let permission_profile = workspace_profile(
            NetworkSandboxPolicy::Restricted,
            &writable_roots,
            /*exclude_tmpdir_env_var*/ true,
            /*exclude_slash_tmp*/ true,
        );
        let workspace_roots = workspace_roots_for(workspace.as_path());
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                workspace_roots.as_slice(),
            )
            .expect("managed permission profile");

        let roots = restricted_token_capability_roots(
            &permissions,
            &workspace,
            &HashMap::new(),
            &sandbox_home,
        );

        assert!(roots.contains(&dunce::canonicalize(&workspace).expect("workspace")));
        assert!(roots.contains(&dunce::canonicalize(&active_root).expect("active root")));
        assert!(!roots.contains(&dunce::canonicalize(&sandbox_home).expect("singularity home")));
        assert!(!roots.contains(&dunce::canonicalize(&sandbox_root).expect("sandbox root")));
    }
}
