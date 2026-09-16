#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn discovery_precedence_policy_and_lazy_reload() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let user = dir.path().join("user");
    fs::create_dir_all(project.join("review")).unwrap();
    fs::create_dir_all(&user).unwrap();
    let path = project.join("review/SKILL.md");
    fs::write(&path, "---\nname: review\ndescription: >\n  Review code\n  carefully\ndisable-model-invocation: true\n---\nProject instructions").unwrap();
    fs::write(
        user.join("review.md"),
        "---\nname: review\ndescription: User review\n---\nShadowed",
    )
    .unwrap();
    fs::write(
        user.join("hidden.md"),
        "---\nname: hidden\ndescription: Auto only\nuser-invocable: false\n---\nHidden body",
    )
    .unwrap();
    fs::write(user.join("broken.md"), "---\nname: [invalid\n---\n").unwrap();
    let catalog = SkillCatalog::from_roots(&[project, user]);
    assert_eq!(catalog.skills.len(), 2);
    assert_eq!(catalog.diagnostics.len(), 1);
    assert!(catalog.diagnostics[0].contains("broken.md"));
    let manual = catalog.manual(" /review this diff").unwrap();
    assert_eq!(manual.path, path);
    assert_eq!(manual.description.trim(), "Review code carefully");
    assert!(catalog.manual("please /review").is_none());
    assert!(catalog.manual("/hidden").is_none());
    assert!(!catalog.prompt().contains("- review:"));
    assert!(catalog.prompt().contains("- hidden:"));
    assert!(!catalog.prompt().contains("Hidden body"));
    fs::write(
        &path,
        "---\nname: review\ndescription: Updated\n---\nUpdated instructions",
    )
    .unwrap();
    assert!(manual.load().unwrap().contains("Updated instructions"));
    fs::remove_file(path).unwrap();
    assert!(manual.load().is_err());
}

/// 共享的用户技能只在数据根取自默认位置时加入。
///
/// 显式指定 `SINGULARITY_HOME` 的数据目录自成一体——即使它指向的正是默认路径——
/// 调用方传入别的 home（评估、测试）时同理：两者都不能把真实用户主目录下的技能
/// 带进提示词，否则同一份配置在不同机器上会看到不同的技能集合。
#[test]
fn shared_user_skills_are_only_added_for_the_default_data_root() {
    let dir = tempfile::tempdir().unwrap();
    // discover 以 cwd 向上的最近 Git 根为项目根；没有 Git 时退回 cwd 自身。
    let project = dir.path().join("project");
    fs::create_dir_all(project.join(".git")).unwrap();
    fs::create_dir_all(project.join(".singularity/skills/mine")).unwrap();
    fs::write(
        project.join(".singularity/skills/mine/SKILL.md"),
        "---\nname: mine\ndescription: 项目技能\n---\n正文",
    )
    .unwrap();
    // 模拟真实用户主目录：默认数据根与它下面的 `.agents/skills` 一起被扫描。
    let os_home = dir.path().join("os-home");
    fs::create_dir_all(os_home.join(".agents/skills/shared")).unwrap();
    fs::write(
        os_home.join(".agents/skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: 共享技能\n---\n正文",
    )
    .unwrap();
    let default_root = os_home.join(crate::SINGULARITY_DIR_NAME);
    let other_root = dir.path().join("other-root");
    let env = |explicit: Option<&std::path::Path>| crate::HomeEnv {
        explicit: explicit.map(|path| path.as_os_str().to_owned()),
        os_home: Some(os_home.as_os_str().to_owned()),
    };
    fn names(catalog: &SkillCatalog) -> Vec<&str> {
        catalog
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }

    // 数据根是默认位置：项目技能与真实用户的共享技能都参与发现。
    let by_default = SkillCatalog::discover_with_env(&project, &default_root, &env(None));
    assert_eq!(names(&by_default), vec!["mine", "shared"]);

    // 调用方传入别的 home：只认项目技能。
    let isolated = SkillCatalog::discover_with_env(&project, &other_root, &env(None));
    assert_eq!(names(&isolated), vec!["mine"]);

    // 显式把 SINGULARITY_HOME 设成默认路径：独立数据目录，不引入真实用户技能。
    let explicit_default =
        SkillCatalog::discover_with_env(&project, &default_root, &env(Some(&default_root)));
    assert_eq!(names(&explicit_default), vec!["mine"]);

    // 显式指向别的目录：同样自成一体。
    let explicit_other =
        SkillCatalog::discover_with_env(&project, &other_root, &env(Some(&other_root)));
    assert_eq!(names(&explicit_other), vec!["mine"]);
}
