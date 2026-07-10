use singularity_windows_sandbox::{
    ManagedFileSystemPermissions, NetworkSandboxPolicy, PermissionProfile,
    ResolvedWindowsSandboxPermissions,
};

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
