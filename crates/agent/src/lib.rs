#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const SIDECAR_METHOD_RUN: &str = "agent/run";
const SIDECAR_METHOD_CANCEL: &str = "agent/cancel";
const SIDECAR_METHOD_STATUS: &str = "agent/status";
const SIDECAR_METHOD_HEALTH: &str = "agent/health";
const SIDECAR_COMPONENT: &str = "python_sidecar";
const DEFAULT_PYTHON_BIN: &str = "python";
const DEFAULT_SIDECAR_MODULE: &str = "singularity.agent_host.sidecar";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentHostStatus {
    NotMigrated,
    Running,
    Completed,
    Blocked,
    Cancelled,
    Failed,
}

impl AgentHostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotMigrated => "not_migrated",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl From<&str> for AgentHostStatus {
    fn from(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "blocked" => Self::Blocked,
            "cancelled" | "canceled" => Self::Cancelled,
            "failed" | "max_turns_exceeded" => Self::Failed,
            "not_migrated" => Self::NotMigrated,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AgentLoopStatusBridge {
    pub status: AgentHostStatus,
    pub completed: bool,
    pub final_answer: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub events: Vec<SidecarRunEvent>,
    pub trace_path: Option<String>,
    pub error: Option<String>,
}

impl AgentLoopStatusBridge {
    pub fn not_migrated() -> Self {
        Self {
            status: AgentHostStatus::NotMigrated,
            completed: false,
            final_answer: None,
            run_id: None,
            session_id: None,
            task_id: None,
            events: Vec::new(),
            trace_path: None,
            error: None,
        }
    }

    pub fn from_sidecar(result: PythonSidecarRunResult) -> Self {
        let status = AgentHostStatus::from(result.status.as_str());
        Self {
            completed: status == AgentHostStatus::Completed,
            final_answer: result.final_answer,
            run_id: Some(result.run_id),
            session_id: Some(result.session_id),
            task_id: Some(result.task_id),
            events: result.events,
            trace_path: result.trace_path,
            error: None,
            status,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: AgentHostStatus::Failed,
            completed: false,
            final_answer: None,
            run_id: None,
            session_id: None,
            task_id: None,
            events: Vec::new(),
            trace_path: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonSidecarConfig {
    pub python_bin: String,
    pub module: String,
    pub project_root: PathBuf,
    pub python_path: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl PythonSidecarConfig {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            python_bin: DEFAULT_PYTHON_BIN.to_string(),
            module: DEFAULT_SIDECAR_MODULE.to_string(),
            project_root: project_root.into(),
            python_path: None,
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PythonSidecarRunResult {
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer: Option<String>,
    #[serde(default)]
    pub events: Vec<SidecarRunEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SidecarRunEvent {
    pub event_id: String,
    pub event_type: String,
    pub summary: String,
    pub component: String,
    pub severity: String,
    pub sequence: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlannerStateBoundary {
    pub task_id: String,
    pub current_phase: String,
    pub status: String,
    pub current_plan: Vec<Value>,
    pub completion_criteria: Value,
    pub open_actions: Vec<Value>,
    pub blocked_actions: Vec<Value>,
    pub risk_escalations: Vec<Value>,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug)]
pub struct PythonSidecarClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl PythonSidecarClient {
    pub fn spawn(config: &PythonSidecarConfig) -> Result<Self, String> {
        let mut command = Command::new(&config.python_bin);
        command
            .args(["-m", &config.module])
            .env("SINGULARITY_SIDECAR_PROJECT_ROOT", &config.project_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(python_path) = &config.python_path {
            command.env("PYTHONPATH", python_path);
        }
        for (name, value) in &config.env {
            command.env(name, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start Python sidecar: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Python sidecar stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Python sidecar stdout unavailable".to_string())?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    pub fn run_agent(&mut self, goal: &str) -> Result<PythonSidecarRunResult, String> {
        let value = self.request(SIDECAR_METHOD_RUN, json!({"goal": goal}))?;
        serde_json::from_value(value)
            .map_err(|error| format!("invalid sidecar run result: {error}"))
    }

    pub fn cancel(&mut self, run_id: &str) -> Result<Value, String> {
        self.request(SIDECAR_METHOD_CANCEL, json!({"runId": run_id}))
    }

    pub fn status(&mut self, run_id: &str) -> Result<Value, String> {
        self.request(SIDECAR_METHOD_STATUS, json!({"runId": run_id}))
    }

    pub fn health(&mut self) -> Result<Value, String> {
        self.request(SIDECAR_METHOD_HEALTH, json!({}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_request_id();
        let message = json!({"id": id, "method": method, "params": params});
        writeln!(self.stdin, "{message}")
            .map_err(|error| format!("failed to write Python sidecar request: {error}"))?;
        self.stdin
            .flush()
            .map_err(|error| format!("failed to flush Python sidecar request: {error}"))?;
        let response = self.read_response()?;
        if response.get("id").and_then(Value::as_i64) != Some(id) {
            return Err("Python sidecar returned mismatched response id".to_string());
        }
        if let Some(error) = response.get("error") {
            let message = error["message"].as_str().unwrap_or("Python sidecar error");
            return Err(message.to_string());
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "Python sidecar response missing result".to_string())
    }

    fn read_response(&mut self) -> Result<Value, String> {
        let mut line = String::new();
        let bytes = self
            .stdout
            .read_line(&mut line)
            .map_err(|error| format!("failed to read Python sidecar response: {error}"))?;
        if bytes == 0 {
            return Err("Python sidecar closed stdout".to_string());
        }
        serde_json::from_str(line.trim())
            .map_err(|error| format!("invalid Python sidecar JSON: {error}"))
    }

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

impl Drop for PythonSidecarClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn sidecar_trace_summary(bridge: &AgentLoopStatusBridge) -> Value {
    json!({
        "component": SIDECAR_COMPONENT,
        "status": bridge.status.as_str(),
        "run_id": bridge.run_id,
        "session_id": bridge.session_id,
        "task_id": bridge.task_id,
        "trace_path": bridge.trace_path,
    })
}
