mod shared;

use assert_cmd::Command;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const HELPER_TARGET_NAME: &str = "cli_test_fake_app_server";

pub struct FakeAppServer {
    binary: PathBuf,
    scenario_path: PathBuf,
}

impl FakeAppServer {
    pub fn new(dir: &Path, scenario: Scenario) -> Self {
        let scenario_path = dir.join("fake-app-server-scenario.json");
        std::fs::write(
            &scenario_path,
            serde_json::to_vec(&scenario.into_value()).expect("serialize fake app-server scenario"),
        )
        .expect("write fake app-server scenario");
        Self {
            binary: helper_binary(),
            scenario_path,
        }
    }

    pub fn binary(&self) -> &Path {
        &self.binary
    }

    pub fn configure(&self, command: &mut Command) {
        command.env(shared::SCENARIO_ENV, &self.scenario_path);
    }

    pub fn configure_process(&self, command: &mut std::process::Command) {
        command.env(shared::SCENARIO_ENV, &self.scenario_path);
    }
    pub fn copy_binary_as(&self, dir: &Path, name: &str) -> PathBuf {
        let target = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(&self.binary, &target).expect("copy fake app-server binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = std::fs::metadata(&target)
                .expect("fake app-server metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&target, permissions)
                .expect("fake app-server binary executable");
        }
        target
    }
}

#[derive(Default)]
pub struct Scenario {
    startup: Vec<Value>,
    methods: Map<String, Value>,
    method_trace: Option<PathBuf>,
}

impl Scenario {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn startup(mut self, actions: Vec<Value>) -> Self {
        self.startup = actions;
        self
    }

    pub fn interaction(mut self, method: &str, actions: Vec<Value>) -> Self {
        self.methods
            .entry(method.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("method interactions array")
            .push(Value::Array(actions));
        self
    }

    pub fn respond(self, method: &str, result: Value) -> Self {
        self.interaction(method, vec![respond(result)])
    }

    pub fn initialized(self) -> Self {
        self.respond("initialize", initialize_result())
    }

    pub fn agent_loop_ready(self) -> Self {
        self.respond(
            "agent/capability",
            agent_loop_capability(true, "completed", "enabled", &[]),
        )
    }

    pub fn shutdown(self) -> Self {
        self.interaction(
            "server/shutdown",
            vec![respond(json!({"shutdown": true})), exit(0)],
        )
    }
    pub fn error(self, method: &str, code: i64, message: &str) -> Self {
        self.interaction(method, vec![respond_error(code, message)])
    }

    pub fn trace_methods_to(mut self, path: &Path) -> Self {
        self.method_trace = Some(path.to_path_buf());
        self
    }

    fn into_value(self) -> Value {
        let mut root = Map::new();
        root.insert("methods".to_owned(), Value::Object(self.methods));
        if !self.startup.is_empty() {
            root.insert("startup".to_owned(), Value::Array(self.startup));
        }
        if let Some(path) = self.method_trace {
            root.insert(
                "method_trace".to_owned(),
                Value::String(path.to_string_lossy().into_owned()),
            );
        }
        Value::Object(root)
    }
}

pub fn respond(result: Value) -> Value {
    json!({"respond": {"result": result}})
}

pub fn respond_error(code: i64, message: &str) -> Value {
    json!({"respond": {"error": {"code": code, "message": message}}})
}

pub fn send(message: Value) -> Value {
    json!({"send": message})
}

pub fn capture_params(path: &Path) -> Value {
    json!({"capture": {"path": path, "value": "params"}})
}

pub fn write_text(path: &Path, text: &str) -> Value {
    json!({"write": {"path": path, "text": text}})
}

pub fn print_stderr(text: &str) -> Value {
    json!({"stderr": text})
}

pub fn exit(code: i32) -> Value {
    json!({"exit": code})
}

pub fn sleep_ms(delay_ms: u64) -> Value {
    json!({"sleep_ms": delay_ms})
}

pub fn initialize_result() -> Value {
    json!({"userAgent": "fake", "platformFamily": "local", "platformOs": "test"})
}

pub fn agent_loop_capability(
    available: bool,
    status: &str,
    reason: &str,
    blockers: &[&str],
) -> Value {
    json!({
        "agentLoop": {
            "available": available,
            "status": status,
            "reason": reason,
            "blockers": blockers,
        },
        "providerReadiness": {
            "source": "project_env",
            "snapshotId": "provider_snapshot_fake_server",
            "ready": true,
            "blocker": null,
            "apiKeyPresent": true,
            "baseUrlPresent": true,
            "modelPresent": true,
        }
    })
}

pub fn thread(thread_id: &str) -> Value {
    json!({"thread_id": thread_id, "model": null, "cwd": null, "status": "active"})
}

pub fn turn(turn_id: &str, thread_id: &str, status: &str, agent_loop_status: &str) -> Value {
    json!({
        "turn_id": turn_id,
        "thread_id": thread_id,
        "status": status,
        "agent_loop_status": agent_loop_status,
    })
}

fn helper_binary() -> PathBuf {
    static HELPER_BINARY: OnceLock<PathBuf> = OnceLock::new();
    HELPER_BINARY.get_or_init(locate_or_build_helper).clone()
}

fn locate_or_build_helper() -> PathBuf {
    if let Some(path) = locate_helper() {
        return path;
    }

    let status = std::process::Command::new("cargo")
        .args([
            "test",
            "-p",
            "singularity_cli",
            "--test",
            HELPER_TARGET_NAME,
            "--no-run",
            "--locked",
        ])
        .current_dir(workspace_root())
        .status()
        .expect("build fake app-server test target");
    assert!(
        status.success(),
        "failed to build fake app-server test target"
    );
    locate_helper().expect("fake app-server test target binary")
}

fn locate_helper() -> Option<PathBuf> {
    let current_exe = std::env::current_exe().ok()?;
    let deps_dir = current_exe.parent()?;
    let prefix = format!("{}-", HELPER_TARGET_NAME);
    let source_modified = helper_source_modified()?;

    std::fs::read_dir(deps_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    let has_executable_suffix = if std::env::consts::EXE_SUFFIX.is_empty() {
                        path.extension().is_none()
                    } else {
                        name.ends_with(std::env::consts::EXE_SUFFIX)
                    };
                    name.starts_with(&prefix) && has_executable_suffix && path != &current_exe
                })
        })
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified = metadata.modified().ok()?;
            (modified >= source_modified).then_some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

fn helper_source_modified() -> Option<std::time::SystemTime> {
    ["fake_app_server.rs", "shared.rs"]
        .into_iter()
        .filter_map(|name| {
            std::fs::metadata(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/support")
                    .join(name),
            )
            .ok()?
            .modified()
            .ok()
        })
        .max()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}
