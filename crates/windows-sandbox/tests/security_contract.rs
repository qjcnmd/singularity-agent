use singularity_windows_sandbox::{
    AbsolutePathBuf, ManagedFileSystemPermissions, NetworkSandboxPolicy, PermissionProfile,
    ResolvedWindowsSandboxPermissions, run_windows_sandbox_capture_with_filesystem_overrides,
};
use std::collections::HashMap;

#[test]
fn restricted_network_is_not_supported_by_restricted_token_fallback() {
    let profile = PermissionProfile::read_only();
    let resolved =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            &profile,
            &[],
        )
        .expect("managed profile should resolve");

    assert!(!resolved.supports_restricted_token_fallback());
}

#[test]
fn enabled_network_can_use_restricted_token_fallback() {
    let profile = PermissionProfile::Managed {
        file_system: ManagedFileSystemPermissions::Restricted {
            entries: singularity_windows_sandbox::FileSystemSandboxPolicy::read_only().entries,
            glob_scan_max_depth: None,
        },
        network: NetworkSandboxPolicy::Enabled,
    };
    let resolved =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            &profile,
            &[],
        )
        .expect("managed profile should resolve");

    assert!(resolved.supports_restricted_token_fallback());
}

#[test]
fn restricted_token_fallback_rejects_deny_read_overrides() {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let protected = workspace.join(".git");
    let sandbox_home = temp.path().join("sandbox-home");
    std::fs::create_dir_all(&protected).expect("create protected path");
    let workspace_root =
        AbsolutePathBuf::from_absolute_path_checked(&workspace).expect("absolute workspace");
    let protected =
        AbsolutePathBuf::from_absolute_path_checked(&protected).expect("absolute protected path");
    let profile = PermissionProfile::Managed {
        file_system: ManagedFileSystemPermissions::Restricted {
            entries: singularity_windows_sandbox::FileSystemSandboxPolicy::read_only().entries,
            glob_scan_max_depth: None,
        },
        network: NetworkSandboxPolicy::Enabled,
    };

    let error = match run_windows_sandbox_capture_with_filesystem_overrides(
        &profile,
        &[workspace_root],
        &sandbox_home,
        vec![
            "cmd.exe".to_string(),
            "/d".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ],
        &workspace,
        HashMap::new(),
        Some(5_000),
        None,
        &[protected],
        &[],
        false,
    ) {
        Ok(_) => panic!("WRITE_RESTRICTED fallback cannot enforce deny-read"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("deny-read overrides require the elevated Windows sandbox backend")
    );
}
