//! core 取消合同测试。
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

use singularity_core::CancellationToken;

#[test]
fn cloned_cancellation_tokens_share_one_monotonic_state() {
    let token = CancellationToken::new();
    let clone = token.clone();

    assert!(!token.is_cancelled());
    clone.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn singularity_home_boundary_uses_nearest_repository_and_missing_tail() {
    let directory = tempfile::tempdir().expect("repository boundary directory");
    let workspace = directory.path().join("workspace");
    let repository = workspace.join("repository");
    let nested = repository.join("nested");
    std::fs::create_dir_all(&nested).expect("create repository tree");
    std::fs::write(repository.join(".git"), b"gitdir: test").expect("create worktree marker");

    singularity_core::ensure_singularity_home_outside_workspace(&workspace, &nested)
        .expect("repository ancestor remains usable");
    let inside = repository.join("missing-home");
    let error = singularity_core::ensure_singularity_home_outside_workspace(&inside, &nested)
        .expect_err("repository descendant rejected");
    assert!(error.contains("current repository"));
}

#[cfg(windows)]
#[test]
fn singularity_home_boundary_is_case_insensitive_with_missing_tail() {
    let directory = tempfile::tempdir().expect("repository boundary directory");
    let repository = directory.path().join("CaseSensitiveRepo");
    let nested = repository.join("nested");
    std::fs::create_dir_all(&nested).expect("create repository tree");
    std::fs::create_dir(repository.join(".git")).expect("create repository marker");
    let case_variant = std::path::PathBuf::from(repository.to_string_lossy().to_ascii_lowercase())
        .join("missing-home");

    assert!(
        singularity_core::ensure_singularity_home_outside_workspace(&case_variant, &nested)
            .is_err(),
        "case variants of repository descendants must be rejected"
    );
}
