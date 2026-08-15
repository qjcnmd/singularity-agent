use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

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
}

impl JsonOutput {
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
                        "app-server output message: {error}; buffered: {:?}",
                        self.buffered
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
    pub child: Child,
    pub input: ChildStdin,
    pub output: JsonOutput,
}

impl AppServerProcess {
    pub fn spawn(cwd: &Path, home: &Path, base_url: &str) -> Self {
        let mut child = Command::new(app_server_bin())
            .current_dir(cwd)
            .env("SINGULARITY_HOME", home)
            .env("SINGULARITY_MODEL_PROVIDER", "openai_compatible")
            .env("SINGULARITY_MODEL", "gpt-test")
            .env("SINGULARITY_BASE_URL", base_url)
            .env("SINGULARITY_API_KEY", "test-secret")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn app-server");
        let input = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
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
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let _ = line;
            }
        });
        Self {
            child,
            input,
            output: JsonOutput {
                receiver,
                buffered: Vec::new(),
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
            if let Some(status) = self.child.try_wait().expect("poll child") {
                assert!(status.success(), "app-server exited with {status}");
                return;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("app-server did not exit after shutdown");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for AppServerProcess {
    fn drop(&mut self) {
        if self
            .child
            .try_wait()
            .map(|status| status.is_none())
            .unwrap_or(false)
        {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn send_json(input: &mut impl Write, message: Value) {
    writeln!(input, "{message}").expect("write app-server request");
    input.flush().expect("flush app-server request");
}
