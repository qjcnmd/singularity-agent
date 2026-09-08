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
