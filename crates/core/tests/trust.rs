//! 项目信任决策解析与 trust.json 存储测试（对齐 Pi project-trust.js 语义）。

use std::path::{Path, PathBuf};

use singularity_core::{TrustDecisions, TrustDefault, TrustResolution, resolve_project_trusted};

/// 创建带 AGENTS.md 信任资源的项目目录（无记录时走默认策略）。
fn project_with_agents(base: &Path, name: &str) -> PathBuf {
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).expect("project dir");
    std::fs::write(dir.join("AGENTS.md"), "project instructions").expect("agents file");
    dir
}

#[test]
fn project_without_trust_resource_is_directly_trusted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = temp.path().join("plain");
    std::fs::create_dir_all(&project).expect("project dir");
    let decisions = TrustDecisions::load(temp.path());

    // 即使有 never 记录或 never 默认，无 AGENTS.md 仍直接信任（对齐 Pi 顺序）。
    let mut decisions = decisions;
    decisions.set(&project, false).expect("write never record");
    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Never, false),
        TrustResolution::Trusted
    );
}

#[test]
fn recorded_decision_wins_over_default() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = project_with_agents(temp.path(), "recorded");
    let mut decisions = TrustDecisions::load(temp.path());
    decisions.set(&project, true).expect("set trusted");

    // 记录 true 覆盖 never 默认。
    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Never, false),
        TrustResolution::Trusted
    );
    decisions.set(&project, false).expect("set never");
    // 记录 false 覆盖 always 默认。
    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Always, false),
        TrustResolution::NotTrusted
    );
}

#[test]
fn unrecorded_default_always_and_never() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = project_with_agents(temp.path(), "defaults");
    let decisions = TrustDecisions::load(temp.path());

    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Always, false),
        TrustResolution::Trusted
    );
    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Never, true),
        TrustResolution::NotTrusted
    );
}

#[test]
fn ask_default_requires_interactive_ui() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = project_with_agents(temp.path(), "ask");
    let decisions = TrustDecisions::load(temp.path());

    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Ask, false),
        TrustResolution::NotTrusted
    );
    assert_eq!(
        resolve_project_trusted(&project, &decisions, TrustDefault::Ask, true),
        TrustResolution::AskNeeded
    );
}

#[test]
fn storage_round_trips_through_trust_json() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = project_with_agents(temp.path(), "storage");
    let mut decisions = TrustDecisions::load(temp.path());
    assert_eq!(decisions.get(&project), None);

    decisions.set(&project, true).expect("set trusted");
    assert_eq!(decisions.get(&project), Some(true));

    // 从磁盘重新加载后记录仍然存在（原子写落盘）。
    let reloaded = TrustDecisions::load(temp.path());
    assert_eq!(reloaded.get(&project), Some(true));

    let mut decisions = reloaded;
    decisions.set(&project, false).expect("set never");
    assert_eq!(decisions.get(&project), Some(false));
    let reloaded = TrustDecisions::load(temp.path());
    assert_eq!(reloaded.get(&project), Some(false));

    let mut decisions = reloaded;
    decisions.remove(&project).expect("clear record");
    assert_eq!(decisions.get(&project), None);
    assert_eq!(TrustDecisions::load(temp.path()).get(&project), None);
}

#[test]
fn load_accepts_manually_written_version_one_file_and_ignores_corruption() {
    let temp = tempfile::tempdir().expect("temp dir");
    let project = project_with_agents(temp.path(), "manual");
    // 存储键为 canonical 路径（Windows 上含 \\?\ 前缀）。
    let canonical_key = std::fs::canonicalize(&project)
        .expect("canonical project")
        .to_string_lossy()
        .into_owned();
    std::fs::write(
        temp.path().join("trust.json"),
        serde_json::json!({
            "version": 1,
            "projects": { canonical_key: true },
        })
        .to_string(),
    )
    .expect("write trust.json");

    assert_eq!(TrustDecisions::load(temp.path()).get(&project), Some(true));

    // 损坏文件 fail-soft：空决策集。
    std::fs::write(temp.path().join("trust.json"), "{not json").expect("corrupt file");
    assert_eq!(TrustDecisions::load(temp.path()).get(&project), None);
}
