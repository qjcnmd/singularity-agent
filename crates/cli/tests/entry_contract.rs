//! `sg` 新双入口的黑盒合同测试。
//!
//! 产品合同（讨论裁决 2026-08-23）：
//! - 无参数进入长驻交互式 TUI；无终端时以明确诊断退出，不落入 clap 用法错误。
//! - `sg --print <goal>` 单次执行，stdout 只含最终 assistant 文本。
//! - `sg --json <goal>` 单次执行，stdout 为逐行 JSONL 事件，最后一行是
//!   `{"summary": {"thread": ..., "turn": ...}}` 终态行。
//! - `<goal>` 是无交互模式的必需位置参数；`--model` 只覆盖本次执行；
//!   `--session <id>` 指定既有 Thread；`--no-session` 关闭本次持久化。
//! - 旧管理型子命令（run/continue/session/threads/config）不得出现。

use assert_cmd::Command;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 本地假 Chat Completions SSE 服务器：黑盒驱动真实 Provider 栈。
mod fake_server {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// 启动返回固定 assistant 文本的假 chat 服务器；返回 base_url。
    pub fn spawn(reply_text: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    serve_once(&mut stream, reply_text);
                });
            }
        });
        format!("http://{addr}/v1")
    }

    fn serve_once(stream: &mut std::net::TcpStream, reply_text: &str) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    if let Some(position) = find_header_end(&buffer) {
                        break position;
                    }
                }
                Err(_) => return,
            }
        };
        if let Some(length) = content_length(&buffer[..header_end]) {
            let mut remaining = length.saturating_sub(buffer.len() - header_end);
            while remaining > 0 {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buffer.extend_from_slice(&chunk[..n]);
                        remaining -= n.min(remaining);
                    }
                    Err(_) => break,
                }
            }
        }
        let body = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{reply_text}\"}}}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}}}\n\n\
             data: [DONE]\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|p| p + 4)
    }

    fn content_length(headers: &[u8]) -> Option<usize> {
        let text = std::str::from_utf8(headers).ok()?;
        for line in text.lines() {
            if let Some(value) = line
                .split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim())
            {
                return value.parse().ok();
            }
        }
        None
    }
}

fn sg() -> Command {
    Command::cargo_bin("sg").expect("sg binary")
}

struct HomeGuard {
    _home: TempDir,
    path: PathBuf,
}

fn isolated_home() -> HomeGuard {
    let home = TempDir::new().expect("temporary SINGULARITY_HOME");
    let path = home.path().to_path_buf();
    HomeGuard { _home: home, path }
}

fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

fn stdout_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

#[test]
fn help_shows_only_new_entry_contract() {
    let output = sg()
        .arg("--help")
        .env("SINGULARITY_HOME", isolated_home().path)
        .output()
        .expect("run sg --help");
    assert!(output.status.success(), "sg --help must succeed");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    for flag in ["--print", "--json", "--session", "--no-session", "--model"] {
        assert!(stdout.contains(flag), "help must document {flag}");
    }
    for legacy in [
        "run",
        "continue",
        "threads",
        "config",
        "doctor",
        "import-env",
    ] {
        assert!(
            !stdout.contains(legacy),
            "help must not offer legacy management command {legacy}"
        );
    }
}

#[test]
fn print_writes_only_final_assistant_text() {
    let home = isolated_home();
    let base_url = fake_server::spawn("done");
    let output = sg()
        .args(["--print", "Reply with exactly: done"])
        .env("SINGULARITY_HOME", &home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .env_remove("SINGULARITY_MODEL_PROVIDER")
        .output()
        .expect("run sg --print");
    assert!(
        output.status.success(),
        "--print must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // stdout 是且只是最终 assistant 文本。
    assert_eq!(String::from_utf8(output.stdout).expect("utf8"), "done\n");
}

#[test]
fn json_emits_event_lines_and_terminal_summary() {
    let home = isolated_home();
    let base_url = fake_server::spawn("done");
    let output = sg()
        .args(["--json", "Reply with exactly: done"])
        .env("SINGULARITY_HOME", &home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .output()
        .expect("run sg --json");
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        output.status.success(),
        "--json must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = stdout_lines(&stdout);
    assert!(
        !lines.is_empty(),
        "--json must emit at least the summary line"
    );
    let mut thread_id: Option<String> = None;
    for line in &lines {
        let value: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("event line not JSON: {line}: {error}"));
        if let Some(id) = value
            .pointer("/summary/thread/threadId")
            .or_else(|| value.pointer("/params/threadId"))
            .and_then(Value::as_str)
        {
            match &thread_id {
                Some(existing) => {
                    assert_eq!(existing, id, "thread id must be stable across events")
                }
                None => thread_id = Some(id.to_string()),
            }
        }
    }
    let last: Value =
        serde_json::from_str(lines.last().expect("summary line")).expect("terminal summary line");
    let summary = last
        .get("summary")
        .expect("final line carries summary object");
    let status = summary
        .pointer("/turn/status")
        .and_then(Value::as_str)
        .expect("summary.turn.status");
    assert_eq!(
        status, "completed",
        "successful run must report completed turn"
    );
    assert!(
        summary.pointer("/thread/threadId").is_some(),
        "summary.thread.threadId must be present"
    );
    assert!(
        summary.pointer("/turn/usage").is_some(),
        "summary.turn.usage must be present"
    );
}

#[test]
fn goal_is_required_for_non_interactive_modes() {
    let home = isolated_home();
    for mode in ["--print", "--json"] {
        let output = sg()
            .arg(mode)
            .env("SINGULARITY_HOME", &home.path)
            .output()
            .expect("run sg without goal");
        assert!(
            !output.status.success(),
            "{mode} without a goal positional must fail"
        );
    }
}

#[test]
fn print_and_json_are_mutually_exclusive() {
    let home = isolated_home();
    let output = sg()
        .args(["--print", "--json", "goal"])
        .env("SINGULARITY_HOME", &home.path)
        .output()
        .expect("run sg with both modes");
    assert!(
        !output.status.success(),
        "--print and --json together must be rejected"
    );
}

#[test]
fn no_session_persists_nothing() {
    let home = isolated_home();
    let base_url = fake_server::spawn("done");
    let output = sg()
        .args(["--json", "Reply with exactly: done", "--no-session"])
        .env("SINGULARITY_HOME", &home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .output()
        .expect("run sg --no-session");
    assert!(
        output.status.success(),
        "--no-session run must succeed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sessions = sessions_dir(&home.path);
    let persisted = sessions
        .read_dir()
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert_eq!(persisted, 0, "--no-session must not write session files");
}

#[test]
fn session_resume_of_unknown_thread_fails_without_partial_output() {
    let home = isolated_home();
    let output = sg()
        .args([
            "--json",
            "Reply with exactly: done",
            "--session",
            "6f27b1b8-2b30-4b83-9d94-6e2d57d3e0a1",
        ])
        .env("SINGULARITY_HOME", &home.path)
        .output()
        .expect("run sg --session");
    assert!(
        !output.status.success(),
        "resuming an unknown thread must fail"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let lines = stdout_lines(&stdout);
    if let Some(last) = lines.last() {
        // 失败也必须以终态 summary 收尾，不能留下悬空事件流。
        let value: Value =
            serde_json::from_str(last).expect("failure path still emits a terminal summary line");
        assert!(
            value.get("summary").is_some(),
            "failure terminal line must carry summary"
        );
        assert_eq!(
            value
                .pointer("/summary/turn/status")
                .and_then(Value::as_str),
            Some("failed"),
            "unknown thread resume must end failed"
        );
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.is_empty() || !lines.is_empty(),
        "unknown thread resume must produce a diagnostic"
    );
}

#[test]
fn model_override_is_accepted_for_one_execution() {
    let home = isolated_home();
    let output = sg()
        .args([
            "--json",
            "Reply with exactly: done",
            "--model",
            "test-provider/test-model",
        ])
        .env("SINGULARITY_HOME", &home.path)
        .output()
        .expect("run sg --model");
    // 未配置 test-provider 时允许失败，但失败必须是配置/选择层诊断，
    // 而不是参数解析错误；这里只验证 flag 在新入口下被接受。
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "--model must be a recognized top-level flag: {stderr}"
    );
    assert!(
        !stderr.contains("unrecognized subcommand"),
        "new entry has no subcommands: {stderr}"
    );
}

#[test]
fn interactive_mode_requires_a_terminal() {
    let home = isolated_home();
    let output = sg()
        .write_stdin("")
        .env("SINGULARITY_HOME", &home.path)
        .output()
        .expect("run bare sg");
    assert!(
        !output.status.success(),
        "bare sg without a terminal cannot run interactively"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        !combined.contains("usage:"),
        "interactive entry must not fall back to clap usage output: {stderr}"
    );
}
