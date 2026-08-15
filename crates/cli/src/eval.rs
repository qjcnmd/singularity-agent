//! `sg eval` 轻量回归评估工具（裁决 2，独立于产品核心）。
//!
//! 固定任务集 × 模型列表（默认 5 题 × 2 模型 = 10 cell）全并行；每个 cell：
//! 干净 workspace 副本 → 子进程跑真实 `sg run --json`（禁止 mock）→ 收集会话
//! 文件（rollout）+ turn usage → 独立运行 `checker.sh` 判分（exit 0=通过，
//! 绝不采信 agent 自报）→ 聚合指标，按模型分组输出，结果 JSON 落盘。
//!
//! checker.sh 约定：exit 0 = 通过；exit 1 = 失败；exit 2 = 部分通过；
//! 其他 exit = checker 异常（按失败处理并标记）。

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 默认评估配置路径（相对当前工作目录）。
const DEFAULT_CONFIG_PATH: &str = "evaluations/eval-config.json";
/// 默认每 cell 超时（秒）。
const DEFAULT_TIMEOUT_SECS: u64 = 1800;
/// 默认并行度。
const DEFAULT_MAX_PARALLEL: usize = 5;

/// `evaluations/eval-config.json` 的 schema。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EvalConfig {
    /// 任务 id 列表（`evaluations/tasks/<id>/`）。
    pub tasks: Vec<String>,
    /// 模型 selector 列表（`provider/model#variant`）。
    pub models: Vec<String>,
    /// 结果输出目录（相对当前工作目录）。
    pub output_dir: String,
    /// 每 cell 超时秒数（可选，默认 1800）。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 并行 cell 上限（可选，默认 5；超出部分排队等待）。
    #[serde(default)]
    pub max_parallel: Option<usize>,
    /// 判分使用的 bash 可执行文件（可选；默认探测 Git for Windows 安装）。
    /// 不能依赖 PATH 中的裸 `bash`：Windows 上 CreateProcess 会优先命中
    /// System32 的 WSL bash，把 Windows 路径当 Linux 路径解析。
    #[serde(default)]
    pub bash_path: Option<String>,
}

impl EvalConfig {
    fn load(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|error| format!("failed to read eval config {}: {error}", path.display()))?;
        serde_json::from_str(&content).map_err(|error| format!("invalid eval config: {error}"))
    }
}

/// 一个已加载的任务（`evaluations/tasks/<id>/`）。
struct Task {
    id: String,
    instruction: String,
    workspace: PathBuf,
    checker: PathBuf,
}

fn load_task(tasks_root: &Path, id: &str) -> Result<Task, String> {
    let dir = tasks_root.join(id);
    let instruction_path = dir.join("instruction.md");
    let workspace = dir.join("workspace");
    let checker = dir.join("checker.sh");
    if !instruction_path.is_file() {
        return Err(format!("task {id}: missing instruction.md"));
    }
    if !workspace.is_dir() {
        return Err(format!("task {id}: missing workspace/"));
    }
    if !checker.is_file() {
        return Err(format!("task {id}: missing checker.sh"));
    }
    let instruction = fs::read_to_string(&instruction_path)
        .map_err(|error| format!("task {id}: failed to read instruction.md: {error}"))?;
    Ok(Task {
        id: id.to_string(),
        instruction,
        workspace,
        // 绝对路径：checker 由 bash 在 workspace 副本的 cwd 下运行。
        checker: std::path::absolute(&checker)
            .unwrap_or(checker)
            .to_path_buf(),
    })
}

/// 单个 cell 的原始结果（序列化到 `cell.json` 与聚合层）。
#[derive(Debug, Clone, Serialize)]
struct CellResult {
    task_id: String,
    model: String,
    /// passed / failed / partial / interrupted / crashed / timed_out。
    status: String,
    checker_exit: Option<i32>,
    /// cell 总墙钟秒数（含 sg run 与 checker）。
    duration_secs: f64,
    /// turn usage（协议 wire 投影；无终态 turn 时为 null）。
    usage: Option<Value>,
    /// cached/input；input 为 0 时 null。
    cache_hit_ratio: Option<f64>,
    /// rollout 中统计的工具调用数与失败数。
    tool_calls: u64,
    tool_failures: u64,
    /// provider transport retry 无持久化数据源，恒为 null。
    retries: Option<u64>,
    /// 重复动作数（相同 tool_name+args 出现超过一次的数量）。
    duplicate_actions: u64,
    /// rollout 条目时间戳拆解（毫秒）。
    breakdown_ms: Breakdown,
    /// 相对输出目录的 cell 子目录（rollout/日志存档）。
    cell_dir: String,
    /// checker 输出摘录（有限长度）。
    checker_output: String,
    /// sg run 失败原因摘要（crashed/timed_out 时非空）。
    error: Option<String>,
}

/// rollout 时间戳拆解（按条目类型归属相邻间隔）。
#[derive(Debug, Clone, Default, Serialize)]
struct Breakdown {
    /// assistant 段（模型调用间隔）毫秒。
    model_ms: u64,
    /// toolResult 段（工具执行间隔）毫秒。
    tool_ms: u64,
    /// 其余间隔（user/compaction/custom 等）毫秒。
    other_ms: u64,
}

/// 按模型分组的聚合视图（写入 `results.json` 的 `by_model`）。
#[derive(Debug, Clone, Default, Serialize)]
struct ModelAggregate {
    cells: usize,
    passed: usize,
    failed: usize,
    partial: usize,
    interrupted: usize,
    crashed: usize,
    timed_out: usize,
    total_duration_secs: f64,
    total_tokens: u64,
    avg_cache_hit_ratio: Option<f64>,
    total_cost_estimate: f64,
    total_tool_calls: u64,
    total_tool_failures: u64,
}

/// 执行完整评估并落盘结果。
pub(crate) fn run_eval(
    config_path: Option<PathBuf>,
    tasks_override: Option<&str>,
    models_override: Option<&str>,
    max_parallel_override: Option<usize>,
    timeout_override: Option<u64>,
) -> Result<(), String> {
    let path = config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let mut config = EvalConfig::load(&path)?;
    if let Some(tasks) = tasks_override {
        config.tasks = split_csv(tasks);
    }
    if let Some(models) = models_override {
        config.models = split_csv(models);
    }
    if config.tasks.is_empty() || config.models.is_empty() {
        return Err(format!(
            "eval config {} must define non-empty tasks and models",
            path.display()
        ));
    }
    let timeout = timeout_override.unwrap_or(config.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let max_parallel = max_parallel_override
        .or(config.max_parallel)
        .unwrap_or(DEFAULT_MAX_PARALLEL);

    let output_root = PathBuf::from(&config.output_dir);
    fs::create_dir_all(&output_root).map_err(|error| {
        format!(
            "failed to create output dir {}: {error}",
            output_root.display()
        )
    })?;
    let run_id = format_run_id();
    let run_dir = output_root.join(&run_id);
    let started_at = run_id.clone();
    let cells_dir = run_dir.join("cells");
    fs::create_dir_all(&cells_dir)
        .map_err(|error| format!("failed to create run dir {}: {error}", run_dir.display()))?;

    let tasks_root = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tasks");
    let tasks: Vec<Task> = config
        .tasks
        .iter()
        .map(|id| load_task(&tasks_root, id))
        .collect::<Result<_, _>>()?;

    // 10 cell 全并行：线程池直接按 cell 数开线程（数量固定且很小）。
    let started = Instant::now();
    let bash = config
        .bash_path
        .clone()
        .or_else(resolve_default_bash)
        .ok_or_else(|| "no usable bash found; set eval-config.json bash_path".to_string())?;
    let cell_results = run_cells(
        &tasks,
        &config.models,
        max_parallel,
        timeout,
        &bash,
        &cells_dir,
    )?;
    let total_secs = started.elapsed().as_secs_f64();

    let mut by_model: HashMap<String, ModelAggregate> = HashMap::new();
    for cell in &cell_results {
        let agg = by_model.entry(cell.model.clone()).or_default();
        agg.cells += 1;
        match cell.status.as_str() {
            "passed" => agg.passed += 1,
            "partial" => agg.partial += 1,
            "interrupted" => agg.interrupted += 1,
            "crashed" => agg.crashed += 1,
            "timed_out" => agg.timed_out += 1,
            _ => agg.failed += 1,
        }
        agg.total_duration_secs += cell.duration_secs;
        if let Some(usage) = &cell.usage {
            agg.total_tokens += usage["total_tokens"].as_u64().unwrap_or(0);
            if let Some(ratio) = cell.cache_hit_ratio {
                agg.avg_cache_hit_ratio = Some(
                    agg.avg_cache_hit_ratio.unwrap_or(0.0)
                        + (ratio - agg.avg_cache_hit_ratio.unwrap_or(0.0)) / agg.cells as f64,
                );
            }
            agg.total_cost_estimate += usage["cost_estimate"].as_f64().unwrap_or(0.0);
        }
        agg.total_tool_calls += cell.tool_calls;
        agg.total_tool_failures += cell.tool_failures;
    }

    let mut totals = ModelAggregate::default();
    for agg in by_model.values() {
        totals.cells += agg.cells;
        totals.passed += agg.passed;
        totals.failed += agg.failed;
        totals.partial += agg.partial;
        totals.interrupted += agg.interrupted;
        totals.crashed += agg.crashed;
        totals.timed_out += agg.timed_out;
        totals.total_duration_secs += agg.total_duration_secs;
        totals.total_tokens += agg.total_tokens;
        totals.total_cost_estimate += agg.total_cost_estimate;
        totals.total_tool_calls += agg.total_tool_calls;
        totals.total_tool_failures += agg.total_tool_failures;
    }

    let results = json!({
        "run_id": run_id,
        "started_at": started_at,
        "duration_secs": total_secs,
        "config": {
            "tasks": config.tasks,
            "models": config.models,
            "timeout_secs": timeout,
            "max_parallel": max_parallel,
            "config_path": path.display().to_string(),
        },
        "cells": cell_results,
        "by_model": by_model,
        "total_duration_secs": total_secs,
        "total_tokens": totals.total_tokens,
        "total_cost_estimate": totals.total_cost_estimate,
    });
    let results_path = run_dir.join("results.json");
    fs::write(
        &results_path,
        serde_json::to_string_pretty(&results).map_err(|error| format!("serialize: {error}"))?,
    )
    .map_err(|error| format!("failed to write {}: {error}", results_path.display()))?;

    println!("eval {} finished in {total_secs:.1}s", run_dir.display());
    for (model, agg) in by_model {
        println!(
            "  {model}: {} cells, passed={} failed={} partial={} interrupted={} crashed={} timed_out={} tokens={} cost=${:.4}",
            agg.cells,
            agg.passed,
            agg.failed,
            agg.partial,
            agg.interrupted,
            agg.crashed,
            agg.timed_out,
            agg.total_tokens,
            agg.total_cost_estimate
        );
    }
    println!("results: {}", results_path.display());
    Ok(())
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn format_run_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("run-{millis}")
}

/// 并行执行全部 (task, model) cell。
fn run_cells(
    tasks: &[Task],
    models: &[String],
    max_parallel: usize,
    timeout: u64,
    bash: &str,
    cells_dir: &Path,
) -> Result<Vec<CellResult>, String> {
    let next = Arc::new(AtomicUsize::new(0));
    let total = tasks.len() * models.len();
    let mut results: Vec<CellResult> = thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..max_parallel.min(total) {
            let next = Arc::clone(&next);
            handles.push(scope.spawn(move || {
                let mut local = Vec::new();
                loop {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    if index >= total {
                        break;
                    }
                    let task = &tasks[index / models.len()];
                    let model = &models[index % models.len()];
                    let cell_dir = cells_dir.join(cell_slug(task, model));
                    local.push(run_cell(task, model, timeout, bash, &cell_dir));
                }
                local
            }));
        }
        let mut results = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(mut local) => results.append(&mut local),
                Err(_) => eprintln!("eval cell thread panicked; results may be incomplete"),
            }
        }
        results
    });
    // 按 (task, model) 稳定排序输出。
    results.sort_by(|a, b| {
        (a.task_id.as_str(), a.model.as_str()).cmp(&(b.task_id.as_str(), b.model.as_str()))
    });
    Ok(results)
}

fn cell_slug(task: &Task, model: &str) -> String {
    format!("{}__{}", task.id, model.replace(['/', '#'], "_"))
}

/// 执行单个 cell：复制 workspace → sg run（真实链路）→ checker.sh 判分 → 聚合。
fn run_cell(task: &Task, model: &str, timeout: u64, bash: &str, cell_dir: &Path) -> CellResult {
    let cell_started = Instant::now();
    let mut result = CellResult {
        task_id: task.id.clone(),
        model: model.to_string(),
        status: "crashed".to_string(),
        checker_exit: None,
        duration_secs: 0.0,
        usage: None,
        cache_hit_ratio: None,
        tool_calls: 0,
        tool_failures: 0,
        retries: None,
        duplicate_actions: 0,
        breakdown_ms: Breakdown::default(),
        cell_dir: cell_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| cell_dir.display().to_string()),
        checker_output: String::new(),
        error: None,
    };
    let _ = fs::create_dir_all(cell_dir);

    // 1) 干净任务副本（临时目录）：<copy>/workspace + <copy>/checker.sh。
    let (task_copy, workspace_copy) = match prepare_workspace(task) {
        Ok(paths) => paths,
        Err(error) => {
            result.error = Some(error);
            result.duration_secs = cell_started.elapsed().as_secs_f64();
            let _ = write_cell_artifacts(&result, cell_dir, None, None, None);
            return result;
        }
    };

    // 2) 子进程跑真实 sg run 链路。
    let run = run_sg(&task.instruction, model, &workspace_copy, timeout);

    // 3) 复制 rollout（会话文件）到 cell 目录存档。
    let rollout = copy_rollout(&workspace_copy, cell_dir);

    // 4) 独立 checker.sh 判分（exit 0 = 通过）：执行任务副本内的 checker.sh，
    // 其 `dirname $0` 解析到副本根，`cd 副本根/workspace` 检查模型修改后的副本。
    let checker = run_checker(
        bash,
        &task_copy.join("checker.sh"),
        &workspace_copy,
        &mut result.checker_output,
    );

    // 5) 指标聚合。
    let usage = run.as_ref().ok().and_then(|sg| sg.turn_usage.clone());
    result.usage = usage.clone();
    result.cache_hit_ratio = usage.as_ref().and_then(cache_hit_ratio);
    if let Some(rollout) = &rollout {
        let parsed = parse_rollout(rollout);
        result.tool_calls = parsed.tool_calls;
        result.tool_failures = parsed.tool_failures;
        result.duplicate_actions = parsed.duplicate_actions;
        result.breakdown_ms = parsed.breakdown;
    }
    result.duration_secs = cell_started.elapsed().as_secs_f64();

    // 状态判定：超时 > 链路崩溃 > 中断 > checker。
    match &run {
        Err(error) => {
            result.error = Some(error.clone());
            result.status = "crashed".to_string();
        }
        Ok(sg) => {
            if sg.timed_out {
                result.status = "timed_out".to_string();
                result.error = Some("sg run exceeded per-cell timeout".to_string());
            } else if sg.turn_interrupted {
                result.status = "interrupted".to_string();
            } else if sg.turn_failed {
                result.status = "crashed".to_string();
                result.error = Some("agent loop failed (turn failed/blocked)".to_string());
            } else if let Some(exit) = checker {
                match exit {
                    0 => result.status = "passed".to_string(),
                    2 => result.status = "partial".to_string(),
                    _ => result.status = "failed".to_string(),
                }
            } else {
                result.status = "failed".to_string();
            }
            result.checker_exit = checker;
        }
    }

    let _ = write_cell_artifacts(
        &result,
        cell_dir,
        run.as_ref().ok(),
        checker.as_ref(),
        rollout.as_deref(),
    );
    let _ = fs::remove_dir_all(&task_copy);
    result
}

/// sg run 子进程的运行结果。
struct SgRun {
    timed_out: bool,
    turn_interrupted: bool,
    turn_failed: bool,
    turn_usage: Option<Value>,
    stdout: String,
    stderr: String,
}

/// 执行 `sg run <instruction> --model <selector> --json`（cwd = workspace 副本）。
fn run_sg(instruction: &str, model: &str, cwd: &Path, timeout: u64) -> Result<SgRun, String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("failed to resolve sg binary: {error}"))?;
    let mut child = Command::new(&exe)
        .arg("run")
        .arg(instruction)
        .arg("--model")
        .arg(model)
        .arg("--json")
        .current_dir(cwd)
        // 每 cell 独立 app-server 进程（stdio 承载）：保持 eval 各 cell 的隔离工作区
        // 语义与并行等价（不共享 daemon 单 worker 槽位，与 daemon 化之前行为一致）。
        .env("SINGULARITY_APP_SERVER_TRANSPORT", "stdio")
        // 非交互：stdin 不继承终端（否则 eval 从终端启动时子进程会误判交互并触发 trust 询问）。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to spawn sg run: {error}"))?;

    // 边运行边读 stdout/stderr：Windows 匿名管道缓冲很小，等待结束后再读
    // 会让输出大的 --json 事件流阻塞子进程，直到超时。
    let stdout = Arc::new(std::sync::Mutex::new(String::new()));
    let stderr = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(mut out) = child.stdout.take() {
        let captured = Arc::clone(&stdout);
        thread::spawn(move || {
            let mut buffer = String::new();
            if out.read_to_string(&mut buffer).is_ok() {
                *captured
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = buffer;
            }
        });
    }
    if let Some(mut err) = child.stderr.take() {
        let captured = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut buffer = String::new();
            if err.read_to_string(&mut buffer).is_ok() {
                *captured
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = buffer;
            }
        });
    }

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_process_tree(&mut child);
                    break None;
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("failed to poll sg run: {error}")),
        }
    };
    let read_stdout = || {
        stdout
            .lock()
            .map(|captured| captured.clone())
            .unwrap_or_default()
    };
    let read_stderr = || {
        stderr
            .lock()
            .map(|captured| captured.clone())
            .unwrap_or_default()
    };
    if timed_out {
        let _ = child.wait();
        return Ok(SgRun {
            timed_out: true,
            turn_interrupted: false,
            turn_failed: false,
            turn_usage: None,
            stdout: read_stdout(),
            stderr: read_stderr(),
        });
    }
    let status = status.ok_or_else(|| "sg run terminated without status".to_string())?;
    let stdout_text = read_stdout();
    let stderr_text = read_stderr();
    if !status.success() {
        // 失败时仍尝试解析 stdout 中的 JSON（json 模式失败前可能已输出）。
        let parsed = parse_sg_json(&stdout_text);
        if let Some(parsed) = parsed {
            return Ok(SgRun {
                timed_out: false,
                turn_interrupted: parsed.interrupted,
                turn_failed: parsed.failed,
                turn_usage: parsed.usage,
                stdout: stdout_text,
                stderr: stderr_text,
            });
        }
        // 无 JSON 输出的失败是链路级失败（provider/transport 错误等），
        // 保留 stderr 摘要供 cell 报告。
        return Err(format!(
            "sg run failed with exit {status}; stderr: {}",
            truncate(&stderr_text, 500)
        ));
    }
    let parsed = match parse_sg_json(&stdout_text) {
        Some(parsed) => parsed,
        None => {
            return Err(format!(
                "sg run exited 0 but produced no parseable --json output; stderr: {}",
                truncate(&stderr_text, 500)
            ));
        }
    };
    Ok(SgRun {
        timed_out: false,
        turn_interrupted: parsed.interrupted,
        turn_failed: parsed.failed,
        turn_usage: parsed.usage,
        stdout: stdout_text,
        stderr: stderr_text,
    })
}

/// 从 `sg run --json` 输出解析 turn 状态与 usage。
struct ParsedSgJson {
    interrupted: bool,
    failed: bool,
    usage: Option<Value>,
}

fn parse_sg_json(stdout: &str) -> Option<ParsedSgJson> {
    let value: Value = serde_json::from_str(stdout.trim()).ok()?;
    let turn = value.get("turn")?;
    let status = turn.get("status").and_then(Value::as_str).unwrap_or("");
    let agent_loop_status = turn
        .get("agent_loop_status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let interrupted = status == "interrupted" || agent_loop_status == "cancelled";
    let failed =
        matches!(status, "failed" | "blocked") || matches!(agent_loop_status, "failed" | "blocked");
    Some(ParsedSgJson {
        interrupted,
        failed,
        usage: turn.get("model_usage").cloned(),
    })
}

/// 超时 kill：Windows 用 taskkill 杀进程树；其他平台 kill 直接子进程。
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let pid = child.id();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
    }
    #[cfg(not(windows))]
    {
        let _ = child.kill();
    }
}

/// 从 bash 可执行路径推导 Git for Windows 安装根（`.../Git/usr/bin/bash.exe` → `.../Git`）。
fn git_root_for_bash(bash: &str) -> Option<String> {
    let root = Path::new(bash)
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "usr"))?
        .parent()?
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .replace('\\', "/");
    Some(root)
}

/// 探测本机可用的 bash：优先 Git for Windows 的显式安装路径。
fn resolve_default_bash() -> Option<String> {
    for candidate in [
        "C:/Program Files/Git/usr/bin/bash.exe",
        "C:/Program Files/Git/bin/bash.exe",
    ] {
        if Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// 运行 checker.sh（cwd = workspace 副本），返回退出码。
fn run_checker(bash: &str, checker: &Path, workspace: &Path, output: &mut String) -> Option<i32> {
    // bash 把反斜杠当转义符：Windows 路径必须转正斜杠再作为参数。
    let checker_arg = checker.to_string_lossy().replace('\\', "/");
    let mut command = Command::new(bash);
    command
        .arg(&checker_arg)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // 受限 PATH 环境（如沙箱）可能不含 Git for Windows 的 MSYS 路径（/usr/bin 等），
    // 导致 checker.sh 内 dirname/cd 等 POSIX 工具找不到（实测 dirname: command not
    // found）。按 bash 所在 Git 安装注入 usr/bin、bin、mingw64/bin 前缀兜底。
    if let Some(git_root) = git_root_for_bash(bash) {
        let msys_path = [
            format!("{git_root}/usr/bin"),
            format!("{git_root}/bin"),
            format!("{git_root}/mingw64/bin"),
        ]
        .join(";");
        let current = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{msys_path};{current}"));
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            *output = format!("failed to spawn checker.sh: {error}");
            return None;
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    let status = child
        .wait()
        .map_err(|error| {
            *output = format!("failed to wait checker.sh: {error}");
        })
        .ok()?;
    *output = truncate(&format!("{stdout}\n{stderr}"), 2000);
    status.code()
}

/// 解析 rollout（会话 JSONL）的工具调用统计与时间拆解。
struct RolloutStats {
    tool_calls: u64,
    tool_failures: u64,
    duplicate_actions: u64,
    breakdown: Breakdown,
}

fn parse_rollout(path: &Path) -> RolloutStats {
    let mut tool_calls = 0u64;
    let mut tool_failures = 0u64;
    let mut breakdown = Breakdown::default();
    let mut action_seen: HashMap<String, usize> = HashMap::new();
    let mut prev_timestamp: Option<u64> = None;
    let mut prev_kind = String::new();

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => {
            return RolloutStats {
                tool_calls,
                tool_failures,
                duplicate_actions: 0,
                breakdown,
            };
        }
    };
    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let entry_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if entry_type != "message" {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        // 条目时间戳（ISO8601）解析为 unix 毫秒。
        let timestamp = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso_millis);
        if let (Some(prev), Some(now)) = (prev_timestamp, timestamp) {
            let delta_ms = now.saturating_sub(prev);
            match prev_kind.as_str() {
                "assistant" => breakdown.model_ms += delta_ms,
                "toolResult" => breakdown.tool_ms += delta_ms,
                _ => breakdown.other_ms += delta_ms,
            }
        }
        prev_timestamp = timestamp;
        prev_kind = role.to_string();
        match role {
            "assistant" => {
                if message.get("toolName").is_some() {
                    tool_calls += 1;
                    let action = json!({
                        "tool": message.get("toolName").and_then(Value::as_str).unwrap_or(""),
                        "args": message.get("args").cloned().unwrap_or(Value::Null),
                    });
                    let key = action.to_string();
                    *action_seen.entry(key).or_insert(0) += 1;
                }
            }
            "toolResult" => {
                let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                if tool_result_is_error(content) {
                    tool_failures += 1;
                }
            }
            _ => {}
        }
    }
    let duplicate_actions = action_seen.values().filter(|count| **count > 1).count() as u64;
    RolloutStats {
        tool_calls,
        tool_failures,
        duplicate_actions,
        breakdown,
    }
}

/// 工具失败启发式（bash 失败有确定性状态行前缀；edit/write 失败是 error_result 文本）。
fn tool_result_is_error(content: &str) -> bool {
    const FAILURE_MARKERS: &[&str] = &[
        "Command exited with code",
        "Command aborted",
        "Command timed out after",
        "failed to spawn shell",
        "tool execution failed",
        "Could not edit file",
        "Could not write file",
        "missing required parameter",
        "Operation aborted",
    ];
    FAILURE_MARKERS
        .iter()
        .any(|marker| content.contains(marker))
}

/// 解析 ISO8601 毫秒时间戳（`2025-01-15T10:30:00.000Z`）为 unix 毫秒。
fn parse_iso_millis(text: &str) -> Option<u64> {
    let (date, rest) = text.split_once('T')?;
    let rest = rest.trim_end_matches('Z');
    let (time, millis_part) = rest.rsplit_once('.')?;
    let millis: u64 = millis_part.parse().ok()?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u64 = parts.next()?.parse().ok()?;
    let day: u64 = parts.next()?.parse().ok()?;
    let mut parts = time.split(':');
    let hour: u64 = parts.next()?.parse().ok()?;
    let minute: u64 = parts.next()?.parse().ok()?;
    let second: u64 = parts.next()?.parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    Some((days as u64 * 86_400 + hour * 3600 + minute * 60 + second) * 1000 + millis)
}

/// 公历日数（Howard Hinnant 算法），月份/日期超界时返回 None。
fn days_from_civil(year: i64, month: u64, day: u64) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = month as i64 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn cache_hit_ratio(usage: &Value) -> Option<f64> {
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input == 0 {
        None
    } else {
        Some(cached as f64 / input as f64)
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        format!("{}... ({} bytes total)", &text[..max], text.len())
    }
}

/// 复制任务 workspace 到独立临时目录（干净副本）。
fn prepare_workspace(task: &Task) -> Result<(PathBuf, PathBuf), String> {
    // 不用 std::env::temp_dir()：从 git-bash 启动时它解析为 D:\Temp（MSYS 的
    // /tmp 挂载点，usertemp），agent 的 bash pwd 会显示 /tmp/... 误导文件操作。
    // 固定用 Windows 用户 Temp（bash 显示 /c/Users/...），agent 更可能用相对路径。
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|local| local.join("Temp").join("sg-eval"))
        .unwrap_or_else(|| std::env::temp_dir().join("sg-eval"));
    fs::create_dir_all(&base)
        .map_err(|error| format!("failed to create eval temp root: {error}"))?;
    let copy = base.join(format!(
        "{}_{}",
        task.id,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // 任务根副本：<copy>/workspace（模型工作区）+ <copy>/checker.sh（判分脚本）。
    // checker.sh 通过 `dirname $0` 定位 workspace，必须与 workspace 同在副本内，
    // 否则它检查的是源任务目录的原始 workspace（模型修改从未被判分）。
    copy_dir(&task.workspace, &copy.join("workspace"))?;
    fs::copy(&task.checker, copy.join("checker.sh"))
        .map_err(|error| format!("failed to copy checker.sh: {error}"))?;
    let workspace_copy = copy.join("workspace");
    Ok((copy, workspace_copy))
}

/// 递归复制目录（跳过 `.git`；跟随符号链接的目标文件复制为普通文件）。
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to)
        .map_err(|error| format!("failed to create {}: {error}", to.display()))?;
    let entries = fs::read_dir(from)
        .map_err(|error| format!("failed to read {}: {error}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read dir entry: {error}"))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|error| format!("file type {}: {error}", src.display()))?;
        if file_type.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst).map_err(|error| {
                format!(
                    "failed to copy {} -> {}: {error}",
                    src.display(),
                    dst.display()
                )
            })?;
        }
    }
    Ok(())
}

/// 将会话文件（rollout）从 workspace 副本复制到 cell 目录，返回存档路径。
fn copy_rollout(workspace: &Path, cell_dir: &Path) -> Option<PathBuf> {
    let sessions = workspace.join(".singularity").join("agent-sessions");
    let entries = fs::read_dir(&sessions).ok()?;
    let mut latest: Option<(PathBuf, SystemTime)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        if latest.as_ref().is_none_or(|(_, time)| modified > *time) {
            latest = Some((path, modified));
        }
    }
    let (source, _) = latest?;
    let target = cell_dir.join("rollout.jsonl");
    fs::copy(&source, &target).ok()?;
    Some(target)
}

/// 将 cell 的 stdout/stderr/checker 输出与 rollout 落盘到 cell 目录。
fn write_cell_artifacts(
    result: &CellResult,
    cell_dir: &Path,
    sg: Option<&SgRun>,
    checker_exit: Option<&i32>,
    rollout: Option<&Path>,
) -> Result<(), String> {
    if let Some(sg) = sg {
        fs::write(cell_dir.join("sg_stdout.txt"), &sg.stdout)
            .map_err(|error| format!("write sg_stdout: {error}"))?;
        fs::write(cell_dir.join("sg_stderr.txt"), &sg.stderr)
            .map_err(|error| format!("write sg_stderr: {error}"))?;
    }
    if checker_exit.is_some() {
        fs::write(cell_dir.join("checker_stdout.txt"), &result.checker_output)
            .map_err(|error| format!("write checker_stdout: {error}"))?;
    }
    if rollout.is_some() {
        // rollout 已复制到 cell_dir/rollout.jsonl；在 cell.json 中记录路径。
        let _ = rollout;
    }
    fs::write(
        cell_dir.join("cell.json"),
        serde_json::to_string_pretty(result).map_err(|error| format!("serialize: {error}"))?,
    )
    .map_err(|error| format!("write cell.json: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_millis_handles_utc_timestamps() {
        assert_eq!(
            parse_iso_millis("2026-08-14T09:15:30.000Z"),
            Some(1_786_698_930_000)
        );
        assert_eq!(
            parse_iso_millis("2026-08-14T10:59:59.500Z"),
            Some(1_786_705_199_500)
        );
        assert_eq!(parse_iso_millis("garbage"), None);
        assert_eq!(parse_iso_millis("2026-13-01T00:00:00.000Z"), None);
    }

    #[test]
    fn tool_result_is_error_matches_failure_markers() {
        assert!(tool_result_is_error(
            "Command exited with code 1\nls: no such file"
        ));
        assert!(tool_result_is_error("Command timed out after 5000 ms"));
        assert!(tool_result_is_error("tool execution failed: unknown tool"));
        assert!(tool_result_is_error(
            "Could not edit file: src/a.py. No match."
        ));
        assert!(tool_result_is_error("missing required parameter \"path\""));
        assert!(!tool_result_is_error("(no output)"));
        assert!(!tool_result_is_error("hello.txt created, 5 bytes"));
    }

    #[test]
    fn parse_sg_json_extracts_turn_state_and_usage() {
        let stdout = r#"{"thread":{"thread_id":"t1"},"turn":{"turn_id":"tr1","thread_id":"t1","status":"completed","agent_loop_status":"completed","model_usage":{"input_tokens":100,"output_tokens":50,"total_tokens":150,"cached_input_tokens":40,"reasoning_tokens":10,"cost_estimate":0.01}},"events":[]}"#;
        let parsed = parse_sg_json(stdout).expect("parseable");
        assert!(!parsed.interrupted);
        assert!(!parsed.failed);
        let usage = parsed.usage.expect("usage present");
        assert_eq!(usage["total_tokens"], 150);
        assert_eq!(usage["cached_input_tokens"], 40);
    }

    #[test]
    fn parse_sg_json_detects_interrupted_and_failed() {
        let interrupted =
            parse_sg_json(r#"{"turn":{"status":"interrupted","agent_loop_status":"cancelled"}}"#)
                .expect("parseable");
        assert!(interrupted.interrupted);
        let failed = parse_sg_json(r#"{"turn":{"status":"failed","agent_loop_status":"failed"}}"#)
            .expect("parseable");
        assert!(failed.failed);
        assert!(parse_sg_json("not json").is_none());
    }

    #[test]
    fn cache_hit_ratio_computes_cached_over_input() {
        let usage = json!({"input_tokens": 200, "cached_input_tokens": 50});
        assert_eq!(cache_hit_ratio(&usage), Some(0.25));
        let zero_input = json!({"input_tokens": 0, "cached_input_tokens": 0});
        assert_eq!(cache_hit_ratio(&zero_input), None);
    }

    #[test]
    fn parse_rollout_counts_tools_and_breaks_down_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-14T09:00:00.000Z"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-14T09:00:01.000Z","message":{"role":"user","content":"fix it"}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-14T09:00:02.000Z","message":{"role":"assistant","content":"","toolCallId":"c1","toolName":"bash","args":{"command":"ls"}}}"#,
            r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-08-14T09:00:05.000Z","message":{"role":"toolResult","content":"Command exited with code 1","toolCallId":"c1","toolName":"bash"}}"#,
            r#"{"type":"message","id":"m4","parentId":"m3","timestamp":"2026-08-14T09:00:07.000Z","message":{"role":"assistant","content":"done"}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let stats = parse_rollout(&path);
        assert_eq!(stats.tool_calls, 1);
        assert_eq!(stats.tool_failures, 1);
        assert_eq!(stats.duplicate_actions, 0);
        // 差值按前一条类型归属：user->assistant 1s（other）；assistant->toolResult 3s（model）；
        // toolResult->assistant 2s（tool）。
        assert_eq!(stats.breakdown.model_ms, 3000);
        assert_eq!(stats.breakdown.tool_ms, 2000);
    }

    #[test]
    fn parse_rollout_counts_duplicate_actions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let lines = [
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-14T09:00:00.000Z"}"#,
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-08-14T09:00:01.000Z","message":{"role":"assistant","content":"","toolCallId":"c1","toolName":"bash","args":{"command":"ls"}}}"#,
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-08-14T09:00:02.000Z","message":{"role":"assistant","content":"","toolCallId":"c2","toolName":"bash","args":{"command":"ls"}}}"#,
            r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-08-14T09:00:03.000Z","message":{"role":"assistant","content":"","toolCallId":"c3","toolName":"read","args":{"path":"a.txt"}}}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let stats = parse_rollout(&path);
        assert_eq!(stats.tool_calls, 3);
        // bash+ls 出现 2 次 = 1 个重复动作；read+a.txt 1 次。
        assert_eq!(stats.duplicate_actions, 1);
    }
}
