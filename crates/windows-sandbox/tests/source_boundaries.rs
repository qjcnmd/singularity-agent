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
