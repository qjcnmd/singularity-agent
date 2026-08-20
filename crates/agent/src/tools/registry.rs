//! 工具注册表：名称 → ToolSpec 的单一事实源，以及参数校验。

use std::collections::{BTreeMap, HashMap};
use std::fmt::{Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use serde_json::Value;
use singularity_core::CancellationToken;

use super::bash;
use super::edit;
use super::read;
use super::write;

/// 一次工具执行的模型可见结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecution {
    pub content: String,
    pub is_error: bool,
}

/// Per-registry serialization for complete file mutation windows.
///
/// Entries are keyed by canonical execution environment and canonical target
/// path. Idle entries are removed on the last lease drop, so the map does not
/// grow with every path ever touched.
#[derive(Debug, Default)]
pub struct FileMutationQueue {
    entries: Mutex<HashMap<MutationKey, MutationEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MutationKey {
    environment: PathBuf,
    path: PathBuf,
}

#[derive(Debug)]
struct MutationEntry {
    gate: Arc<MutationGate>,
    users: usize,
}

#[derive(Debug, Default)]
struct MutationGate {
    held: Mutex<bool>,
    available: Condvar,
}

impl MutationGate {
    fn acquire(&self) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *held {
            held = self
                .available
                .wait(held)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        *held = true;
    }

    fn release(&self) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *held = false;
        self.available.notify_one();
    }
}

pub(crate) struct MutationLease<'a> {
    queue: &'a FileMutationQueue,
    key: MutationKey,
    gate: Arc<MutationGate>,
}

impl Drop for MutationLease<'_> {
    fn drop(&mut self) {
        self.gate.release();
        let mut entries = self
            .queue
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let remove = entries.get_mut(&self.key).is_some_and(|entry| {
            entry.users = entry.users.saturating_sub(1);
            entry.users == 0 && Arc::ptr_eq(&entry.gate, &self.gate)
        });
        if remove {
            entries.remove(&self.key);
        }
    }
}

impl FileMutationQueue {
    pub(crate) fn lock<'a>(&'a self, cwd: &Path, path: &str) -> Result<MutationLease<'a>, String> {
        let key = mutation_key(cwd, path)?;
        let gate = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = entries.entry(key.clone()).or_insert_with(|| MutationEntry {
                gate: Arc::new(MutationGate::default()),
                users: 0,
            });
            entry.users = entry.users.saturating_add(1);
            Arc::clone(&entry.gate)
        };
        gate.acquire();
        Ok(MutationLease {
            queue: self,
            key,
            gate,
        })
    }

    #[cfg(test)]
    fn entry_count(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

fn mutation_key(cwd: &Path, path: &str) -> Result<MutationKey, String> {
    let environment = std::fs::canonicalize(cwd)
        .map_err(|error| format!("failed to canonicalize execution environment: {error}"))?;
    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        environment.join(path)
    };
    let normalized = canonicalize_for_mutation(&candidate)?;
    Ok(MutationKey {
        environment: normalize_mutation_key(&environment),
        path: normalize_mutation_key(&normalized),
    })
}

/// Windows resolves ordinary paths case-insensitively, including missing path
/// components that have not yet been created.  Fold only the in-memory queue
/// key; callers still receive and write the original path spelling.
#[cfg(windows)]
fn normalize_mutation_key(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(windows))]
fn normalize_mutation_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

fn canonicalize_for_mutation(candidate: &Path) -> Result<PathBuf, String> {
    let mut existing = candidate.to_path_buf();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| format!("cannot normalize mutation path {}", candidate.display()))?;
        missing.push(name.to_os_string());
        if !existing.pop() {
            return Err(format!(
                "cannot normalize mutation path {}",
                candidate.display()
            ));
        }
    }
    let mut normalized = std::fs::canonicalize(&existing)
        .map_err(|error| format!("failed to canonicalize mutation path: {error}"))?;
    for component in missing.iter().rev() {
        match Path::new(component).components().next() {
            Some(Component::Normal(name)) => normalized.push(name),
            Some(Component::ParentDir) => {
                normalized.pop();
            }
            Some(Component::CurDir) => {}
            _ => {
                return Err(format!(
                    "cannot normalize mutation path {}",
                    candidate.display()
                ));
            }
        }
    }
    Ok(normalized)
}

/// Result of the lookup and argument-validation preflight performed before a
/// tool batch starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedTool {
    pub(crate) name: &'static str,
}

/// Preflight either produces an executable tool or a model-visible rejection.
/// Unknown names remain registry errors so callers can preserve that boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPreflight {
    Ready(PreparedTool),
    Rejected(ToolExecution),
}

/// 注册表/内部层错误（如未知工具名）。工具自身的失败一律走 `ToolExecution::is_error`，
/// 不进入该错误通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl Display for ToolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

/// 工具执行上下文：参数、会话工作区（构造时绑定）、中断信号、流式输出回调。
pub struct ExecuteContext<'a> {
    pub args: Value,
    pub cwd: &'a Path,
    pub signal: Option<&'a CancellationToken>,
    pub on_update: Option<&'a mut dyn FnMut(&str)>,
    pub mutation_queue: Option<Arc<FileMutationQueue>>,
}

/// 工具规格：模型可见的名称/描述/JSON Schema（parameters），以及真实执行函数。
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    pub execute: for<'a> fn(ExecuteContext<'a>) -> Result<ToolExecution, ToolError>,
}

/// 名称 → 工具规格的注册表；`new()` 注册默认工具集（read/bash/edit/write）。
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, ToolSpec>,
    mutation_queue: Arc<FileMutationQueue>,
}

impl ToolRegistry {
    /// 创建注册表并注册默认工具（read/bash/edit/write）。
    pub fn new() -> Self {
        let mut registry = Self::default();
        registry.insert_fixed(read::spec());
        registry.insert_fixed(bash::spec());
        registry.insert_fixed(edit::spec());
        registry.insert_fixed(write::spec());
        registry
    }

    fn insert_fixed(&mut self, spec: ToolSpec) {
        self.tools.insert(spec.name, spec);
    }

    /// 注册工具。重复名称是编程错误，直接 panic（调用方必须使用唯一名称）。
    #[cfg(test)]
    pub fn register(&mut self, spec: ToolSpec) {
        if self.tools.contains_key(spec.name) {
            panic!("tool already registered: {}", spec.name);
        }
        self.tools.insert(spec.name, spec);
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }

    /// 已注册工具名（确定性排序）。
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
    }

    /// 执行工具：先按 `parameters` JSON Schema 校验参数，校验失败返回 is_error；
    /// 未知工具名返回 `Err(ToolError)`。
    pub fn execute<'a>(
        &self,
        name: &str,
        ctx: ExecuteContext<'a>,
    ) -> Result<ToolExecution, ToolError> {
        match self.preflight(name, &ctx.args)? {
            ToolPreflight::Ready(prepared) => self.execute_prepared(prepared, ctx),
            ToolPreflight::Rejected(execution) => Ok(execution),
        }
    }

    /// Lookup and validate a call without executing it. Agent batches use this
    /// before deciding whether the whole response can run in parallel.
    pub fn preflight(&self, name: &str, args: &Value) -> Result<ToolPreflight, ToolError> {
        let spec = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError(format!("unknown tool: {name}")))?;
        if let Err(message) = validate_arguments(&spec.parameters, args) {
            return Ok(ToolPreflight::Rejected(ToolExecution {
                content: format!("tool arguments failed validation: {message}"),
                is_error: true,
            }));
        }
        Ok(ToolPreflight::Ready(PreparedTool { name: spec.name }))
    }

    /// Execute a call that has already passed [`Self::preflight`].
    pub fn execute_prepared<'a>(
        &self,
        prepared: PreparedTool,
        mut ctx: ExecuteContext<'a>,
    ) -> Result<ToolExecution, ToolError> {
        if ctx.mutation_queue.is_none() {
            ctx.mutation_queue = Some(Arc::clone(&self.mutation_queue));
        }
        let spec = self
            .tools
            .get(prepared.name)
            .ok_or_else(|| ToolError(format!("unknown tool: {}", prepared.name)))?;
        (spec.execute)(ctx)
    }
}

/// 工具失败结果（is_error=true）的构造捷径。
pub(crate) fn error_result(message: impl Into<String>) -> Result<ToolExecution, ToolError> {
    Ok(ToolExecution {
        content: message.into(),
        is_error: true,
    })
}

/// 相对路径解析绑定到当前工作区目录，绝对路径保持原样。
pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

/// 依据工具定义的 JSON Schema（properties / required / type）进行参数合法性校验。
/// 缺少必填参数、参数类型不匹配或出现未知未声明参数均作为校验错误拦截。
fn validate_arguments(parameters: &Value, args: &Value) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err("tool arguments must be a JSON object".to_string());
    };
    let properties = parameters.get("properties").and_then(Value::as_object);
    if let Some(required) = parameters.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("missing required parameter \"{name}\""));
            }
        }
    }
    for (name, value) in object {
        let Some(schema) = properties.and_then(|properties| properties.get(name)) else {
            return Err(format!("unknown parameter \"{name}\""));
        };
        let expected = schema.get("type").and_then(Value::as_str).unwrap_or("any");
        if !json_value_matches_type(value, expected) {
            return Err(format!("parameter \"{name}\" must be of type {expected}"));
        }
    }
    Ok(())
}

fn json_value_matches_type(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::context;
    use serde_json::json;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    fn ping_execute(_ctx: ExecuteContext<'_>) -> Result<ToolExecution, ToolError> {
        Ok(ToolExecution {
            content: "pong".to_string(),
            is_error: false,
        })
    }

    fn ping_spec() -> ToolSpec {
        ToolSpec {
            name: "ping",
            description: "custom test tool",
            parameters: json!({ "type": "object", "properties": {}, "required": [] }),
            execute: ping_execute,
        }
    }

    #[test]
    fn default_tools_include_read_bash_edit_write() {
        let registry = ToolRegistry::new();
        for expected in ["read", "bash", "edit", "write"] {
            assert!(registry.names().contains(&expected), "missing {expected}");
            assert!(
                registry.get(expected).is_some(),
                "missing spec for {expected}"
            );
        }
    }

    #[test]
    fn register_and_query_custom_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(ping_spec());
        assert!(registry.get("ping").is_some());
        assert!(registry.names().contains(&"ping"));
        let result = registry
            .execute("ping", context(json!({}), Path::new(".")))
            .expect("execute");
        assert_eq!(result.content, "pong");
        assert!(!result.is_error);
    }

    #[test]
    #[should_panic(expected = "tool already registered")]
    fn duplicate_registration_is_rejected() {
        let mut registry = ToolRegistry::new();
        registry.register(ping_spec());
        registry.register(ping_spec());
    }

    #[test]
    fn unknown_tool_is_err() {
        let registry = ToolRegistry::new();
        assert!(
            registry
                .execute("nope", context(json!({}), Path::new(".")))
                .is_err()
        );
    }

    #[test]
    fn missing_required_parameter_is_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("bash", context(json!({}), Path::new(".")))
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("missing required parameter \"command\"")
        );
    }

    #[test]
    fn wrong_parameter_type_is_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("bash", context(json!({ "command": 42 }), Path::new(".")))
            .expect("execute");
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("parameter \"command\" must be of type string")
        );
    }

    #[test]
    fn unknown_parameter_is_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute(
                "read",
                context(json!({ "path": "a.txt", "bogus": 1 }), Path::new(".")),
            )
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("unknown parameter \"bogus\""));
    }

    #[test]
    fn non_object_arguments_are_error() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("bash", context(json!("oops"), Path::new(".")))
            .expect("execute");
        assert!(result.is_error);
        assert!(result.content.contains("must be a JSON object"));
    }

    #[test]
    fn mutation_queue_serializes_same_path_and_cleans_idle_entry() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::new();
        let lease = registry
            .mutation_queue
            .lock(dir.path(), "same.txt")
            .expect("first mutation lease");
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let result = registry.execute(
                    "write",
                    context(
                        json!({ "path": "same.txt", "content": "value" }),
                        dir.path(),
                    ),
                );
                sender.send(result).unwrap();
            });
            assert!(
                receiver.recv_timeout(Duration::from_millis(50)).is_err(),
                "same-path mutation must wait for the held lease"
            );
            drop(lease);
            let result = receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("write after lease")
                .expect("write result");
            assert!(!result.is_error);
            handle.join().unwrap();
        });
        assert_eq!(registry.mutation_queue.entry_count(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("same.txt")).unwrap(),
            "value"
        );
    }

    #[test]
    fn mutation_queue_allows_different_paths_to_run_in_parallel() {
        let dir = tempdir().unwrap();
        let registry = ToolRegistry::new();
        let lease_a = registry
            .mutation_queue
            .lock(dir.path(), "a.txt")
            .expect("held path lease");
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            let handle_a = scope.spawn(|| {
                let result = registry.execute(
                    "write",
                    context(json!({ "path": "a.txt", "content": "a" }), dir.path()),
                );
                sender.send(("a", result)).unwrap();
            });
            let handle_b = scope.spawn(|| {
                let result = registry.execute(
                    "write",
                    context(json!({ "path": "b.txt", "content": "b" }), dir.path()),
                );
                sender.send(("b", result)).unwrap();
            });
            let (label, result) = receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("different path result");
            assert_eq!(label, "b");
            assert!(!result.expect("different path execution").is_error);
            drop(lease_a);
            let (label, result) = receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("held path result");
            assert_eq!(label, "a");
            assert!(!result.expect("held path execution").is_error);
            handle_a.join().unwrap();
            handle_b.join().unwrap();
        });
        assert_eq!(registry.mutation_queue.entry_count(), 0);
    }

    #[test]
    fn mutation_queue_canonicalizes_parent_aliases() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let registry = ToolRegistry::new();
        let lease = registry
            .mutation_queue
            .lock(dir.path(), "same.txt")
            .expect("canonical path lease");
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let result = registry
                    .mutation_queue
                    .lock(dir.path(), "nested/../same.txt");
                sender.send(result.map(|_| ())).unwrap();
            });
            assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
            drop(lease);
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("alias lease")
                .expect("alias path lock");
        });
        assert_eq!(registry.mutation_queue.entry_count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn mutation_queue_serializes_windows_case_aliases() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Same.txt"), "original").unwrap();
        let registry = ToolRegistry::new();
        let lease = registry
            .mutation_queue
            .lock(dir.path(), "Same.txt")
            .expect("original path lease");
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let result = registry.mutation_queue.lock(dir.path(), "same.TXT");
                sender.send(result.map(|_| ())).unwrap();
            });
            assert!(
                receiver.recv_timeout(Duration::from_millis(50)).is_err(),
                "Windows path aliases must share the mutation lease"
            );
            drop(lease);
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("case alias lease")
                .expect("case alias path lock");
        });
        assert_eq!(registry.mutation_queue.entry_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn mutation_queue_serializes_symlink_aliases() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("target.txt"), "original").unwrap();
        std::os::unix::fs::symlink("target.txt", dir.path().join("alias.txt"))
            .expect("file symlink");

        let registry = ToolRegistry::new();
        let lease = registry
            .mutation_queue
            .lock(dir.path(), "target.txt")
            .expect("target path lease");
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let result = registry.mutation_queue.lock(dir.path(), "alias.txt");
                sender.send(result.map(|_| ())).unwrap();
            });
            assert!(
                receiver.recv_timeout(Duration::from_millis(50)).is_err(),
                "symlink alias must wait for the target path lease"
            );
            drop(lease);
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("symlink alias lease")
                .expect("symlink alias path lock");
        });
        assert_eq!(registry.mutation_queue.entry_count(), 0);
    }

    #[test]
    fn edit_holds_mutation_lease_across_read_and_write() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("edit.txt"), "old").unwrap();
        let registry = ToolRegistry::new();
        let lease = registry
            .mutation_queue
            .lock(dir.path(), "edit.txt")
            .expect("held edit lease");
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let result = registry.execute(
                    "edit",
                    context(
                        json!({ "path": "edit.txt", "oldString": "old", "newString": "new" }),
                        dir.path(),
                    ),
                );
                sender.send(result).unwrap();
            });
            assert!(receiver.recv_timeout(Duration::from_millis(50)).is_err());
            drop(lease);
            assert!(
                !receiver
                    .recv_timeout(Duration::from_secs(1))
                    .expect("edit result")
                    .expect("edit execution")
                    .is_error
            );
        });
        assert_eq!(
            std::fs::read_to_string(dir.path().join("edit.txt")).unwrap(),
            "new"
        );
        assert_eq!(registry.mutation_queue.entry_count(), 0);
    }

    #[test]
    fn mutation_queue_releases_after_write_or_edit_failure() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("directory")).unwrap();
        let registry = ToolRegistry::new();
        let write = registry
            .execute(
                "write",
                context(json!({ "path": "directory", "content": "x" }), dir.path()),
            )
            .unwrap();
        assert!(write.is_error);
        assert_eq!(registry.mutation_queue.entry_count(), 0);
        let edit = registry
            .execute(
                "edit",
                context(
                    json!({ "path": "missing.txt", "oldString": "a", "newString": "b" }),
                    dir.path(),
                ),
            )
            .unwrap();
        assert!(edit.is_error);
        assert_eq!(registry.mutation_queue.entry_count(), 0);
    }
}
