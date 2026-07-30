//! 开发期 Evaluation 二进制的命令边界回归。

use assert_cmd::Command;

#[test]
fn evaluation_runner_is_a_separate_development_binary() {
    let output = Command::cargo_bin("singularity-evaluation")
        .expect("evaluation binary")
        .arg("--help")
        .output()
        .expect("evaluation help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 help");
    assert!(stdout.contains("Development evaluator for the Singularity agent"));
    assert!(stdout.contains("run"));
}

#[test]
fn evaluation_runner_rejects_a_missing_manifest_before_runtime_setup() {
    let output = Command::cargo_bin("singularity-evaluation")
        .expect("evaluation binary")
        .args([
            "run",
            "missing-evaluation-manifest.json",
            "--run-id",
            "missing",
        ])
        .output()
        .expect("evaluation command");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf-8 error");
    assert!(stderr.contains("evaluation manifest not found"));
}
