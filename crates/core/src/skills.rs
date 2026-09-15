//! Filesystem skill discovery shared by the Agent and the workbench.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;

/// A discoverable skill; only metadata is retained until invocation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub user_invocable: bool,
    pub disable_model_invocation: bool,
}

/// Invalid files are reported without hiding other usable skills.
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
    /// Load current instructions and include their resource directory for relative paths.
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
    /// Project skills take precedence, then the application's user home, then shared user skills.
    pub fn discover(cwd: &Path, home: &Path) -> Self {
        let root = cwd
            .ancestors()
            .find(|p| p.join(crate::PROJECT_ROOT_MARKER).exists())
            .unwrap_or(cwd);
        let mut roots = vec![
            root.join(".singularity/skills"),
            root.join(".agents/skills"),
            home.join("skills"),
        ];
        // An explicit application home is self-contained; do not leak the real user's skills into it.
        if crate::user_singularity_home().as_deref() == Some(home)
            && std::env::var_os("SINGULARITY_HOME").is_none()
            && let Some((base, _)) = crate::user_home_base_from_env()
        {
            roots.push(base.join(".agents/skills"));
        }
        Self::from_roots(&roots)
    }

    /// Discover flat Markdown files or one-level bundles in precedence order.
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
                        if path.is_dir() {
                            let skill = path.join("SKILL.md");
                            if skill.exists() {
                                paths.push(skill);
                            }
                        } else if path
                            .extension()
                            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
                        {
                            paths.push(path);
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

    /// Only a leading command invokes a skill; inline slashes remain ordinary text.
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

    /// Summary-only catalog: full instructions are loaded through the skill tool.
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
