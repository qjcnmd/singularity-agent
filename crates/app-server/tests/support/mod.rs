#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Returns a temporary home outside any ancestor Git repository.
///
/// The product intentionally rejects `SINGULARITY_HOME` inside the current
/// repository. Some machines configure the system temp directory itself as a
/// Git repository, so integration tests must not assume `tempdir()` is safe.
pub fn isolated_home() -> tempfile::TempDir {
    let candidate = tempfile::tempdir().expect("temp home");
    if !has_git_ancestor(candidate.path()) {
        return candidate;
    }

    #[cfg(windows)]
    let fallback_parent = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\Temp"));
    #[cfg(not(windows))]
    let fallback_parent = PathBuf::from("/tmp");

    std::fs::create_dir_all(&fallback_parent).expect("fallback temp parent");
    let fallback = tempfile::tempdir_in(&fallback_parent).expect("fallback temp home");
    assert!(
        !has_git_ancestor(fallback.path()),
        "temporary home must be outside every ancestor Git repository: {}",
        fallback.path().display()
    );
    fallback
}

fn has_git_ancestor(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

pub fn app_server_bin() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_singularity_app_server") {
        let binary = PathBuf::from(path);
        assert!(
            binary.is_file(),
            "app-server binary missing: {}",
            binary.display()
        );
        return binary;
    }
    let current_exe = std::env::current_exe().expect("current test binary");
    let profile_dir = current_exe
        .parent()
        .and_then(Path::parent)
        .expect("profile dir");
    let binary = profile_dir.join(format!(
        "singularity_app_server{}",
        std::env::consts::EXE_SUFFIX
    ));
    assert!(
        binary.is_file(),
        "app-server binary missing: {}",
        binary.display()
    );
    binary
}

pub struct JsonOutput {
    receiver: Receiver<Value>,
    buffered: Vec<Value>,
    diagnostics: Arc<ProcessDiagnostics>,
}

struct ProcessDiagnostics {
    binary: PathBuf,
    cwd: PathBuf,
    home: PathBuf,
    child: Arc<Mutex<Child>>,
    stderr: Mutex<String>,
    stderr_complete: (Mutex<bool>, Condvar),
}

impl ProcessDiagnostics {
    fn failure_context(&self) -> String {
        let (complete_lock, complete_cv) = &self.stderr_complete;
        let mut complete = complete_lock.lock().expect("stderr completion lock");
        if !*complete {
            let (updated, _) = complete_cv
                .wait_timeout(complete, Duration::from_millis(100))
                .expect("stderr completion wait");
            complete = updated;
        }
        let stderr = self.stderr.lock().expect("stderr buffer lock");
        let exit = self
            .child
            .lock()
            .expect("child lock")
            .try_wait()
            .map(format_exit_status)
            .unwrap_or_else(|error| format!("<poll failed: {error}>"));
        format!(
            "binary={} cwd={} SINGULARITY_HOME={} child_exit={} stderr_complete={} stderr={:?}",
            self.binary.display(),
            self.cwd.display(),
            self.home.display(),
            exit,
            *complete,
            stderr.trim_end(),
        )
    }
}

fn format_exit_status(status: Option<ExitStatus>) -> String {
    match status {
        Some(status) => status.to_string(),
        None => "<running>".to_string(),
    }
}

impl JsonOutput {
    /// 服务器失败上下文（stderr 与退出状态），供测试断言拼接。
    /// 仅 steer_transport 测试目标使用，app_server 目标不引用。
    #[allow(dead_code)]
    pub fn failure_context(&self) -> String {
        self.diagnostics.failure_context()
    }

    pub fn recv_id(&mut self, id: i64, timeout: Duration) -> Value {
        self.recv_where(timeout, |message| message["id"] == id)
    }

    pub fn recv_where(&mut self, timeout: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.buffered.iter().position(&predicate) {
            return self.buffered.remove(index);
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("timed out waiting for app-server output");
            let message = self
                .receiver
                .recv_timeout(remaining)
                .unwrap_or_else(|error| {
                    panic!(
                        "app-server output message: {error}; buffered: {:?}; {}",
                        self.buffered,
                        self.diagnostics.failure_context(),
                    )
                });
            if predicate(&message) {
                return message;
            }
            self.buffered.push(message);
        }
    }
}

pub struct AppServerProcess {
    child: Arc<Mutex<Child>>,
    pub input: ChildStdin,
    pub output: JsonOutput,
}

impl AppServerProcess {
    pub fn spawn(cwd: &Path, home: &Path, base_url: &str) -> Self {
        let provider_env: Vec<(&str, &str)> = vec![
            ("SINGULARITY_MODEL_PROVIDER", "openai_compatible"),
            ("SINGULARITY_MODEL", "test-model"),
            ("SINGULARITY_BASE_URL", base_url),
            ("SINGULARITY_API_KEY", "test-secret"),
        ];
        Self::spawn_with_provider_env(cwd, home, &provider_env)
    }

    /// 变体：通过用户配置目录（`{home}/config.json` + `{home}/auth.json`）
    /// 提供 provider 选择，不注入 SINGULARITY_MODEL/BASE_URL/API_KEY——
    /// 环境层会覆盖用户配置层，注入这些变量会把快照打回单模型 legacy 形态。
    /// 仅 steer_transport 测试目标使用，app_server 目标不引用。
    #[allow(dead_code)]
    pub fn spawn_with_user_config(cwd: &Path, home: &Path) -> Self {
        Self::spawn_with_provider_env(cwd, home, &[])
    }

    fn spawn_with_provider_env(cwd: &Path, home: &Path, provider_env: &[(&str, &str)]) -> Self {
        let binary = app_server_bin();
        let mut command = Command::new(&binary);
        command
            .current_dir(cwd)
            .env("SINGULARITY_HOME", home)
            // 防止宿主环境泄漏覆盖用户配置层。
            .env_remove("SINGULARITY_MODEL_PROVIDER")
            .env_remove("SINGULARITY_MODEL")
            .env_remove("SINGULARITY_BASE_URL")
            .env_remove("SINGULARITY_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in provider_env {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap_or_else(|error| {
            panic!(
                "spawn app-server failed: binary={} cwd={} SINGULARITY_HOME={} error={error}",
                binary.display(),
                cwd.display(),
                home.display(),
            )
        });
        let input = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let child = Arc::new(Mutex::new(child));
        let diagnostics = Arc::new(ProcessDiagnostics {
            binary,
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            child: Arc::clone(&child),
            stderr: Mutex::new(String::new()),
            stderr_complete: (Mutex::new(false), Condvar::new()),
        });
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(message) = serde_json::from_str(&line) else {
                    continue;
                };
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        let stderr_diagnostics = Arc::clone(&diagnostics);
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                stderr_diagnostics
                    .stderr
                    .lock()
                    .expect("stderr buffer lock")
                    .push_str(&format!("{line}\n"));
            }
            let (complete_lock, complete_cv) = &stderr_diagnostics.stderr_complete;
            *complete_lock.lock().expect("stderr completion lock") = true;
            complete_cv.notify_all();
        });
        Self {
            child,
            input,
            output: JsonOutput {
                receiver,
                buffered: Vec::new(),
                diagnostics,
            },
        }
    }

    pub fn send_request(&mut self, id: i64, method: &str, params: Value) {
        send_json(
            &mut self.input,
            serde_json::json!({"jsonrpc":"2.0","method":method,"id":id,"params":params}),
        );
    }

    pub fn initialize(&mut self) {
        self.send_request(
            1,
            "initialize",
            serde_json::json!({
                "clientInfo": {
                    "name": "app-server-test",
                    "title": "App Server Test",
                    "version": "0.1.0"
                }
            }),
        );
        assert_eq!(
            self.output.recv_id(1, Duration::from_secs(5))["result"]["platformFamily"],
            "local"
        );
        send_json(
            &mut self.input,
            serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        );
    }

    pub fn shutdown(&mut self) {
        self.send_request(2, "server/shutdown", serde_json::json!({}));
        let response = self.output.recv_id(2, Duration::from_secs(5));
        assert_eq!(response["result"]["shutdown"], true);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self
                .child
                .lock()
                .expect("child lock")
                .try_wait()
                .expect("poll child")
            {
                assert!(status.success(), "app-server exited with {status}");
                return;
            }
            if Instant::now() >= deadline {
                let mut child = self.child.lock().expect("child lock");
                let _ = child.kill();
                let _ = child.wait();
                panic!("app-server did not exit after shutdown");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for AppServerProcess {
    fn drop(&mut self) {
        let mut child = self.child.lock().expect("child lock");
        if child
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(false)
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn send_json(input: &mut impl Write, message: Value) {
    writeln!(input, "{message}").expect("write app-server request");
    input.flush().expect("flush app-server request");
}
