use std::fs;
use std::path::Path;

const PRODUCT_IDENTITY_FILE: &str = "product_identity.rs";
const FORBIDDEN_CODEX_OR_PTY_REFERENCES: &[&str] = &[
    "OpenAI",
    "OpenAI/Codex",
    ".codex",
    "codex-command-runner",
    "codex-windows-sandbox-setup",
    "conpty",
    "unified_exec",
    "stdio_bridge",
    "wrapper::",
];
const CENTRALIZED_PRODUCT_IDENTITY_LITERALS: &[&str] = &[
    "SgSandboxOffline",
    "SgSandboxOnline",
    "SingularitySandboxUsers",
    "SingularitySandboxDesktop",
    "singularity-windows-sandbox-setup.exe",
    "singularity-command-runner.exe",
    "singularity_sandbox_offline",
    "singularity_wfp_",
    "SINGULARITY_HOME",
    "SINGULARITY_NETWORK_ALLOW_LOCAL_BINDING",
];

#[test]
fn runtime_sources_do_not_reference_forbidden_codex_paths_or_pty_surfaces() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let violations =
        collect_literal_violations(&source_root, FORBIDDEN_CODEX_OR_PTY_REFERENCES, |_| true);

    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn product_identity_literals_are_centralized() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let violations = collect_literal_violations(
        &source_root,
        CENTRALIZED_PRODUCT_IDENTITY_LITERALS,
        |path| path.file_name().and_then(|name| name.to_str()) != Some(PRODUCT_IDENTITY_FILE),
    );

    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn restricted_children_are_job_bound_before_they_can_run() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let process_source = fs::read_to_string(source_root.join("process.rs")).expect("read process");
    let runner_source = fs::read_to_string(
        source_root
            .join("bin")
            .join("command_runner")
            .join("win.rs"),
    )
    .expect("read command runner");
    let library_source = fs::read_to_string(source_root.join("lib.rs")).expect("read library");

    assert!(
        process_source.contains(
            "CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED"
        ),
        "explicit-stdio launches must create the child suspended"
    );
    assert!(
        process_source.contains("CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED"),
        "inherited-stdio launches must create the child suspended"
    );
    let assign = process_source
        .find("job.assign_process(process_info.hProcess)")
        .expect("job assignment must be mandatory");
    let resume = process_source
        .find("ResumeThread(process_info.hThread)")
        .expect("the primary thread must be resumed explicitly");
    assert!(
        assign < resume,
        "job assignment must happen before ResumeThread"
    );
    assert!(
        !runner_source.contains("create_job_kill_on_close().ok()"),
        "the elevated runner must not ignore Job Object creation failures"
    );
    assert!(
        !runner_source.contains("TerminateProcess("),
        "runner cancellation and timeout must terminate the whole Job Object"
    );
    assert!(
        runner_source.contains("job.terminate_and_wait(process, 1)")
            && runner_source.contains("job.terminate_and_wait(pi.hProcess, 1)"),
        "runner cancellation and timeout must both terminate and await the Job Object"
    );
    assert!(
        library_source.contains("job.terminate_and_wait(pi.hProcess, 1)"),
        "restricted-token fallback cancellation and timeout must terminate the Job Object"
    );
}

fn collect_literal_violations(
    root: &Path,
    forbidden_literals: &[&str],
    should_scan: impl Fn(&Path) -> bool,
) -> Vec<String> {
    let mut violations = Vec::new();
    visit_rust_files(root, &mut |path| {
        if !should_scan(path) {
            return;
        }

        let text = fs::read_to_string(path).expect("read source");
        for forbidden in forbidden_literals {
            if text.contains(forbidden) {
                violations.push(format!("{} contains {forbidden}", path.display()));
            }
        }
    });
    violations
}

fn visit_rust_files(root: &Path, visit: &mut impl FnMut(&Path)) {
    if !root.exists() {
        return;
    }
    for entry in fs::read_dir(root).expect("read source directory") {
        let entry = entry.expect("read source entry");
        let path = entry.path();
        if path.is_dir() {
            visit_rust_files(&path, visit);
        } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
            visit(&path);
        }
    }
}
