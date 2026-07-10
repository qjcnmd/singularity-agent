use singularity_windows_sandbox::product_identity::{
    COMMAND_RUNNER_BINARY_NAME, DEFAULT_USE_PRIVATE_DESKTOP, OFFLINE_ACCOUNT_NAME,
    ONLINE_ACCOUNT_NAME, SETUP_BINARY_NAME,
};

const _: () = assert!(DEFAULT_USE_PRIVATE_DESKTOP);

#[test]
fn singularity_product_identity_is_centralized() {
    assert_eq!(OFFLINE_ACCOUNT_NAME, "SingularitySandboxOffline");
    assert_eq!(ONLINE_ACCOUNT_NAME, "SingularitySandboxOnline");
    assert_eq!(SETUP_BINARY_NAME, "singularity-windows-sandbox-setup.exe");
    assert_eq!(COMMAND_RUNNER_BINARY_NAME, "singularity-command-runner.exe");
}
