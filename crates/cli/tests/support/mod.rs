//! CLI 集成测试共用的 fake app-server 夹具和协议构造器。

mod shared;

use assert_cmd::Command;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const HELPER_TARGET_NAME: &str = "cli_test_fake_app_server";

/// 可配置并可复制的 fake app-server 测试实例。
pub struct FakeAppServer {
    binary: PathBuf,
    scenario_path: PathBuf,
}

// FakeAppServer 的 scenario 持久化、环境配置和二进制复制操作。
impl FakeAppServer {
    /// 将 scenario 持久化到临时目录并准备 fake app-server 二进制。
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

    /// 返回 fake app-server 二进制路径。
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// 为 assert_cmd 命令注入 fake app-server 的 scenario 路径。
    pub fn configure(&self, command: &mut Command) {
        command.env(shared::SCENARIO_ENV, &self.scenario_path);
    }

    /// 为标准进程命令注入 fake app-server 的 scenario 路径。
    pub fn configure_process(&self, command: &mut std::process::Command) {
        command.env(shared::SCENARIO_ENV, &self.scenario_path);
    }
    /// 将 fake app-server 复制为指定名称，测试相邻二进制解析。
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
/// fake app-server 的启动动作、method 交互和 trace 配置。
pub struct Scenario {
    startup: Vec<Value>,
    methods: Map<String, Value>,
    method_trace: Option<PathBuf>,
}

// Scenario builder 的交互组合与 JSON 投影。
impl Scenario {
    /// 创建空 scenario。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置进程启动后立即执行的动作。
    pub fn startup(mut self, actions: Vec<Value>) -> Self {
        self.startup = actions;
        self
    }

    /// 为一个 method 追加按调用顺序使用的 action 列表。
    pub fn interaction(mut self, method: &str, actions: Vec<Value>) -> Self {
        self.methods
            .entry(method.to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("method interactions array")
            .push(Value::Array(actions));
        self
    }

    /// 为一个 method 配置单次 result 响应。
    pub fn respond(self, method: &str, result: Value) -> Self {
        self.interaction(method, vec![respond(result)])
    }

    /// 添加标准 initialize 响应。
    pub fn initialized(self) -> Self {
        self.respond("initialize", initialize_result())
    }

    /// 添加已完成且无 blocker 的 AgentLoop capability 响应。
    pub fn agent_loop_ready(self) -> Self {
        self.respond(
            "agent/capability",
            agent_loop_capability(true, "completed", "enabled", &[]),
        )
    }

    /// 添加 server/shutdown 响应并使 fake 进程退出。
    pub fn shutdown(self) -> Self {
        self.interaction(
            "server/shutdown",
            vec![respond(json!({"shutdown": true})), exit(0)],
        )
    }
    /// 添加指定 JSON-RPC 错误响应。
    pub fn error(self, method: &str, code: i64, message: &str) -> Self {
        self.interaction(method, vec![respond_error(code, message)])
    }

    /// 将收到的 method 名称追加写入指定 trace 文件。
    pub fn trace_methods_to(mut self, path: &Path) -> Self {
        self.method_trace = Some(path.to_path_buf());
        self
    }

    // 将 builder 状态转换为 fake app-server 消费的 JSON scenario。
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

/// 构造带 result 的 fake JSON-RPC 响应动作。
pub fn respond(result: Value) -> Value {
    json!({"respond": {"result": result}})
}

/// 构造带 error 的 fake JSON-RPC 响应动作。
pub fn respond_error(code: i64, message: &str) -> Value {
    json!({"respond": {"error": {"code": code, "message": message}}})
}

/// 构造发送异步通知或非匹配消息的动作。
pub fn send(message: Value) -> Value {
    json!({"send": message})
}

/// 构造捕获请求 params 的动作。
pub fn capture_params(path: &Path) -> Value {
    json!({"capture": {"path": path, "value": "params"}})
}

/// 捕获完整 JSON-RPC request，便于断言控制 lane 的 method、id 与 params。
pub fn capture_request(path: &Path) -> Value {
    json!({"capture": {"path": path, "value": "request"}})
}

/// 构造写入固定文本的动作。
pub fn write_text(path: &Path, text: &str) -> Value {
    json!({"write": {"path": path, "text": text}})
}

/// 将 fake app-server 当前进程 ID 写入文件。
pub fn write_pid(path: &Path) -> Value {
    json!({"write_pid": {"path": path}})
}

/// 构造向 fake server stderr 输出文本的动作。
pub fn print_stderr(text: &str) -> Value {
    json!({"stderr": text})
}

/// 构造以指定退出码结束 fake server 的动作。
pub fn exit(code: i32) -> Value {
    json!({"exit": code})
}

/// 构造阻塞指定毫秒数的动作。
pub fn sleep_ms(delay_ms: u64) -> Value {
    json!({"sleep_ms": delay_ms})
}

/// 构造最小 initialize result。
pub fn initialize_result() -> Value {
    json!({"userAgent": "fake", "platformFamily": "local", "platformOs": "test"})
}

/// 构造带 AgentLoop 与 provider capability 的响应对象。
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
        "providerConfiguration": {
            "source": "process_env",
            "snapshotId": "provider_snapshot_fake_server",
            "configured": true,
            "configurationBlocker": null,
            "apiKeyPresent": true,
            "baseUrlPresent": true,
            "modelPresent": true,
        }
    })
}

/// 构造测试用 thread 对象。
pub fn thread(thread_id: &str) -> Value {
    json!({"threadId": thread_id, "model": null, "cwd": null, "lastTurnStatus": "active"})
}

/// 构造测试用 turn 对象。
pub fn turn(turn_id: &str, thread_id: &str, status: &str) -> Value {
    json!({
        "turnId": turn_id,
        "threadId": thread_id,
        "status": status,
    })
}

// 缓存 fake app-server 二进制路径，避免每个测试重复构建。
fn helper_binary() -> PathBuf {
    static HELPER_BINARY: OnceLock<PathBuf> = OnceLock::new();
    HELPER_BINARY.get_or_init(locate_or_build_helper).clone()
}

// 定位已有 helper，缺失时编译 fake app-server 测试目标。
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

// 在当前测试 target 目录中选择与源码时间匹配的 helper 二进制。
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

// 取 fake app-server 源文件中最新的修改时间。
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

// 从测试 crate manifest 推导 workspace 根目录。
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}
