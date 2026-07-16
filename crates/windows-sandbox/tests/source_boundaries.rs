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
fn read_acl_and_deny_read_follow_codex_principal_boundaries() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let setup_source =
        fs::read_to_string(source_root.join("bin").join("setup_main").join("win.rs"))
            .expect("read setup helper");
    let library_source = fs::read_to_string(source_root.join("lib.rs")).expect("read library");

    assert!(
        !setup_source.contains("workspace_capability_psid")
            && !setup_source.contains("granting {access_label} ACE to {} for workspace capability"),
        "Codex read grants must not be projected onto per-workspace capability SIDs"
    );
    assert!(
        setup_source.contains(
            "Codex uses the dedicated Sandbox Users group as the authoritative read principal."
        ) && setup_source.contains("&sandbox_group_sid_str")
            && setup_source.contains("read_acl_mutex_exists()"),
        "elevated read grants and deny-read ACLs must use the Codex Sandbox Users boundary"
    );
    assert!(
        library_source.contains("WRITE_RESTRICTED tokens consult restricting SIDs only for writes")
            && library_source
                .contains("deny-read overrides require the elevated Windows sandbox backend"),
        "restricted-token fallback must reject deny-read overrides"
    );
}

#[test]
fn null_device_compatibility_grant_is_best_effort_like_codex() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let acl_source = fs::read_to_string(source_root.join("acl.rs")).expect("read ACL source");
    let elevated_source =
        fs::read_to_string(source_root.join("elevated_impl.rs")).expect("read elevated source");
    let runner_source = fs::read_to_string(
        source_root
            .join("bin")
            .join("command_runner")
            .join("win.rs"),
    )
    .expect("read command runner");

    assert!(
        acl_source.contains("pub unsafe fn allow_null_device(psid: *mut c_void) {")
            && acl_source.contains("if handle == 0 || handle == INVALID_HANDLE_VALUE"),
        "NUL compatibility must remain a best-effort Codex helper"
    );
    assert!(
        !elevated_source.contains("allow_null_device(sid_for_null.as_ptr())?")
            && !runner_source.contains("allow_null_device(cap_psid_ptrs[0])?"),
        "NUL WRITE_DAC availability must not decide whether strict sandbox enforcement exists"
    );
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
        runner_source.contains("job.terminate(1)")
            && runner_source.contains("job.terminate_and_wait(pi.hProcess, 1)"),
        "runner transport, cancellation, and timeout paths must terminate the whole Job Object"
    );
    assert!(
        runner_source.contains("runner control pipe closed before child completion")
            && runner_source.contains("runner control pipe read failed"),
        "runner control-pipe EOF and read errors must be treated as containment failures"
    );
    let runner_cleanup = runner_source
        .find("let cleanup_error")
        .expect("runner must close the Job Object after waiting for the child");
    let runner_capture_join = runner_source
        .find("out_thread.join()")
        .expect("runner must join output readers");
    assert!(
        runner_cleanup < runner_capture_join,
        "runner Job cleanup must happen before output reader joins"
    );
    let spawn_ready = runner_source
        .find("message: Message::SpawnReady")
        .expect("runner must emit spawn_ready");
    let spawn_ready_write = runner_source[spawn_ready..]
        .find("write_frame(&mut *guard, &msg)")
        .map(|offset| spawn_ready + offset)
        .expect("runner must write spawn_ready");
    let capture_transfer = runner_source
        .find("let (pi, stdout_handle, stderr_handle) = ipc_spawn.take_capture_handles()")
        .expect("runner must transfer capture handles after spawn_ready");
    assert!(
        spawn_ready_write < capture_transfer,
        "spawn_ready failures must leave process and capture handles under RAII ownership"
    );
    assert!(
        library_source.contains("job.close().err()")
            && library_source.contains("job.terminate_and_wait(pi.hProcess, 1)"),
        "restricted-token fallback normal exit and cancellation/timeout must clean up the Job Object"
    );
    let library_cleanup = library_source
        .find("let cleanup_error")
        .expect("restricted-token capture must clean up the Job Object");
    let library_capture_join = library_source
        .find("stdout_reader.join()")
        .expect("restricted-token capture must join output readers");
    assert!(
        library_cleanup < library_capture_join,
        "restricted-token Job cleanup must happen before output reader joins"
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
