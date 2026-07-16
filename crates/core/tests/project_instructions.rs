//! 项目指令 workspace 边界、大小限制和错误归因测试。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use singularity_core::{
    PROJECT_INSTRUCTIONS_MAX_FILE_BYTES, PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES,
    ProjectInstructionErrorCode, load_project_instructions, load_project_instructions_from_cwd,
};

static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let unique = format!(
            "singularity-project-instructions-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_nanos(),
            TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir(&path).expect("test temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct AgentsEscape {
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for AgentsEscape {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(windows)]
impl Drop for AgentsEscape {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

#[test]
fn loads_agents_files_from_workspace_root_to_nested_cwd() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("crates").join("agent");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    std::fs::create_dir_all(workspace.join("sibling")).expect("sibling");
    std::fs::write(workspace.join("AGENTS.md"), "root instructions").expect("root agents");
    std::fs::write(
        workspace.join("crates").join("AGENTS.md"),
        "crate instructions",
    )
    .expect("crate agents");
    std::fs::write(cwd.join("AGENTS.md"), "agent instructions").expect("agent agents");
    std::fs::write(workspace.join("sibling").join("AGENTS.md"), "must not load")
        .expect("sibling agents");

    let loaded = load_project_instructions(&workspace, &cwd)
        .expect("load project instructions")
        .expect("instructions present");

    assert_eq!(
        loaded
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        ["AGENTS.md", "crates/AGENTS.md", "crates/agent/AGENTS.md"]
    );
    assert!(
        loaded
            .sources
            .iter()
            .all(|source| source.content_digest.starts_with("sha256:")
                && source.content_digest.len() == "sha256:".len() + 64)
    );
    assert!(loaded.aggregate_digest.starts_with("sha256:"));
    assert_eq!(
        loaded.content,
        "root instructions\n\ncrate instructions\n\nagent instructions"
    );
    assert!(!loaded.content.contains("must not load"));
}

#[test]
fn discovers_git_workspace_root_from_nested_cwd() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("src").join("nested");
    std::fs::create_dir_all(workspace.join(".git")).expect("git marker");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    std::fs::write(workspace.join("AGENTS.md"), "root instructions").expect("root agents");
    std::fs::write(cwd.join("AGENTS.md"), "nested instructions").expect("nested agents");

    let loaded = load_project_instructions_from_cwd(&cwd)
        .expect("discover project instructions")
        .expect("instructions present");

    assert_eq!(
        loaded
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        ["AGENTS.md", "src/nested/AGENTS.md"]
    );
    assert_eq!(loaded.content, "root instructions\n\nnested instructions");
}

#[test]
fn missing_agents_files_are_not_an_error() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("src");
    std::fs::create_dir_all(&cwd).expect("nested cwd");

    assert_eq!(
        load_project_instructions(&workspace, &cwd).expect("missing is valid"),
        None
    );
}

#[test]
fn override_file_wins_once_per_hierarchy_layer() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("src");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    std::fs::write(workspace.join("AGENTS.md"), "root ordinary").expect("root agents");
    std::fs::write(workspace.join("AGENTS.override.md"), "root override").expect("root override");
    std::fs::write(cwd.join("AGENTS.md"), "cwd ordinary").expect("cwd agents");
    std::fs::write(cwd.join("AGENTS.override.md"), "cwd override").expect("cwd override");

    let loaded = load_project_instructions(&workspace, &cwd)
        .expect("load project instructions")
        .expect("instructions present");

    assert_eq!(loaded.content, "root override\n\ncwd override");
    assert_eq!(
        loaded
            .sources
            .iter()
            .map(|source| source.path.as_str())
            .collect::<Vec<_>>(),
        ["AGENTS.override.md", "src/AGENTS.override.md"]
    );
}

#[test]
fn source_and_aggregate_digests_change_when_instruction_content_changes() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("src");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    let agents = cwd.join("AGENTS.md");
    std::fs::write(&agents, "first instructions").expect("agents");

    let first = load_project_instructions(&workspace, &cwd)
        .expect("load first instructions")
        .expect("first instructions present");
    std::fs::write(&agents, "second instructions").expect("updated agents");
    let second = load_project_instructions(&workspace, &cwd)
        .expect("load second instructions")
        .expect("second instructions present");

    assert_ne!(
        first.sources[0].content_digest,
        second.sources[0].content_digest
    );
    assert_ne!(first.aggregate_digest, second.aggregate_digest);
}

#[test]
fn rejects_cwd_outside_workspace() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&outside).expect("outside");

    let error = load_project_instructions(&workspace, &outside).expect_err("outside cwd");

    assert_eq!(
        error.code,
        ProjectInstructionErrorCode::WorkingDirectoryOutsideWorkspace
    );
}

#[test]
fn rejects_agents_path_that_resolves_outside_workspace() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let cwd = workspace.join("src");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(outside.join("AGENTS.md"), "outside instructions").expect("outside agents");
    let _escape = create_agents_escape(&cwd.join("AGENTS.md"), &outside);

    let error = load_project_instructions(&workspace, &cwd).expect_err("escape rejected");

    assert_eq!(
        error.code,
        ProjectInstructionErrorCode::PathOutsideWorkspace
    );
    assert_eq!(error.path.as_deref(), Some(Path::new("src/AGENTS.md")));
}

#[test]
fn rejects_single_agents_file_over_named_limit() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        workspace.join("AGENTS.md"),
        vec![b'x'; PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1],
    )
    .expect("oversized agents");

    let error = load_project_instructions(&workspace, &workspace).expect_err("file too large");

    assert_eq!(error.code, ProjectInstructionErrorCode::FileTooLarge);
    assert_eq!(error.path.as_deref(), Some(Path::new("AGENTS.md")));
}

#[test]
fn rejects_hierarchy_over_named_total_limit() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let first = workspace.join("first");
    let cwd = first.join("second");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    let per_file_bytes = (PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES / 3) + 1;
    assert!(per_file_bytes <= PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    for directory in [&workspace, &first, &cwd] {
        std::fs::write(directory.join("AGENTS.md"), vec![b'x'; per_file_bytes])
            .expect("agents file");
    }

    let error = load_project_instructions(&workspace, &cwd).expect_err("total too large");

    assert_eq!(error.code, ProjectInstructionErrorCode::TotalTooLarge);
    assert_eq!(
        error.path.as_deref(),
        Some(Path::new("first/second/AGENTS.md"))
    );
}

#[cfg(unix)]
fn create_agents_escape(candidate: &Path, outside: &Path) -> AgentsEscape {
    std::os::unix::fs::symlink(outside.join("AGENTS.md"), candidate).expect("agents symlink");
    AgentsEscape {
        path: candidate.to_path_buf(),
    }
}

#[cfg(windows)]
fn create_agents_escape(candidate: &Path, outside: &Path) -> AgentsEscape {
    let output = std::process::Command::new("cmd.exe")
        .args([
            "/C",
            "mklink",
            "/J",
            candidate.to_str().expect("candidate path"),
            outside.to_str().expect("outside path"),
        ])
        .output()
        .expect("create agents junction");
    assert!(
        output.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    AgentsEscape {
        path: candidate.to_path_buf(),
    }
}
