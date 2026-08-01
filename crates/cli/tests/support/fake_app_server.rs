//! 按 JSON scenario 驱动 fake app-server，用于 CLI 协议测试。

mod shared;

use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process;
use std::time::Duration;

// 读取 scenario、处理请求循环，并执行对应的 fake action。
fn main() {
    let Some(scenario_path) = std::env::var_os(shared::SCENARIO_ENV) else {
        return;
    };

    if let Err(error) = run(Path::new(&scenario_path)) {
        eprintln!("fake app-server failed: {error}");
        process::exit(1);
    }
}

// 加载 scenario 并按 method/调用次数分发交互。
fn run(scenario_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let scenario: Value = serde_json::from_reader(File::open(scenario_path)?)?;
    let mut stdout = io::stdout().lock();
    let mut next_event_sequence = 1_u64;

    if let Some(actions) = scenario.get("startup").and_then(Value::as_array) {
        execute_actions(actions, &Value::Null, &mut stdout, &mut next_event_sequence)?;
    }

    let methods = scenario
        .get("methods")
        .and_then(Value::as_object)
        .ok_or("scenario must contain a methods object")?;
    let method_trace = scenario.get("method_trace").and_then(Value::as_str);
    let mut request_counts = HashMap::<String, usize>::new();
    let stdin = io::stdin();

    for line in BufReader::new(stdin.lock()).lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or("request is missing method")?;

        if let Some(path) = method_trace {
            append_text(Path::new(path), &format!("{method}\n"))?;
        }

        let Some(interactions) = methods.get(method).and_then(Value::as_array) else {
            continue;
        };
        if interactions.is_empty() {
            continue;
        }

        let request_count = request_counts.entry(method.to_owned()).or_default();
        let interaction_index = (*request_count).min(interactions.len() - 1);
        *request_count += 1;
        let actions = interactions[interaction_index]
            .as_array()
            .ok_or("method interaction must be an action array")?;
        execute_actions(actions, &request, &mut stdout, &mut next_event_sequence)?;
    }

    Ok(())
}

// 执行响应、通知、捕获、延时和退出等测试动作。
fn execute_actions(
    actions: &[Value],
    request: &Value,
    stdout: &mut impl Write,
    next_event_sequence: &mut u64,
) -> Result<(), Box<dyn std::error::Error>> {
    for action in actions {
        if let Some(response) = action.get("respond") {
            let mut response = response
                .as_object()
                .ok_or("respond action must be an object")?
                .clone();
            response.insert(
                "id".to_owned(),
                request.get("id").cloned().unwrap_or(Value::Null),
            );
            write_json(stdout, &Value::Object(response))?;
        } else if let Some(message) = action.get("send") {
            let mut message = message.clone();
            decorate_event(&mut message, next_event_sequence)?;
            write_json(stdout, &message)?;
        } else if let Some(capture) = action.get("capture") {
            let path = capture
                .get("path")
                .and_then(Value::as_str)
                .ok_or("capture action is missing path")?;
            let captured = match capture.get("value").and_then(Value::as_str) {
                Some("params") => request.get("params").unwrap_or(&Value::Null),
                Some("request") | None => request,
                Some(value) => return Err(format!("unsupported capture value {value}").into()),
            };
            std::fs::write(path, serde_json::to_vec(captured)?)?;
        } else if let Some(write) = action.get("write") {
            let path = write
                .get("path")
                .and_then(Value::as_str)
                .ok_or("write action is missing path")?;
            let text = write
                .get("text")
                .and_then(Value::as_str)
                .ok_or("write action is missing text")?;
            std::fs::write(path, text)?;
        } else if let Some(text) = action.get("stderr").and_then(Value::as_str) {
            eprintln!("{text}");
        } else if let Some(delay_ms) = action.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(delay_ms));
        } else if let Some(exit_code) = action.get("exit").and_then(Value::as_i64) {
            let exit_code = i32::try_from(exit_code).map_err(|_| "exit code is out of range")?;
            process::exit(exit_code);
        } else {
            return Err(format!("unsupported fake app-server action: {action}").into());
        }
    }

    Ok(())
}

fn decorate_event(
    message: &mut Value,
    next_event_sequence: &mut u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(method) = message
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(());
    };
    let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if let Some(existing) = params.get("event") {
        if let Some(sequence) = existing.get("sequence").and_then(Value::as_u64) {
            *next_event_sequence = (*next_event_sequence).max(sequence.saturating_add(1));
        }
        return Ok(());
    }
    if method == "event/gap" {
        return Ok(());
    }
    let is_progress = matches!(
        method.as_str(),
        "item/agentMessage/delta" | "item/commandExecution/outputDelta"
    );
    let class = if is_progress { "progress" } else { "state" };
    let delivery = if is_progress {
        "best_effort"
    } else {
        "reliable"
    };
    let recovery_query = match method.as_str() {
        "thread/started" => params
            .get("thread")
            .and_then(|thread| thread.get("thread_id"))
            .and_then(Value::as_str)
            .map(|thread_id| json!({"method":"thread/read","params":{"threadId":thread_id}})),
        "turn/started" | "turn/completed" => params
            .get("turn")
            .and_then(|turn| turn.get("turn_id"))
            .and_then(Value::as_str)
            .map(|turn_id| json!({"method":"turn/status","params":{"turnId":turn_id}})),
        "approval/requested" => Some(json!({"method":"approval/list","params":{}})),
        _ => None,
    };
    let sequence = *next_event_sequence;
    *next_event_sequence = (*next_event_sequence).saturating_add(1);
    let mut metadata = json!({
        "sequence": sequence,
        "cursor": sequence,
        "class": class,
        "delivery": delivery,
    });
    if let Some(query) = recovery_query {
        metadata["recoveryQuery"] = query;
    }
    params.insert("event".to_string(), metadata);
    Ok(())
}

// 将 JSON 响应写成一行并立即刷新 stdout。
fn write_json(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

// 追加写入 method trace 或其他测试文本文件。
fn append_text(path: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}
