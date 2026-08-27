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
        loaded.content(),
        "root instructions\n\ncrate instructions\n\nagent instructions"
    );
    assert!(!loaded.content().contains("must not load"));
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

    assert_eq!(loaded.content(), "root instructions\n\nnested instructions");
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
fn instruction_content_changes_are_observed() {
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

    assert_eq!(first.content(), "first instructions");
    assert_eq!(second.content(), "second instructions");
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
fn truncates_single_agents_file_to_file_budget() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
        workspace.join("AGENTS.md"),
        vec![b'x'; PROJECT_INSTRUCTIONS_MAX_FILE_BYTES + 1],
    )
    .expect("oversized agents");

    let loaded = load_project_instructions(&workspace, &workspace)
        .expect("oversized loads via truncation")
        .expect("instructions present");

    assert_eq!(loaded.content().len(), PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    assert!(loaded.truncated(), "truncation fact must be observable");
}

#[test]
fn truncates_hierarchy_to_total_budget_and_stops() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let first = workspace.join("first");
    let cwd = first.join("second");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    let per_file_bytes = (PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES / 3) + 1;
    assert!(per_file_bytes <= PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    std::fs::write(workspace.join("AGENTS.md"), vec![b'a'; per_file_bytes]).expect("root agents");
    std::fs::write(first.join("AGENTS.md"), vec![b'b'; per_file_bytes]).expect("first agents");
    std::fs::write(cwd.join("AGENTS.md"), vec![b'c'; per_file_bytes]).expect("cwd agents");

    let loaded = load_project_instructions(&workspace, &cwd)
        .expect("over-budget hierarchy loads via truncation")
        .expect("instructions present");

    // 前两个文件全部纳入，第三个文件被截断到剩余预算，之后停止。
    assert_eq!(
        loaded.content().matches('a').count(),
        per_file_bytes,
        "root file fully incorporated"
    );
    assert_eq!(
        loaded.content().matches('b').count(),
        per_file_bytes,
        "first file fully incorporated"
    );
    assert_eq!(
        loaded.content().matches('c').count(),
        per_file_bytes - 2,
        "cwd file limited to remaining total budget"
    );
    assert!(loaded.truncated(), "truncation fact must be observable");
}

#[test]
fn whitespace_only_instruction_files_do_not_consume_budget() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let first = workspace.join("first");
    let cwd = first.join("second");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    // 根文件与 cwd 文件各占满单文件预算；中间只有空白字符的文件若被计入
    // 预算，cwd 文件就会被截断——空白文件必须零成本跳过。
    std::fs::write(
        workspace.join("AGENTS.md"),
        vec![b'a'; PROJECT_INSTRUCTIONS_MAX_FILE_BYTES],
    )
    .expect("root agents");
    std::fs::write(first.join("AGENTS.md"), "\n\n  ").expect("whitespace first agents");
    std::fs::write(
        cwd.join("AGENTS.md"),
        vec![b'b'; PROJECT_INSTRUCTIONS_MAX_FILE_BYTES],
    )
    .expect("cwd agents");

    let loaded = load_project_instructions(&workspace, &cwd)
        .expect("load succeeds")
        .expect("instructions present");

    assert!(
        !loaded.truncated(),
        "whitespace file must not push over budget"
    );
    assert_eq!(
        loaded.content().matches('b').count(),
        PROJECT_INSTRUCTIONS_MAX_FILE_BYTES,
        "cwd file fully incorporated"
    );
}

#[test]
fn budget_exhaustion_skips_remaining_files() {
    let temp = TestDir::new();
    let workspace = temp.path().join("workspace");
    let first = workspace.join("first");
    let cwd = first.join("second");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    // 前两个文件各占满单文件预算，合计耗尽 64KB 总预算。
    std::fs::write(
        workspace.join("AGENTS.md"),
        vec![b'a'; PROJECT_INSTRUCTIONS_MAX_FILE_BYTES],
    )
    .expect("root agents");
    std::fs::write(
        first.join("AGENTS.md"),
        vec![b'b'; PROJECT_INSTRUCTIONS_MAX_FILE_BYTES],
    )
    .expect("first agents");
    // 预算耗尽后不得再读取：目录形式的 AGENTS.md 一旦被读即报 UnsupportedFileType。
    std::fs::create_dir(cwd.join("AGENTS.md")).expect("cwd agents dir");

    let loaded = load_project_instructions(&workspace, &cwd)
        .expect("budget-exhausted load succeeds")
        .expect("instructions present");

    assert!(loaded.truncated(), "exhausted budget marks truncated");
    assert_eq!(
        loaded.content().matches('a').count(),
        PROJECT_INSTRUCTIONS_MAX_FILE_BYTES
    );
    assert_eq!(
        loaded.content().matches('b').count(),
        PROJECT_INSTRUCTIONS_MAX_FILE_BYTES
    );
}
