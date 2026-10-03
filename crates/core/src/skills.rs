//! 从文件系统发现技能，供 Agent 与工作台共用。

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

/// 一个被发现出来的技能；在被调用之前只保留元数据。
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
}

/// 无效文件如实记进 diagnostics，不因此隐藏其他可用技能。
#[derive(Debug, Default)]
pub struct SkillCatalog {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Metadata {
    name: String,
    description: String,
    #[serde(default = "enabled")]
    user_invocable: bool,
    #[serde(default)]
    disable_model_invocation: bool,
}
fn enabled() -> bool {
    true
}

/// 发现阶段只读 frontmatter；正文由模型通过 read 读取。
fn discover_skill(path: &Path) -> Result<Skill, String> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let first = lines
        .next()
        .transpose()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .unwrap_or_default();
    if first.trim_start_matches('\u{feff}').trim() != "---" {
        return Err(format!("{}: missing YAML frontmatter", path.display()));
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        if line.trim() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(&line);
        yaml.push('\n');
    }
    if !closed {
        return Err(format!("{}: unclosed YAML frontmatter", path.display()));
    }
    let meta: Metadata =
        serde_yaml_ng::from_str(&yaml).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.name.is_empty()
        || meta
            .name
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '<' | '>'))
        || meta.description.trim().is_empty()
    {
        return Err(format!(
            "{}: name must be a single command name and description must be nonempty",
            path.display()
        ));
    }
    Ok(Skill {
        name: meta.name,
        description: meta.description,
        path: path.to_path_buf(),
        user_invocable: meta.user_invocable,
        disable_model_invocation: meta.disable_model_invocation,
    })
}

impl SkillCatalog {
    /// 优先级从高到低：项目技能、应用主目录、共享的用户技能。
    pub fn discover(cwd: &Path, home: &Path) -> Self {
        // 与项目指令共用同一套根目录规则。标记读不出来时不阻断技能发现：退回
        // cwd，并像其他扫描失败一样把原因记进 diagnostics。
        let (root, root_error) = match crate::workspace::project_root(cwd) {
            Ok(root) => (root, None),
            Err(error) => (cwd.to_path_buf(), Some(error)),
        };
        let mut roots = vec![
            root.join(crate::user_home::SINGULARITY_DIR_NAME)
                .join("skills"),
            root.join(".agents/skills"),
            home.join("skills"),
        ];
        // 只有取自默认位置的数据根才和真实用户主目录共享技能：显式指定 SINGULARITY_HOME 的数据
        // 目录自成一体，哪怕它的路径正好就是默认位置；调用方传入别的 home（评估、测试）时同样不引入。
        if let Ok(resolved) = crate::resolve_home()
            && resolved.path == home
            && let crate::HomeOrigin::Default(os_home) = resolved.origin
        {
            roots.push(os_home.join(".agents/skills"));
        }
        let mut catalog = Self::from_roots(&roots);
        if let Some(error) = root_error {
            catalog.diagnostics.push(error);
        }
        catalog
    }

    /// 按 roots 的先后顺序，发现平铺的 Markdown 文件或一层 bundle 目录。
    fn from_roots(roots: &[PathBuf]) -> Self {
        let mut found = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for root in roots {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    diagnostics.push(format!("{}: {e}", root.display()));
                    continue;
                }
            };
            let mut paths = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => {
                        let path = entry.path();
                        // 访问子路径元数据失败时，不能当成「不存在」或「不是目录」：每个检查点都
                        // 保留真实原因并写进诊断。metadata 跟随符号链接，发现范围与 is_dir() 一致。
                        match path.metadata() {
                            Ok(metadata) if metadata.is_dir() => {
                                let skill = path.join("SKILL.md");
                                // SKILL.md 是可选的：确实不存在就安静跳过，其他 I/O 失败写进诊断。
                                match skill.metadata() {
                                    Ok(_) => paths.push(skill),
                                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                                    Err(e) => diagnostics.push(format!("{}: {e}", skill.display())),
                                }
                            }
                            Ok(_) => {
                                if path
                                    .extension()
                                    .is_some_and(|e| e.eq_ignore_ascii_case("md"))
                                {
                                    paths.push(path);
                                }
                            }
                            Err(e) => diagnostics.push(format!("{}: {e}", path.display())),
                        }
                    }
                    Err(e) => diagnostics.push(format!("{}: {e}", root.display())),
                }
            }
            paths.sort();
            for path in paths {
                match discover_skill(&path) {
                    Ok(skill) => {
                        found.entry(skill.name.clone()).or_insert(skill);
                    }
                    Err(error) => diagnostics.push(error),
                }
            }
        }
        Self {
            skills: found.into_values().collect(),
            diagnostics,
        }
    }

    /// 列出名称、摘要和文件路径；完整指令由模型通过 read 工具加载。
    pub fn prompt(&self) -> String {
        let mut lines: Vec<_> = self
            .skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
            .map(|s| {
                format!(
                    "- {}: {} (path: {})",
                    s.name,
                    s.description,
                    s.path.display()
                )
            })
            .collect();
        if !lines.is_empty() {
            lines.insert(0, "Available skills: when a skill matches the user's task, use the read tool on its path to load the complete instructions. Relative resources in a skill are resolved from that skill file's directory.".into());
        }
        if !self.diagnostics.is_empty() {
            lines.push(format!(
                "Skill discovery errors (these skills are unavailable):\n{}",
                self.diagnostics.join("\n")
            ));
        }
        lines.join("\n")
    }
}
