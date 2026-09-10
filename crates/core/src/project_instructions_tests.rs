#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use super::*;

fn write_file(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// 层级合并：root→cwd 逐层指令按目录顺序拼接，空文件不进入正文。
#[test]
fn merges_instructions_root_to_cwd_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    let nested = root.join("crates").join("core");
    write_file(&root.join(PROJECT_INSTRUCTIONS_FILE_NAME), "root rules");
    write_file(
        &root.join("crates").join(PROJECT_INSTRUCTIONS_FILE_NAME),
        "   ",
    );
    write_file(&nested.join(PROJECT_INSTRUCTIONS_FILE_NAME), "crate rules");
    let instructions = load_agent_instructions(&nested, &root.join(".singularity"))
        .unwrap()
        .expect("instructions found");
    assert!(
        instructions.content().find("root rules").unwrap()
            < instructions.content().find("crate rules").unwrap()
    );
    assert!(!instructions.content().contains("crates/AGENTS.md"));
    assert!(!instructions.truncated());
}

/// 单文件预算截断：超过单文件预算的正文只保留有效 UTF-8 前缀并标记截断；
/// 无文件时投影为 None。
#[test]
fn truncates_over_budget_file_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    // 多字节字符跨预算边界：前缀必须停在字符边界上。
    let filler = "é".repeat(PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    write_file(&root.join(PROJECT_INSTRUCTIONS_FILE_NAME), &filler);
    let instructions = load_agent_instructions(root, &root.join(".singularity"))
        .unwrap()
        .expect("instructions found");
    assert!(instructions.truncated());
    assert!(instructions.content().len() <= PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES);
    assert!(instructions.content().ends_with('é'));

    let empty = tempfile::tempdir().unwrap();
    std::fs::create_dir(empty.path().join(".git")).unwrap();
    assert!(
        load_agent_instructions(empty.path(), &empty.path().join(".singularity"))
            .unwrap()
            .is_none()
    );
}

/// 合并预算截断：每个文件单独都在预算内，但 root→cwd 累计超过总预算时，
/// 后续文件只纳入剩余预算内的前缀并标记截断，正文总长不超过总预算。
#[test]
fn truncates_cumulative_merge_at_total_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    // 每份都小于单文件预算，三份合计超过总预算。
    let chunk = "a".repeat(PROJECT_INSTRUCTIONS_MAX_FILE_BYTES - 1024);
    write_file(&root.join(PROJECT_INSTRUCTIONS_FILE_NAME), &chunk);
    let mid = root.join("packages");
    write_file(&mid.join(PROJECT_INSTRUCTIONS_FILE_NAME), &chunk);
    let leaf = mid.join("app");
    write_file(&leaf.join(PROJECT_INSTRUCTIONS_FILE_NAME), &chunk);

    let instructions = load_agent_instructions(&leaf, &root.join(".singularity"))
        .unwrap()
        .expect("instructions found");
    assert!(
        instructions.truncated(),
        "cumulative content beyond the total budget must be reported as truncated"
    );
    assert!(
        instructions.content().len() <= PROJECT_INSTRUCTIONS_MAX_TOTAL_BYTES,
        "merged content must respect the total budget"
    );
}

#[test]
fn global_and_project_instructions_have_sources_and_reload_changes() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write_file(&home.path().join("AGENTS.md"), "global rule");
    write_file(&project.path().join("AGENTS.md"), "project rule");
    let first = load_agent_instructions(project.path(), home.path())
        .unwrap()
        .unwrap();
    assert!(
        first.content().find("global rule").unwrap()
            < first.content().find("project rule").unwrap()
    );
    assert!(first.content().contains(&home.path().display().to_string()));
    std::fs::remove_file(home.path().join("AGENTS.md")).unwrap();
    write_file(&project.path().join("AGENTS.md"), "new project rule");
    let second = load_agent_instructions(project.path(), home.path())
        .unwrap()
        .unwrap();
    assert!(!second.content().contains("global rule"));
    assert!(second.content().contains("new project rule"));
}

#[test]
fn invalid_sources_fail_with_the_affected_path() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let missing = project.path().join("missing");
    let error = load_agent_instructions(&missing, home.path()).unwrap_err();
    assert!(error.contains(&missing.display().to_string()));
    assert!(error.contains("unavailable"));

    let instructions = project.path().join(PROJECT_INSTRUCTIONS_FILE_NAME);
    std::fs::write(&instructions, [0xff]).unwrap();
    let error = load_agent_instructions(&instructions, home.path()).unwrap_err();
    assert!(error.contains(&instructions.display().to_string()));
    assert!(error.contains("not a directory"));

    let error = load_agent_instructions(project.path(), home.path()).unwrap_err();
    assert!(error.contains("AGENTS.md"));
    assert!(error.contains("invalid_utf8"));

    let directory_source = home.path().join(PROJECT_INSTRUCTIONS_FILE_NAME);
    std::fs::create_dir(&directory_source).unwrap();
    let error = load_agent_instructions(project.path(), home.path()).unwrap_err();
    assert!(error.contains(&directory_source.display().to_string()));
    assert!(error.contains("unsupported_file_type"));
}
