//! 从文件系统发现技能，供 Agent 与工作台共用。

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

/// 一个被发现出来的技能；在被调用之前只保留元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
}

impl Skill {
    /// 用户选择或模型调用 skill 工具时完整读取入口；辅助资源仍由模型按需读取，不受工具输出截断限制。
    pub fn load_instructions(&self) -> Result<String, String> {
        let directory = self.path.parent().ok_or_else(|| {
            format!("无法加载技能 /{}（{}）：缺少父目录", self.name, crate::display_path(&self.path))
        })?;
        let content = fs::read_to_string(&self.path).map_err(|error| {
            format!("无法加载技能 /{}（{}）：{error}", self.name, crate::display_path(&self.path))
        })?;
        Ok(format!(
            "<skill>\nName: {}\nPath: {}\nBase directory: {}\nSkill instructions follow. Already loaded; do not read SKILL.md again to activate it. Resolve relative resources from the base directory using absolute tool paths.\n\n{content}\n</skill>",
            self.name,
            crate::display_path(&self.path),
            crate::display_path(directory),
        ))
    }
}

/// 无效文件如实记进 diagnostics，只在工作台展示，不进入模型提示词。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

/// 发现阶段只读 frontmatter；正文在显式选择时加载，或由模型按需读取。
fn discover_skill(path: &Path, file: fs::File) -> Result<Skill, String> {
    let mut lines = BufReader::new(file).lines();
    let first = lines
        .next()
        .transpose()
        .map_err(|e| format!("{}: {e}", crate::display_path(path)))?
        .unwrap_or_default();
    if first.trim_start_matches('\u{feff}').trim() != "---" {
        return Err(format!("{}: missing YAML frontmatter", crate::display_path(path)));
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        let line = line.map_err(|e| format!("{}: {e}", crate::display_path(path)))?;
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(&line);
        yaml.push('\n');
    }
    if !closed {
        return Err(format!("{}: unclosed YAML frontmatter", crate::display_path(path)));
    }
    let meta: Metadata =
        serde_saphyr::from_str(&yaml).map_err(|e| format!("{}: {e}", crate::display_path(path)))?;
    if meta.name.is_empty()
        || meta.name.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '\\' | '<' | '>'))
        || meta.description.trim().is_empty()
    {
        return Err(format!(
            "{}: name must be a single command name and description must be nonempty",
            crate::display_path(path)
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
        // 与项目指令共用同一套根目录规则。目录规范化或标记读不出来时不阻断技能发现：
        // 退回可用目录，并像其他扫描失败一样把原因记进 diagnostics。
        let mut extra_diagnostics = Vec::new();
        let canonical = match crate::canonicalize_workspace(cwd) {
            Ok(canonical) => Some(canonical),
            Err(error) => {
                extra_diagnostics.push(error);
                None
            }
        };
        let effective_cwd = canonical.as_ref().map_or(cwd, crate::CanonicalWorkspacePath::as_path);
        let root = match crate::workspace::project_root(effective_cwd) {
            Ok(root) => root,
            Err(error) => {
                extra_diagnostics.push(error);
                effective_cwd.to_path_buf()
            }
        };
        let mut roots = vec![
            root.join(crate::identity::DATA_DIR_NAME).join("skills"),
            root.join(".agents").join("skills"),
            home.join("skills"),
        ];
        if let Some(os_home) = std::env::home_dir() {
            roots.push(os_home.join(".agents").join("skills"));
        }
        let mut seen = std::collections::HashSet::new();
        roots.retain(|root| {
            let key = std::fs::canonicalize(root)
                .map_or_else(|_| crate::workspace_key(root), |path| crate::workspace_key(&path));
            seen.insert(key)
        });
        let mut catalog = Self::from_roots(&roots);
        catalog.diagnostics.extend(extra_diagnostics);
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
                    diagnostics.push(format!("{}: {e}", crate::display_path(root)));
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
                            Err(e) => diagnostics.push(format!("{}: {e}", crate::display_path(&path))),
                        }
                    }
                    Err(e) => diagnostics.push(format!("{}: {e}", crate::display_path(root))),
                }
            }
            paths.sort();
            for path in paths {
                let file = match fs::File::open(&path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        diagnostics.push(format!("{}: {error}", crate::display_path(&path)));
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

    /// 列出名称和摘要；完整指令由模型通过 skill 工具按名称加载。没有可公布的技能时为空。
    pub fn prompt(&self) -> String {
        let mut lines: Vec<_> =
            self.model_skills().map(|s| format!("- {}: {}", s.name, s.description)).collect();
        if lines.is_empty() {
            return String::new();
        }
        lines.insert(0, "Available skills: when a skill matches the user's task, call the skill tool with its name to load the complete instructions.".into());
        lines.join("\n")
    }
}
