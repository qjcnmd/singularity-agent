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

/// 无效文件如实记进 diagnostics，只在工作台展示，不进入模型提示词。
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
fn discover_skill(path: &Path, file: fs::File) -> Result<Skill, String> {
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
    let meta: Metadata = serde_saphyr::from_str(&yaml).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.name.is_empty()
        || meta.name.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '<' | '>'))
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
            root.join(crate::user_home::SINGULARITY_DIR_NAME).join("skills"),
            root.join(".agents/skills"),
            home.join("skills"),
        ];
        if let Some(os_home) = std::env::home_dir() {
            roots.push(os_home.join(".agents/skills"));
        }
        let mut catalog = Self::from_roots(&roots);
        if let Some(error) = root_error {
            catalog.diagnostics.push(error);
        }
        catalog
    }

    /// 按 roots 的先后顺序，发现子目录中的 SKILL.md。
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
                        // 子路径元数据访问失败不等于「不存在」或「不是目录」，真实原因
                        // 写进诊断。metadata 跟随符号链接，发现范围与 is_dir() 一致。
                        match path.metadata() {
                            Ok(metadata) if metadata.is_dir() => {
                                paths.push(path.join("SKILL.md"));
                            }
                            Ok(_) => {}
                            Err(e) => diagnostics.push(format!("{}: {e}", path.display())),
                        }
                    }
                    Err(e) => diagnostics.push(format!("{}: {e}", root.display())),
                }
            }
            paths.sort();
            for path in paths {
                let file = match fs::File::open(&path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        diagnostics.push(format!("{}: {error}", path.display()));
                        continue;
                    }
                };
                match discover_skill(&path, file) {
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

    /// 实际向模型公布的技能；请求目录和运行状态使用同一个筛选规则。
    pub fn model_skills(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter().filter(|skill| !skill.disable_model_invocation)
    }

    /// 列出名称、摘要和文件路径；完整指令由模型通过 read 工具加载。
    pub fn prompt(&self) -> String {
        let mut lines: Vec<_> = self
            .model_skills()
            .map(|s| format!("- {}: {} (path: {})", s.name, s.description, s.path.display()))
            .collect();
        if lines.is_empty() {
            return String::new();
        }
        lines.insert(0, "Available skills: when a skill matches the user's task, use the read tool on its path to load the complete instructions. Relative resources in a skill are resolved from that skill file's directory.".into());
        lines.join("\n")
    }
}
