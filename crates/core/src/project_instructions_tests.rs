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
    let instructions = load_project_instructions(root, &nested)
        .unwrap()
        .expect("instructions found");
    assert_eq!(
        instructions.content(),
        format!("root rules{PROJECT_INSTRUCTIONS_SEPARATOR}crate rules")
    );
    assert!(!instructions.truncated());
}

/// 单文件预算截断：超过单文件预算的正文只保留有效 UTF-8 前缀并标记截断；
/// 无文件时投影为 `None`。
#[test]
fn truncates_over_budget_file_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    // 多字节字符跨预算边界：前缀必须停在字符边界上。
    let filler = "é".repeat(PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    write_file(&root.join(PROJECT_INSTRUCTIONS_FILE_NAME), &filler);
    let instructions = load_project_instructions(root, root)
        .unwrap()
        .expect("instructions found");
    assert!(instructions.truncated());
    assert!(instructions.content().len() <= PROJECT_INSTRUCTIONS_MAX_FILE_BYTES);
    assert!(instructions.content().ends_with('é'));

    let empty = tempfile::tempdir().unwrap();
    std::fs::create_dir(empty.path().join(".git")).unwrap();
    assert!(
        load_project_instructions(empty.path(), empty.path())
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

    let instructions = load_project_instructions(root, &leaf)
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
