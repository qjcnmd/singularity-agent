//! bash 工具行为测试（经 `bash/mod.rs` 的 `#[path]` 纳入）。

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;
use tempfile::tempdir;

use super::capture::{CaptureState, INTERNAL_TAIL_MAX_BYTES};
use super::exec::{OUTPUT_TRUNCATED_BACKGROUND_NOTE, spawn_shell};
use super::pump::Utf8Decoder;
use super::shell::bash_shell_command;
use crate::tools::registry::{ExecuteContext, ToolExecution, ToolRegistry};
use crate::tools::test_support::context;

fn run(command: &str, timeout_ms: Option<u64>) -> ToolExecution {
    let dir = tempdir().expect("temp dir");
    let mut args = json!({ "command": command });
    if let Some(ms) = timeout_ms {
        args["timeout_ms"] = json!(ms);
    }
    ToolRegistry::new()
        .execute("bash", context(args, dir.path()))
        .expect("execute")
}

#[test]
fn utf8_decoder_reassembles_every_split_boundary() {
    let bytes = "前界\r\n后".as_bytes();
    for split in 0..=bytes.len() {
        let mut decoder = Utf8Decoder::default();
        let mut output = decoder.decode(&bytes[..split], false);
        output.push_str(&decoder.decode(&bytes[split..], true));
        assert_eq!(output, "前界\n后", "split={split}");
    }
}

#[test]
fn utf8_decoder_replaces_only_incomplete_tail_at_eof() {
    let mut decoder = Utf8Decoder::default();
    assert_eq!(decoder.decode(&[0xe5, 0xa4], false), "");
    assert_eq!(decoder.decode(&[], true), "\u{FFFD}");
    let mut invalid = Utf8Decoder::default();
    assert_eq!(invalid.decode(&[0xff, b'a'], true), "\u{FFFD}a");
}

#[test]
fn stdout_and_stderr_decoders_keep_independent_carry_state() {
    let mut stdout = Utf8Decoder::default();
    let mut stderr = Utf8Decoder::default();
    let stdout_first = "输".as_bytes();
    let stderr_first = "错".as_bytes();
    assert_eq!(stdout.decode(&stdout_first[..1], false), "");
    assert_eq!(stderr.decode(&stderr_first[..1], false), "");
    assert_eq!(stdout.decode(&stdout_first[1..], true), "输");
    assert_eq!(stderr.decode(&stderr_first[1..], true), "错");
}

#[test]
fn small_output_is_returned_in_full() {
    let result = run("echo hello", None);
    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert!(
        result.content.contains("hello"),
        "content: {}",
        result.content
    );
    assert!(
        !result.content.contains("[Showing"),
        "small output must not be truncated: {}",
        result.content
    );
}

#[test]
fn timeout_ms_lower_bound_is_accepted() {
    let result = run("sleep 0.01", Some(1));
    assert!(result.is_error, "1ms should time out");
    assert!(
        result.content.contains("timed out"),
        "content: {}",
        result.content
    );
}

#[test]
fn timeout_ms_large_values_are_accepted() {
    let result = run("echo ok", Some(600_000_000));
    assert!(!result.is_error, "content: {}", result.content);
    assert!(result.content.contains("ok"));
}

#[test]
fn omitted_timeout_runs_to_completion() {
    let started = Instant::now();
    let result = run("sleep 2; echo late", None);
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("late"),
        "content: {}",
        result.content
    );
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "command must not be killed by an implicit timeout"
    );
}

#[test]
fn timeout_ms_wrong_types_are_typed_errors() {
    let dir = tempdir().expect("temp dir");
    for bad in [
        json!("120000"),
        json!(1.5),
        json!(-1),
        json!(null),
        json!(true),
        json!({"ms": 1}),
    ] {
        let args = json!({"command": "echo should-not-run", "timeout_ms": bad});
        let result = ToolRegistry::new()
            .execute("bash", context(args, dir.path()))
            .expect("execute");
        assert!(result.is_error, "bad timeout {bad:?} was accepted");
        assert!(
            result.content.contains("timeout_ms")
                && (result.content.contains("invalid timeout_ms")
                    || result.content.contains("must be of type integer")
                    || result.content.contains("must be >= 1")),
            "content: {}",
            result.content
        );
    }
}

#[test]
fn timeout_ms_zero_is_rejected_before_spawn() {
    let result = run("echo should-not-run", Some(0));
    assert!(result.is_error, "content: {}", result.content);
    assert!(result.content.contains("invalid timeout_ms"));
}

#[test]
fn non_zero_exit_code_is_error() {
    let result = run("exit 3", None);
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("Command exited with code 3"),
        "content: {}",
        result.content
    );
}

#[test]
fn large_output_truncates_to_tail_and_spills_full_output() {
    let dir = tempdir().expect("temp dir");
    let content = (1..=2500)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let file = dir.path().join("large.txt");
    std::fs::write(&file, content).expect("write fixture");
    let result = run(&format!("cat \"{}\"", file.display()), None);
    assert!(!result.is_error, "unexpected error: {}", result.content);
    assert!(result.content.contains("line 2500"), "tail must be kept");
    assert!(
        !result.content.lines().any(|line| line == "line 1"),
        "head must be dropped"
    );
    assert!(
        result.content.starts_with("line 501"),
        "tail starts at first kept line"
    );
    assert!(result.content.contains("[Showing lines"), "missing note");
    // 截断发生时必须给出完整输出文件路径，且文件内容包含被截掉的头部。
    let full_output_line = result
        .content
        .lines()
        .find(|line| line.starts_with("Full output: "))
        .expect("truncated output must carry a Full output path");
    let spill_path = Path::new(full_output_line.trim_start_matches("Full output: "));
    let spilled = std::fs::read_to_string(spill_path)
        .unwrap_or_else(|error| panic!("read spill file {}: {error}", spill_path.display()));
    for line in ["line 1", "line 2", "line 1250", "line 2500"] {
        assert!(
            spilled.lines().any(|candidate| candidate == line),
            "spill file must contain {line}"
        );
    }
}

#[test]
fn small_output_never_spills() {
    let result = run("echo hello", None);
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        !result.content.contains("Full output:"),
        "untruncated output must not reference a spill file, content: {}",
        result.content
    );
}

#[test]
fn spill_append_failure_drops_writer_and_never_emits_full_output_path() {
    let mut state = CaptureState {
        command_slug: "test".to_string(),
        ..CaptureState::default()
    };
    // 一次性写入超过内部尾部上限的内容，触发 spill 创建。
    let big = "x".repeat(INTERNAL_TAIL_MAX_BYTES + 1);
    state.ingest(&big);
    assert!(state.spill.is_some(), "truncation must create a spill");
    // 以同一路径的只读句柄替换写句柄：写入只读句柄必然失败
    // （Unix O_RDONLY 写回 EBADF，Windows 非写访问句柄被拒）。
    let mut spill = state.spill.take().expect("spill");
    let path = spill.path.clone();
    spill.file = std::fs::File::open(&path).expect("open spill read-only");
    state.spill = Some(spill);
    state.ingest("more output");
    assert!(
        state.spill.is_none(),
        "spill must be dropped after an append failure"
    );
    assert!(state.spill_failed, "append failure must be recorded");
    assert!(
        state.spill_path().is_none(),
        "no path may be exposed after an append failure"
    );
    let progress = state.final_progress();
    assert!(
        !progress.output_text.contains("Full output:"),
        "no fake Full output line after an append failure"
    );
}

#[test]
fn timeout_terminates_and_marks_error() {
    let result = run("sleep 10", Some(300));
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("timed out after 300 ms"),
        "content: {}",
        result.content
    );
}

#[test]
fn cancellation_terminates_shell_tree_promptly() {
    let dir = tempdir().expect("temp dir");
    let token = singularity_core::CancellationToken::new();
    let worker_token = token.clone();
    let cwd = dir.path().to_path_buf();
    // 保持一个后代存活，使平台进程树包含路径（而不只是终止 shell）
    // 得到实际验证。
    let command = "sleep 30 & wait".to_string();
    let started = Instant::now();
    let worker = thread::spawn(move || {
        ToolRegistry::new().execute(
            "bash",
            ExecuteContext {
                args: json!({"command": command}),
                cwd: &cwd,
                signal: Some(&worker_token),
                on_update: None,
            },
        )
    });
    thread::sleep(Duration::from_millis(150));
    token.cancel();
    let result = worker.join().expect("bash worker").expect("execute");
    assert!(result.is_error, "cancelled command must be an error");
    assert!(
        result.content.contains("Command aborted"),
        "content: {}",
        result.content
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "cancellation must terminate promptly, took {elapsed:?}"
    );
}

/// D-004 树杀专项：取消后，仍持活的后代进程必须在有界时间内停止产出。
///
/// 后台循环每秒向文件追加一个 tick；主 shell 用 `wait` 挂住以保持整树存活。
/// 取消返回后观察若干采样点：若树杀失效，tick 会持续增长；树杀生效则
/// 相邻采样必然相等且只允许极少量 in-flight 余量。
#[test]
fn cancellation_stops_descendant_output_growth() {
    let dir = tempdir().expect("temp dir");
    let tick_file = dir.path().join("ticks.txt");
    let token = singularity_core::CancellationToken::new();
    let worker_token = token.clone();
    let cwd = dir.path().to_path_buf();
    let command = format!(
        "for i in $(seq 1 30); do echo t >> \"{}\"; sleep 1; done & wait",
        tick_file.display()
    );
    let worker = thread::spawn(move || {
        ToolRegistry::new().execute(
            "bash",
            ExecuteContext {
                args: json!({"command": command}),
                cwd: &cwd,
                signal: Some(&worker_token),
                on_update: None,
            },
        )
    });
    // 至少等一个 tick 落盘，确认后代循环已在运行。
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ticks = tick_count(&tick_file);
        if ticks > 0 || Instant::now() >= deadline {
            assert!(ticks > 0, "descendant loop must start producing ticks");
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let before = tick_count(&tick_file);
    thread::sleep(Duration::from_millis(150));
    token.cancel();
    let result = worker.join().expect("bash worker").expect("execute");
    assert!(
        result.content.contains("Command aborted"),
        "content: {}",
        result.content
    );
    // 取消后的三个采样点：相邻相等即证明产出已停止。
    let sample_a = sample_after(&tick_file, Duration::from_millis(1200));
    let sample_b = sample_after(&tick_file, Duration::from_millis(800));
    let sample_c = sample_after(&tick_file, Duration::from_millis(800));
    assert!(
        (sample_a == sample_b || sample_b == sample_c) && sample_c <= before + 2,
        "descendant must stop producing after tree kill: before={before} a={sample_a} b={sample_b} c={sample_c}"
    );
}

fn tick_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn sample_after(path: &Path, delay: Duration) -> usize {
    thread::sleep(delay);
    tick_count(path)
}

#[test]
fn background_process_holding_pipe_truncates_output_boundedly() {
    // 主 shell 退出后一个孙进程仍持有 stdout 写端：pump 做有界读，宽限后
    // 截断输出并给出标记，而非无限阻塞；后台进程本身不受强杀影响。
    let result = run("echo captured; (sleep 3) &", None);
    assert!(!result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("captured"),
        "content: {}",
        result.content
    );
    assert!(
        result.content.contains(OUTPUT_TRUNCATED_BACKGROUND_NOTE),
        "truncation note missing, content: {}",
        result.content
    );
}

#[cfg(windows)]
#[test]
fn windows_job_object_lifecycle_terminates_spawned_process() {
    let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
    let cwd = std::env::current_dir().expect("current directory");
    let mut managed = spawn_shell(
        &shell,
        &["/C".to_string(), "ping -n 10 127.0.0.1".to_string()],
        &cwd,
    )
    .expect("std Command spawn with job assignment");
    let _ = managed.child.stdout.take();
    let _ = managed.child.stderr.take();
    managed.kill_tree();
    let status = managed
        .wait_bounded(Duration::from_secs(5))
        .expect("wait child");
    assert!(!status.success(), "terminated process must not be success");
}

/// 信号终止的子进程必须被判为失败并报告信号号（Unix）。
#[cfg(unix)]
#[test]
fn signal_killed_process_is_reported_as_error() {
    let result = run("kill -9 $$", None);
    assert!(result.is_error, "content: {}", result.content);
    assert!(
        result.content.contains("Command terminated by signal 9"),
        "content: {}",
        result.content
    );
}

#[cfg(windows)]
#[test]
fn missing_bash_reports_configuration_error_instead_of_cmd_fallback() {
    let error = bash_shell_command("echo should-not-run", None).expect_err("missing bash");
    assert!(error.contains("Git for Windows"), "{error}");
    assert!(error.contains("bash.exe"), "{error}");
    assert!(
        error.contains("https://git-scm.com/install/windows"),
        "{error}"
    );
    assert!(!error.to_ascii_lowercase().contains("cmd"), "{error}");
}
