use std::path::{Path, PathBuf};

/// Resolve the app-server binary built for the current Cargo test target.
///
/// Cargo's compile-time path is authoritative when available. The fallback
/// derives the profile directory from this test executable, so a dedicated
/// `CARGO_TARGET_DIR` is honored without ever falling back to a workspace
/// profile binary from another build.
pub fn app_server_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_singularity_app_server") {
        let binary = PathBuf::from(path);
        assert!(
            binary.is_file(),
            "Cargo-provided CARGO_BIN_EXE_singularity_app_server does not point to a file: {}",
            binary.display()
        );
        return binary;
    }

    let current_exe = std::env::current_exe().expect("current test binary");
    let profile_dir = current_exe
        .parent()
        .and_then(Path::parent)
        .expect("Cargo test profile directory");
    let binary = profile_dir.join(format!(
        "singularity_app_server{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        binary.is_file(),
        "CARGO_BIN_EXE_singularity_app_server is unavailable and the current Cargo target does not contain {}",
        binary.display()
    );
    binary
}
