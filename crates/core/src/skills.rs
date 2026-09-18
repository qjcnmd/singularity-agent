//! Agent 与工作台共用的文件系统技能发现。

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;

/// 一个可发现的技能；调用前只保留元数据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
}

/// 无效文件如实上报，不因此隐藏其他可用技能。
#[derive(Debug, Default, Serialize)]
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

/// 解析 frontmatter，返回元数据与正文在 source 中的起点。
/// 正文只借用调用方的文本：发现阶段只需要元数据，不物化无用的正文副本。
fn parse_frontmatter<'a>(path: &Path, source: &'a str) -> Result<(Metadata, &'a str), String> {
    let source = source.trim_start_matches('\u{feff}');
    let mut lines = source.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return Err(format!("{}: missing YAML frontmatter", path.display()));
    };
    if first.trim() != "---" {
        return Err(format!("{}: missing YAML frontmatter", path.display()));
    }
    let mut yaml = String::new();
    let mut consumed = first.len();
    let mut body_start = None;
    for line in lines {
        consumed += line.len();
        if line.trim() == "---" {
            body_start = Some(consumed);
            break;
        }
        yaml.push_str(line);
    }
    let Some(body_start) = body_start else {
        return Err(format!("{}: unclosed YAML frontmatter", path.display()));
    };
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
    Ok((meta, &source[body_start..]))
}

/// 发现阶段只读元数据；正文由 Skill::load 在实际组装指令时读取。
fn discover_skill(path: &Path) -> Result<Skill, String> {
    let source = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (meta, _) = parse_frontmatter(path, &source)?;
    Ok(Skill {
        name: meta.name,
        description: meta.description,
        path: path.to_path_buf(),
        user_invocable: meta.user_invocable,
        disable_model_invocation: meta.disable_model_invocation,
    })
}

impl Skill {
    /// 加载当前指令，并带上资源目录供相对路径使用。
    pub fn load(&self) -> Result<String, String> {
        let source =
            fs::read_to_string(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let (meta, body) = parse_frontmatter(&self.path, &source)?;
        if meta.name != self.name {
            return Err(format!(
                "{}: skill name changed; refresh the catalog",
                self.path.display()
            ));
        }
        Ok(format!(
            "<skill name={:?}>\nSource: {}\nResource directory: {}\nFollow these skill instructions for the requested task, subject to higher-priority instructions.\n\n{}\n</skill>",
            self.name,
            self.path.display(),
            self.path.parent().unwrap_or(Path::new(".")).display(),
            body.trim()
        ))
    }
}

impl SkillCatalog {
    /// 项目技能优先，其次是应用主目录，最后是共享的用户技能。
    pub fn discover(cwd: &Path, home: &Path) -> Self {
        Self::discover_with_env(cwd, home, &crate::HomeEnv::from_process())
    }

    /// 可注入解析输入的发现：共享技能范围取决于数据根是否取自默认位置。
    fn discover_with_env(cwd: &Path, home: &Path, env: &crate::HomeEnv) -> Self {
        // 与项目指令共用同一根目录规则。标记读不到时不阻断技能发现：退回 cwd，
        // 并像其他扫描失败一样把原因留在 diagnostics 里。
        let (root, root_error) = match crate::workspace::project_root(cwd) {
            Ok(root) => (root, None),
            Err(error) => (cwd.to_path_buf(), Some(error)),
        };
        let mut roots = vec![
            root.join(".singularity/skills"),
            root.join(".agents/skills"),
            home.join("skills"),
        ];
        // 只有取自默认位置的数据根才与真实用户主目录共享技能：显式指定
        // SINGULARITY_HOME 的数据目录自成一体，即使它的路径恰好就是默认位置；
        // 调用方传入别的 home（评估、测试）时同样不引入真实用户的技能。
        if let Ok(resolved) = env.resolve()
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

    /// 按优先级发现平铺的 Markdown 文件或一层 bundle。
    pub fn from_roots(roots: &[PathBuf]) -> Self {
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
                        // 子路径的元数据访问失败不能当成「不存在／不是目录」：
                        // 具体检查点保留真实原因并写入既有诊断。metadata 跟随
                        // 符号链接，与原来的 is_dir() 发现范围一致。
                        match path.metadata() {
                            Ok(metadata) if metadata.is_dir() => {
                                let skill = path.join("SKILL.md");
                                // 可选 SKILL.md 真正不存在时安静跳过，其余
                                // I/O 失败进入诊断。
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

    /// 只有开头的命令词会触发技能；正文中的斜杠仍是普通文本。
    pub fn manual(&self, input: &str) -> Option<&Skill> {
        let name = input
            .trim_start()
            .strip_prefix('/')?
            .split_whitespace()
            .next()?;
        self.skills
            .iter()
            .find(|skill| skill.user_invocable && skill.name == name)
    }

    /// 只含摘要的目录：完整指令经 skill 工具加载。
    pub fn prompt(&self) -> String {
        let mut lines: Vec<_> = self
            .skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect();
        if !lines.is_empty() {
            lines.insert(0, "Available skills: use the skill tool to load the complete instructions when a skill matches the user's task. Relative resources are resolved from the loaded skill's directory.".into());
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
