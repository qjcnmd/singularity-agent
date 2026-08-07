//! Explicit Evaluation process-restart recovery injection.
//!
//! This module owns only the short-lived child process and its stdio JSON-RPC client.  The
//! AppServer SQLite file remains the recovery authority: a marker is accepted only after the
//! persisted checkpoint and paired provider-attempt start are visible in that database.  No
//! elapsed-time guess is used to decide when to kill the first owner.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};
use singularity_core::CancellationToken;
use singularity_protocol::{
    TraceEvent, TraceSpanKind, TraceSpanPhase, TraceSpanStatus, TurnStatus,
};
use singularity_store::SessionStore;

const APP_SERVER_BIN_ENV: &str = "SINGULARITY_APP_SERVER_BIN";
const APP_SERVER_BINARY: &str = "singularity_app_server";
const APP_SERVER_DB_ENV: &str = "SINGULARITY_APP_SERVER_DB";
const RECOVERY_SANDBOX_MODE: &str = "workspace-write";
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Whether an explicitly injected trial produced a conclusive recovery observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryAttempt {
    /// No safe marker was observed, or the trace contained an unknown execution boundary.
    NotObserved,
    /// The resumed same-turn request reached a valid terminal completion.
    Completed,
    /// A marker was observed, but the resumed turn reached a non-completed terminal state.
    Failed,
}

/// Evidence returned by one independent AppServer child pair.
#[derive(Debug)]
pub(crate) struct RecoveryRunResult {
    pub(crate) attempt: RecoveryAttempt,
    pub(crate) trace: Vec<TraceEvent>,
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) reason: Option<String>,
}

impl RecoveryRunResult {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            attempt: RecoveryAttempt::NotObserved,
            trace: Vec::new(),
            thread_id: String::new(),
            turn_id: String::new(),
            reason: Some(reason.into()),
        }
    }
}

/// Execute one explicit process-kill and same-turn resume sequence.
pub(crate) fn run_recovery_trial(
    workspace: &Path,
    db_path: &Path,
    prompt: &str,
    model_selector: Option<&str>,
    cancellation: &CancellationToken,
) -> RecoveryRunResult {
    if let Err(error) = fs::create_dir_all(workspace) {
        return RecoveryRunResult::unavailable(format!("recovery workspace unavailable: {error}"));
    }
    if let Some(parent) = db_path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return RecoveryRunResult::unavailable(format!(
            "recovery database directory unavailable: {error}"
        ));
    }

    // Resolve every path before the child changes its current directory.  The same absolute
    // workspace and database paths are used for `spawn`, JSON-RPC `thread/start`, and the
    // marker/trace reads so recovery never observes a different file than the child writes.
    let (workspace, db_path) = match resolve_recovery_paths(workspace, db_path) {
        Ok(paths) => paths,
        Err(error) => return RecoveryRunResult::unavailable(error),
    };

    let mut first = match RecoveryClient::spawn(&workspace, &db_path) {
        Ok(client) => client,
        Err(error) => return RecoveryRunResult::unavailable(error),
    };
    if let Err(error) = first.initialize() {
        first.kill();
        return RecoveryRunResult::unavailable(error);
    }
    let thread_id = match first.thread_start(&workspace, model_selector) {
        Ok(thread_id) => thread_id,
        Err(error) => {
            first.kill();
            return RecoveryRunResult::unavailable(error);
        }
    };
    let turn_id = match first.turn_start(&thread_id, prompt) {
        Ok(turn_id) => turn_id,
        Err(error) => {
            first.kill();
            return RecoveryRunResult::unavailable(error);
        }
    };

    let marker = loop {
        if cancellation.is_cancelled() {
            first.kill();
            return RecoveryRunResult::unavailable(
                "evaluation cancelled during recovery marker observation",
            );
        }
        match recovery_marker(&db_path, &thread_id, &turn_id) {
            Ok(Some(marker)) => break marker,
            Ok(None) => {}
            Err(error) => {
                first.kill();
                return RecoveryRunResult::unavailable(error);
            }
        }
        match first.turn_status(&turn_id) {
            Ok(status) if status != "running" && status != "paused" && status != "suspended" => {
                first.kill();
                return RecoveryRunResult::unavailable(format!(
                    "provider attempt marker was not observed before turn terminal status {status}"
                ));
            }
            Ok(_) => {}
            Err(_) => {}
        }
        match first.child.try_wait() {
            Ok(Some(status)) => {
                return RecoveryRunResult::unavailable(format!(
                    "app-server exited before durable recovery marker: {status}"
                ));
            }
            Ok(None) => {}
            Err(error) => {
                first.kill();
                return RecoveryRunResult::unavailable(format!(
                    "failed to poll app-server before recovery marker: {error}"
                ));
            }
        }
        thread::sleep(POLL_INTERVAL);
    };

    // The marker is DB-backed and paired with a provider-attempt start. Kill immediately; the
    // polling cadence is never used as an interruption decision.
    first.kill();

    if cancellation.is_cancelled() {
        return RecoveryRunResult::unavailable("evaluation cancelled before recovery resume");
    }
    let mut resumed = match RecoveryClient::spawn(&workspace, &db_path) {
        Ok(client) => client,
        Err(error) => {
            return RecoveryRunResult {
                attempt: RecoveryAttempt::Failed,
                trace: marker.trace,
                thread_id,
                turn_id,
                reason: Some(error),
            };
        }
    };
    if let Err(error) = resumed.initialize() {
        resumed.kill();
        return RecoveryRunResult {
            attempt: RecoveryAttempt::Failed,
            trace: marker.trace,
            thread_id,
            turn_id,
            reason: Some(error),
        };
    }
    if let Err(error) = resumed.turn_resume(&turn_id) {
        let unknown = error.contains("unknown tool execution")
            || error.contains("unknown execution")
            || error.contains("cannot be resumed");
        resumed.kill();
        return RecoveryRunResult {
            attempt: if unknown {
                RecoveryAttempt::NotObserved
            } else {
                RecoveryAttempt::Failed
            },
            trace: marker.trace,
            thread_id,
            turn_id,
            reason: Some(error),
        };
    }

    let terminal_status = loop {
        if cancellation.is_cancelled() {
            resumed.kill();
            return RecoveryRunResult::unavailable("evaluation cancelled during recovery resume");
        }
        match resumed.turn_status(&turn_id) {
            Ok(status) if status != "running" && status != "paused" && status != "suspended" => {
                break status;
            }
            Ok(_) => {}
            Err(error) => {
                resumed.kill();
                return RecoveryRunResult {
                    attempt: RecoveryAttempt::Failed,
                    trace: marker.trace,
                    thread_id,
                    turn_id,
                    reason: Some(error),
                };
            }
        }
        match resumed.child.try_wait() {
            Ok(Some(status)) => {
                return RecoveryRunResult {
                    attempt: RecoveryAttempt::Failed,
                    trace: marker.trace,
                    thread_id,
                    turn_id,
                    reason: Some(format!(
                        "app-server exited before resumed turn terminalization: {status}"
                    )),
                };
            }
            Ok(None) => {}
            Err(error) => {
                resumed.kill();
                return RecoveryRunResult {
                    attempt: RecoveryAttempt::Failed,
                    trace: marker.trace,
                    thread_id,
                    turn_id,
                    reason: Some(format!("failed to poll resumed app-server: {error}")),
                };
            }
        }
        thread::sleep(POLL_INTERVAL);
    };
    resumed.shutdown();

    let trace = match load_trace(&db_path, &thread_id) {
        Ok(trace) => trace,
        Err(error) => {
            return RecoveryRunResult {
                attempt: RecoveryAttempt::NotObserved,
                trace: marker.trace,
                thread_id,
                turn_id,
                reason: Some(error),
            };
        }
    };
    let has_unknown_span = has_unknown_tool_or_sandbox_span(&trace, &turn_id);
    let has_terminal_completion = trace.iter().any(|event| {
        event.session_id == turn_id
            && event.span_kind == Some(TraceSpanKind::Turn)
            && event.span_phase == Some(TraceSpanPhase::End)
            && event.span_status == Some(TraceSpanStatus::Ok)
            && trace
                .iter()
                .position(|candidate| candidate.event_id == event.event_id)
                .is_some_and(|index| index > marker.event_index)
    });
    if has_unknown_span {
        return RecoveryRunResult {
            attempt: RecoveryAttempt::NotObserved,
            trace,
            thread_id,
            turn_id,
            reason: Some("recovery trace contains an unknown tool or sandbox span".to_string()),
        };
    }
    RecoveryRunResult {
        attempt: if terminal_status == "completed" && has_terminal_completion {
            RecoveryAttempt::Completed
        } else {
            RecoveryAttempt::Failed
        },
        trace,
        thread_id,
        turn_id,
        reason: (terminal_status != "completed")
            .then(|| format!("resumed turn reached terminal status {terminal_status}")),
    }
}

#[derive(Debug)]
struct RecoveryMarker {
    trace: Vec<TraceEvent>,
    event_index: usize,
}

fn recovery_marker(
    db_path: &Path,
    thread_id: &str,
    turn_id: &str,
) -> Result<Option<RecoveryMarker>, String> {
    if !db_path.is_file() {
        return Ok(None);
    }
    let store = match SessionStore::open(db_path) {
        Ok(store) => store,
        Err(error) => return Err(format!("failed to open recovery database: {error}")),
    };
    let checkpoint = match store.get_turn_checkpoint(turn_id) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) | Err(_) => return Ok(None),
    };
    if !checkpoint.is_object() {
        return Ok(None);
    }
    let trace = match store.list_trace(thread_id) {
        Ok(trace) => trace,
        Err(_) => return Ok(None),
    };
    let Ok(turn) = store.get_turn(turn_id) else {
        return Ok(None);
    };
    if turn.thread_id != thread_id {
        return Ok(None);
    }
    if !matches!(
        turn.status,
        TurnStatus::Running | TurnStatus::Paused | TurnStatus::Suspended
    ) {
        return Ok(None);
    }
    // An unknown or orphaned execution span means the owner cannot be identified safely.  Keep
    // the recovery boundary fail-closed before the injected kill is reached.
    if has_unknown_tool_or_sandbox_span(&trace, turn_id) {
        return Ok(None);
    }
    let Some(event_index) = open_provider_attempt_start(&trace, turn_id) else {
        return Ok(None);
    };
    Ok(Some(RecoveryMarker { trace, event_index }))
}

fn load_trace(db_path: &Path, thread_id: &str) -> Result<Vec<TraceEvent>, String> {
    let store = SessionStore::open(db_path)
        .map_err(|error| format!("failed to reopen recovery database: {error}"))?;
    store
        .list_trace(thread_id)
        .map_err(|error| format!("failed to read recovery trace: {error}"))
}

fn has_unknown_tool_or_sandbox_span(trace: &[TraceEvent], turn_id: &str) -> bool {
    let mut spans = BTreeMap::<String, (Option<&TraceEvent>, Option<&TraceEvent>)>::new();
    for event in trace {
        if event.session_id != turn_id {
            continue;
        }
        if !matches!(
            event.span_kind,
            Some(TraceSpanKind::ToolCall | TraceSpanKind::SandboxExecution)
        ) {
            continue;
        }
        if event.validate_span_lifecycle().is_err() {
            return true;
        }
        let Some(span_id) = event.span_id.as_deref().filter(|id| !id.trim().is_empty()) else {
            return true;
        };
        let entry = spans.entry(span_id.to_string()).or_default();
        match event.span_phase {
            Some(TraceSpanPhase::Start) if entry.0.is_none() => entry.0 = Some(event),
            Some(TraceSpanPhase::End) if entry.1.is_none() => entry.1 = Some(event),
            _ => return true,
        }
    }
    spans.into_iter().any(|(_, (start, end))| {
        let (Some(start), Some(end)) = (start, end) else {
            return true;
        };
        if start.span_kind != end.span_kind
            || start.parent_span_id != end.parent_span_id
            || !start
                .span_projection
                .as_ref()
                .is_some_and(|projection| projection.has_execution_identity(start.span_kind))
            || !end
                .span_projection
                .as_ref()
                .is_some_and(|projection| projection.has_execution_identity(end.span_kind))
        {
            return true;
        }
        match start.span_kind {
            Some(TraceSpanKind::ToolCall) => tool_span_identity_matches(start, end),
            Some(TraceSpanKind::SandboxExecution) => sandbox_span_is_strict_and_bound(end),
            _ => true,
        }
    })
}

trait RecoveryProjectionIdentity {
    fn has_execution_identity(&self, kind: Option<TraceSpanKind>) -> bool;
}

impl RecoveryProjectionIdentity for singularity_protocol::TraceSpanProjection {
    fn has_execution_identity(&self, kind: Option<TraceSpanKind>) -> bool {
        match kind {
            Some(TraceSpanKind::ToolCall) => self.tool.as_ref().is_some_and(|tool| {
                tool.tool_name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
                    && tool
                        .tool_call_id_digest
                        .as_deref()
                        .is_some_and(|digest| !digest.trim().is_empty())
                    && tool.tool_call_ordinal.is_some()
            }),
            Some(TraceSpanKind::SandboxExecution) => self.sandbox.as_ref().is_some_and(|sandbox| {
                sandbox
                    .command_id_digest
                    .as_deref()
                    .is_some_and(|digest| !digest.trim().is_empty())
            }),
            _ => false,
        }
    }
}

fn tool_span_identity_matches(start: &TraceEvent, end: &TraceEvent) -> bool {
    let Some(start_tool) = start.span_projection.as_ref().and_then(|p| p.tool.as_ref()) else {
        return false;
    };
    let Some(end_tool) = end.span_projection.as_ref().and_then(|p| p.tool.as_ref()) else {
        return false;
    };
    start_tool.tool_name == end_tool.tool_name
        && start_tool.tool_call_id_digest == end_tool.tool_call_id_digest
        && start_tool.tool_call_ordinal == end_tool.tool_call_ordinal
        && end_tool.status.is_some()
}

fn sandbox_span_is_strict_and_bound(end: &TraceEvent) -> bool {
    end.span_projection
        .as_ref()
        .and_then(|p| p.sandbox.as_ref())
        .is_some_and(|sandbox| {
            sandbox.command_id_binding_valid == Some(true)
                && sandbox.enforcement
                    == Some(singularity_protocol::TraceSandboxEnforcement::Strict)
                && sandbox.status.is_some()
                && sandbox.workspace_mutation.is_some()
        })
}

fn open_provider_attempt_start(trace: &[TraceEvent], turn_id: &str) -> Option<usize> {
    let mut open = BTreeMap::<String, (&TraceEvent, usize)>::new();
    for (index, event) in trace.iter().enumerate() {
        if event.session_id != turn_id || event.span_kind != Some(TraceSpanKind::ProviderAttempt) {
            continue;
        }
        if event.validate_span_lifecycle().is_err()
            || !event
                .span_projection
                .as_ref()
                .is_some_and(provider_span_identity_known)
        {
            return None;
        }
        let span_id = event
            .span_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())?;
        match event.span_phase {
            Some(TraceSpanPhase::Start) => {
                if open.insert(span_id.to_string(), (event, index)).is_some() {
                    return None;
                }
            }
            Some(TraceSpanPhase::End) => {
                let (start, _) = open.remove(span_id)?;
                let start_projection = start.span_projection.as_ref()?;
                let end_projection = event.span_projection.as_ref()?;
                if start.parent_span_id != event.parent_span_id
                    || !start_projection
                        .same_identity_attributes(TraceSpanKind::ProviderAttempt, end_projection)
                {
                    return None;
                }
            }
            None => return None,
        }
    }
    open.values().map(|(_, index)| *index).max()
}

fn provider_span_identity_known(projection: &singularity_protocol::TraceSpanProjection) -> bool {
    projection
        .provider_name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
        && projection
            .model_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
        && projection.protocol.is_some()
        && projection.operation_phase.is_some()
        && projection.attempt_index.is_some()
}

struct RecoveryClient {
    child: Child,
    stdin: Option<ChildStdin>,
    output: Receiver<Result<Value, String>>,
    reader: Option<JoinHandle<()>>,
    pending: VecDeque<Value>,
    next_id: i64,
    terminated: bool,
}

impl RecoveryClient {
    fn spawn(workspace: &Path, db_path: &Path) -> Result<Self, String> {
        let (workspace, db_path) = resolve_recovery_paths(workspace, db_path)?;
        let binary = app_server_bin()?;
        let mut child = std::process::Command::new(binary)
            .current_dir(&workspace)
            .env(APP_SERVER_DB_ENV, &db_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start recovery app-server: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "recovery app-server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "recovery app-server stdout unavailable".to_string())?;
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            let lines = BufReader::new(stdout).lines();
            for line in lines {
                let result = line
                    .map_err(|error| format!("failed to read recovery app-server: {error}"))
                    .and_then(|line| {
                        serde_json::from_str(&line)
                            .map_err(|error| format!("invalid recovery app-server JSON: {error}"))
                    });
                if sender.send(result).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            output,
            reader: Some(reader),
            pending: VecDeque::new(),
            next_id: 1,
            terminated: false,
        })
    }

    fn initialize(&mut self) -> Result<(), String> {
        self.request("initialize", json!({
            "clientInfo": {"name": "singularity-evaluation-recovery", "title": "Evaluation Recovery", "version": "0.1.0"}
        }))?;
        self.notify("initialized", json!({}))
    }

    fn thread_start(
        &mut self,
        workspace: &Path,
        model_selector: Option<&str>,
    ) -> Result<String, String> {
        let result = self.request(
            "thread/start",
            json!({
                "model": model_selector,
                "cwd": workspace,
                "sandboxMode": RECOVERY_SANDBOX_MODE,
                "approvalPolicy": "never"
            }),
        )?;
        result["thread"]["thread_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| "recovery thread/start omitted thread id".to_string())
    }

    fn turn_start(&mut self, thread_id: &str, prompt: &str) -> Result<String, String> {
        let result = self.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": prompt}]
            }),
        )?;
        result["turn"]["turn_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| "recovery turn/start omitted turn id".to_string())
    }

    fn turn_resume(&mut self, turn_id: &str) -> Result<(), String> {
        self.request("turn/resume", json!({"turnId": turn_id}))
            .map(|_| ())
    }

    fn turn_status(&mut self, turn_id: &str) -> Result<String, String> {
        let result = self.request("turn/status", json!({"turnId": turn_id}))?;
        result["turn"]["status"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "recovery turn/status omitted turn status".to_string())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.write(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write(json!({"jsonrpc": "2.0", "method": method, "id": id, "params": params}))?;
        loop {
            let message = self.recv_next()?;
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                if message.get("id").is_none() {
                    continue;
                }
                self.pending.push_back(message);
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(format!("recovery app-server error: {error}"));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| "recovery app-server response omitted result".to_string());
        }
    }

    fn recv_next(&mut self) -> Result<Value, String> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        self.output
            .recv()
            .map_err(|_| "recovery app-server closed stdout".to_string())?
    }

    fn write(&mut self, message: Value) -> Result<(), String> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| "recovery app-server stdin unavailable".to_string())?;
        serde_json::to_writer(&mut *stdin, &message)
            .map_err(|error| format!("failed to serialize recovery request: {error}"))?;
        stdin
            .write_all(b"\n")
            .and_then(|_| stdin.flush())
            .map_err(|error| format!("failed to write recovery request: {error}"))
    }

    fn shutdown(&mut self) {
        if self.stdin.is_some() {
            let _ = self.request("server/shutdown", json!({}));
        }
        self.reap(false);
    }

    fn kill(&mut self) {
        self.reap(true);
    }

    fn reap(&mut self, force: bool) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        self.stdin.take();
        if force || self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn resolve_recovery_paths(workspace: &Path, db_path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let workspace = fs::canonicalize(workspace)
        .map_err(|error| format!("failed to resolve recovery workspace: {error}"))?;
    let db_parent = db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let db_parent = fs::canonicalize(db_parent)
        .map_err(|error| format!("failed to resolve recovery database directory: {error}"))?;
    let db_name = db_path
        .file_name()
        .ok_or_else(|| "recovery database path has no file name".to_string())?;
    Ok((workspace, db_parent.join(db_name)))
}

impl Drop for RecoveryClient {
    fn drop(&mut self) {
        self.reap(true);
    }
}

fn app_server_bin() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(APP_SERVER_BIN_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("{APP_SERVER_BIN_ENV} does not point to a file"));
    }
    if let Ok(mut path) = std::env::current_exe() {
        path.pop();
        path.push(format!(
            "{APP_SERVER_BINARY}{}",
            std::env::consts::EXE_SUFFIX
        ));
        if path.is_file() {
            return Ok(path);
        }
    }
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target");
    path.push("debug");
    path.push(format!(
        "{APP_SERVER_BINARY}{}",
        std::env::consts::EXE_SUFFIX
    ));
    if path.is_file() {
        return Ok(path);
    }
    Err(format!(
        "{APP_SERVER_BINARY} not found; set {APP_SERVER_BIN_ENV} to an explicit binary"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span_event(
        event_id: &str,
        turn_id: &str,
        kind: TraceSpanKind,
        phase: TraceSpanPhase,
        span_id: &str,
    ) -> TraceEvent {
        let mut event = TraceEvent::for_turn(event_id, "thread", turn_id, "app_server", "span");
        event.span_kind = Some(kind);
        event.span_phase = Some(phase);
        event.span_id = Some(span_id.to_string());
        if phase == TraceSpanPhase::End {
            event.span_status = Some(TraceSpanStatus::Ok);
            event.duration_ms = Some(1);
        }
        if kind == TraceSpanKind::ProviderAttempt {
            event.span_projection = Some(singularity_protocol::TraceSpanProjection {
                provider_name: Some("provider".to_string()),
                model_name: Some("model".to_string()),
                protocol: Some(singularity_protocol::TraceProviderProtocol::OpenAiResponses),
                operation_phase: Some(
                    singularity_protocol::TraceProviderOperationPhase::Completion,
                ),
                attempt_index: Some(0),
                ..Default::default()
            });
        }
        event
    }

    #[test]
    fn recovery_marker_requires_an_open_provider_attempt() {
        let closed_start = span_event(
            "closed-start",
            "turn",
            TraceSpanKind::ProviderAttempt,
            TraceSpanPhase::Start,
            "closed",
        );
        let closed_end = span_event(
            "closed-end",
            "turn",
            TraceSpanKind::ProviderAttempt,
            TraceSpanPhase::End,
            "closed",
        );
        assert_eq!(
            open_provider_attempt_start(&[closed_start.clone(), closed_end], "turn"),
            None
        );
        let open = span_event(
            "open-start",
            "turn",
            TraceSpanKind::ProviderAttempt,
            TraceSpanPhase::Start,
            "open",
        );
        assert_eq!(
            open_provider_attempt_start(&[closed_start, open], "turn"),
            Some(1)
        );
    }

    #[test]
    fn unknown_execution_spans_include_orphan_end_events() {
        let orphan_end = span_event(
            "orphan-end",
            "turn",
            TraceSpanKind::ToolCall,
            TraceSpanPhase::End,
            "orphan",
        );
        assert!(has_unknown_tool_or_sandbox_span(&[orphan_end], "turn"));
    }

    #[test]
    fn unknown_execution_evidence_fails_closed_for_missing_binding_and_identity() {
        let mut sandbox_end = span_event(
            "sandbox-end",
            "turn",
            TraceSpanKind::SandboxExecution,
            TraceSpanPhase::End,
            "sandbox",
        );
        sandbox_end.span_status = Some(TraceSpanStatus::Ok);
        sandbox_end.duration_ms = Some(1);
        sandbox_end.span_projection = Some(singularity_protocol::TraceSpanProjection {
            sandbox: Some(singularity_protocol::TraceSandboxProjection {
                command_id_digest: Some("digest".to_string()),
                command_id_binding_valid: None,
                status: Some(singularity_protocol::TraceSandboxStatus::Ok),
                workspace_mutation: Some(singularity_protocol::TraceWorkspaceMutation::Unchanged),
                enforcement: Some(singularity_protocol::TraceSandboxEnforcement::Strict),
            }),
            ..Default::default()
        });
        assert!(has_unknown_tool_or_sandbox_span(&[sandbox_end], "turn"));

        let mut tool_start = span_event(
            "tool-start",
            "turn",
            TraceSpanKind::ToolCall,
            TraceSpanPhase::Start,
            "tool",
        );
        tool_start.span_projection = Some(singularity_protocol::TraceSpanProjection::default());
        assert!(has_unknown_tool_or_sandbox_span(&[tool_start], "turn"));
    }

    #[test]
    fn recovery_child_paths_are_absolute() {
        let (workspace, db_path) =
            resolve_recovery_paths(Path::new("."), Path::new("recovery.sqlite3"))
                .expect("current workspace paths resolve");
        assert!(workspace.is_absolute());
        assert!(db_path.is_absolute());
    }

    #[test]
    fn recovery_thread_uses_wire_sandbox_profile_name() {
        assert_eq!(RECOVERY_SANDBOX_MODE, "workspace-write");
    }
}
