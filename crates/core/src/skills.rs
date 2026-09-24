//! 从文件系统发现技能，供 Agent 与工作台共用。

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

const MAX_SKILL_FRONTMATTER_BYTES: usize = 64 * 1024;

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

/// 解析 frontmatter，返回元数据以及正文在 source 中的起始位置。
/// 正文以借用方式返回：发现阶段只需要元数据，不必为正文多复制一份。
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

/// 发现阶段只读 frontmatter；正文和它的 UTF-8 校验留给 Skill::load。
fn discover_skill(path: &Path) -> Result<Skill, String> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    // 损坏文件若没有结束分隔符，也只扫描有限的元数据前缀。
    let mut reader = BufReader::new(file).take((MAX_SKILL_FRONTMATTER_BYTES + 1) as u64);
    let mut source = String::new();
    let mut line = String::new();
    let mut first = true;
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .map_err(|e| format!("{}: {e}", path.display()))?
            == 0
        {
            break;
        }
        source.push_str(&line);
        if source.len() > MAX_SKILL_FRONTMATTER_BYTES {
            return Err(format!(
                "{}: YAML frontmatter exceeds 64 KiB",
                path.display()
            ));
        }
        if !first && line.trim() == "---" {
            break;
        }
        first = false;
    }
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
    /// 加载这个技能的完整指令，并附上资源目录，供指令里的相对路径使用。
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
    /// 优先级从高到低：项目技能、应用主目录、共享的用户技能。
    pub fn discover(cwd: &Path, home: &Path) -> Self {
        let env = crate::HomeEnv::from_process();
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

    /// 只有输入开头的命令词会触发技能；正文里的斜杠仍按普通文本处理。
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
