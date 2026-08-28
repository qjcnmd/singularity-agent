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
#![allow(clippy::unwrap_used, clippy::expect_used)] // 测试断言惯例

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
        spawn_with_finish_reason(reply_text, "stop")
    }

    pub fn spawn_with_finish_reason(
        reply_text: &'static str,
        finish_reason: &'static str,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    serve_once(&mut stream, reply_text, finish_reason);
                });
            }
        });
        format!("http://{addr}/v1")
    }

    fn serve_once(stream: &mut std::net::TcpStream, reply_text: &str, finish_reason: &str) {
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
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{finish_reason}\"}}],\"usage\":{{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}}}\n\n\
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

#[cfg(windows)]
#[test]
fn all_execution_modes_fail_at_startup_without_git_bash() {
    let home = isolated_home();
    let cases: &[&[&str]] = &[
        &[],
        &["--print", "must not start"],
        &["--json", "must not start"],
    ];
    for args in cases {
        let output = sg()
            .args(*args)
            .env("SINGULARITY_HOME", &home.path)
            .env("PATH", "")
            .env_remove("ProgramFiles")
            .env_remove("ProgramFiles(x86)")
            .env_remove("ProgramW6432")
            .env_remove("SINGULARITY_MODEL")
            .env_remove("SINGULARITY_BASE_URL")
            .env_remove("SINGULARITY_API_KEY")
            .env_remove("SINGULARITY_MODEL_PROVIDER")
            .output()
            .expect("run sg without Git Bash");

        assert!(!output.status.success(), "args: {args:?}");
        let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
        assert!(stderr.contains("Git for Windows"), "stderr: {stderr}");
        assert!(stderr.contains("bash.exe"), "stderr: {stderr}");
        assert!(
            stderr.contains("https://git-scm.com/install/windows"),
            "stderr: {stderr}"
        );
        let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
        if args.first() == Some(&"--json") {
            let summary: Value = serde_json::from_str(
                stdout_lines(&stdout)
                    .last()
                    .expect("JSON mode must retain its failed summary"),
            )
            .expect("summary JSON");
            assert_eq!(
                summary
                    .pointer("/summary/turn/status")
                    .and_then(Value::as_str),
                Some("failed")
            );
            // thread 未解析时整体省略该字段，不得写入伪造的哨兵值。
            assert!(summary.pointer("/summary/thread").is_none());
            assert!(!stdout.contains("unresolved"));
        } else {
            assert!(stdout.is_empty(), "args: {args:?}");
        }
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
fn length_truncation_is_persisted_and_projected_by_headless_modes() {
    let print_home = isolated_home();
    let base_url = fake_server::spawn_with_finish_reason("partial", "length");
    let output = sg()
        .args(["--print", "produce a long response"])
        .env("SINGULARITY_HOME", &print_home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .output()
        .expect("run truncated sg --print");
    assert!(
        output.status.success(),
        "length is a normal terminal status"
    );
    assert_eq!(String::from_utf8(output.stdout).expect("utf8"), "partial\n");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("Response was truncated before completion."),
        "stderr: {stderr}"
    );

    let rollout = std::fs::read_dir(sessions_dir(&print_home.path))
        .expect("sessions directory")
        .map(|entry| entry.expect("session entry").path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("session rollout");
    let persisted = std::fs::read_to_string(rollout).expect("read rollout");
    let assistant = persisted
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry.pointer("/message/role").and_then(Value::as_str) == Some("assistant"))
        .expect("assistant message entry");
    assert_eq!(
        assistant
            .pointer("/message/stopReason")
            .and_then(Value::as_str),
        Some("length")
    );

    let json_home = isolated_home();
    let output = sg()
        .args(["--json", "produce a long response"])
        .env("SINGULARITY_HOME", &json_home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .output()
        .expect("run truncated sg --json");
    assert!(
        output.status.success(),
        "length is a normal terminal status"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let summary: Value =
        serde_json::from_str(stdout_lines(&stdout).last().expect("terminal summary line"))
            .expect("summary JSON");
    assert_eq!(
        summary
            .pointer("/summary/turn/truncated")
            .and_then(Value::as_bool),
        Some(true)
    );
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
    assert!(
        summary.pointer("/turn/truncated").is_none(),
        "ordinary completion must omit the optional truncated field"
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
fn home_inside_repository_is_rejected_before_directory_creation() {
    let directory = tempfile::tempdir().expect("temp dir");
    let repository = directory.path().join("repo");
    std::fs::create_dir_all(repository.join(".git")).expect("git marker");
    let home = repository.join("nested").join("home");

    let output = sg()
        .args(["--print", "must not start"])
        .current_dir(&repository)
        .env("SINGULARITY_HOME", &home)
        .output()
        .expect("run sg with repository-contained home");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must not be inside"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.exists(), "validation must run before creating home");
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

/// 从最新会话文件中取出首条 user 消息文本（验证 goal 拼装）。
fn first_user_message(home: &Path) -> String {
    let rollout = std::fs::read_dir(sessions_dir(home))
        .expect("sessions directory")
        .map(|entry| entry.expect("session entry").path())
        .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .expect("session rollout");
    let persisted = std::fs::read_to_string(rollout).expect("read rollout");
    let user = persisted
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry.pointer("/message/role").and_then(Value::as_str) == Some("user"))
        .expect("user message entry");
    // content 为块数组：[{"type":"text","text":...}]。
    user.pointer("/message/content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.pointer("/text"))
        .and_then(Value::as_str)
        .expect("user message text block")
        .to_string()
}

#[test]
fn piped_stdin_alone_becomes_the_goal() {
    let home = isolated_home();
    let base_url = fake_server::spawn("done");
    let output = sg()
        .arg("--print")
        .write_stdin("fix the flaky test")
        .env("SINGULARITY_HOME", &home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .output()
        .expect("run sg --print with piped stdin");
    assert!(
        output.status.success(),
        "piped stdin alone must run; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).expect("utf8"), "done\n");
    assert_eq!(first_user_message(&home.path), "fix the flaky test");
}

#[test]
fn piped_stdin_is_appended_to_positional_goal() {
    let home = isolated_home();
    let base_url = fake_server::spawn("done");
    let output = sg()
        .args(["--print", "Reproduce the bug"])
        .write_stdin("trace:\nstep 1\nstep 2")
        .env("SINGULARITY_HOME", &home.path)
        .env("SINGULARITY_MODEL", "fake-model")
        .env("SINGULARITY_BASE_URL", &base_url)
        .env("SINGULARITY_API_KEY", "test-key-placeholder")
        .output()
        .expect("run sg --print with goal and piped stdin");
    assert!(
        output.status.success(),
        "goal + piped stdin must run; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        first_user_message(&home.path),
        "Reproduce the bug\n\n--- piped input ---\ntrace:\nstep 1\nstep 2"
    );
}

#[test]
fn oversized_piped_stdin_fails_as_preparation_error() {
    let home = isolated_home();
    let output = sg()
        .arg("--json")
        .write_stdin("x".repeat(1024 * 1024 + 1))
        .env("SINGULARITY_HOME", &home.path)
        .output()
        .expect("run sg --json with oversized stdin");
    assert!(!output.status.success(), "oversized piped stdin must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("byte limit"),
        "stderr must name the limit: {stderr}"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let summary: Value = serde_json::from_str(
        stdout_lines(&stdout)
            .last()
            .expect("JSON mode must retain its failed summary"),
    )
    .expect("summary JSON");
    assert_eq!(
        summary
            .pointer("/summary/turn/status")
            .and_then(Value::as_str),
        Some("failed")
    );
}

#[test]
fn empty_piped_stdin_keeps_goal_required_error() {
    let home = isolated_home();
    for mode in ["--print", "--json"] {
        let output = sg()
            .arg(mode)
            .write_stdin("")
            .env("SINGULARITY_HOME", &home.path)
            .output()
            .expect("run sg with empty piped stdin");
        assert!(
            !output.status.success(),
            "{mode} with empty stdin must still require a goal"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("a goal is required"), "stderr: {stderr}");
    }
}
