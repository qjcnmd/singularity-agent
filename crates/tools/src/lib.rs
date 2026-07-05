#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    ReadOnly,
    Write,
    Shell,
    Git,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    pub name: String,
    pub version: String,
    pub description: String,
    pub input_schema: Value,
    pub permission_level: PermissionLevel,
    pub risk_tags: Vec<String>,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            version: "0.0.1".to_string(),
            description: description.into(),
            input_schema,
            permission_level: PermissionLevel::ReadOnly,
            risk_tags: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolSpec>,
}

impl ToolRegistry {
    pub fn register(&mut self, spec: ToolSpec) -> Result<(), String> {
        if self.tools.contains_key(&spec.name) {
            return Err(format!("tool already registered: {}", spec.name));
        }
        self.tools.insert(spec.name.clone(), spec);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallEnvelope {
    pub protocol_version: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub raw_arguments: String,
}

impl ToolCallEnvelope {
    pub fn new(
        run_id: impl Into<String>,
        session_id: impl Into<String>,
        task_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        raw_arguments: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: "1.0".to_string(),
            run_id: run_id.into(),
            session_id: session_id.into(),
            task_id: task_id.into(),
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            raw_arguments: raw_arguments.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub ok: bool,
    pub content: Value,
    pub error_code: Option<String>,
    pub truncated: bool,
    pub metadata: Value,
}

impl ToolResult {
    pub fn success(content: Value) -> Self {
        Self {
            ok: true,
            content,
            error_code: None,
            truncated: false,
            metadata: json!({}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolObservationVisibility {
    Summary,
    ReferenceOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolObservation {
    pub tool_call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub status: String,
    pub visibility: ToolObservationVisibility,
    pub content_preview: String,
    pub content_digest: String,
    pub result_ref: Option<String>,
    pub error_code: Option<String>,
    pub reference_ids: Vec<String>,
    pub observation_id: Option<String>,
    pub truncated: bool,
    pub redacted: bool,
    #[serde(skip)]
    policy_decision_id: Option<String>,
    #[serde(skip)]
    approval_grant_id: Option<String>,
    #[serde(skip)]
    internal_metadata: Option<Value>,
}

impl ToolObservation {
    pub fn summary(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        ok: bool,
        content_preview: impl Into<String>,
        content_digest: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            ok,
            status: if ok { "ok" } else { "error" }.to_string(),
            visibility: ToolObservationVisibility::Summary,
            content_preview: content_preview.into(),
            content_digest: content_digest.into(),
            result_ref: None,
            error_code: None,
            reference_ids: Vec::new(),
            observation_id: None,
            truncated: false,
            redacted: true,
            policy_decision_id: None,
            approval_grant_id: None,
            internal_metadata: None,
        }
    }

    pub fn with_internal_metadata(
        mut self,
        policy_decision_id: impl Into<String>,
        approval_grant_id: impl Into<String>,
        metadata: Value,
    ) -> Self {
        self.policy_decision_id = Some(policy_decision_id.into());
        self.approval_grant_id = Some(approval_grant_id.into());
        self.internal_metadata = Some(metadata);
        self
    }

    pub fn from_result(
        envelope: &ToolCallEnvelope,
        result: &ToolResult,
        visibility: ToolObservationVisibility,
    ) -> Self {
        Self {
            visibility,
            error_code: result.error_code.clone(),
            truncated: result.truncated,
            ..Self::summary(
                envelope.tool_call_id.clone(),
                envelope.tool_name.clone(),
                result.ok,
                result.content.to_string(),
                "",
            )
        }
    }

    pub fn to_model_payload(&self) -> Value {
        let mut payload = json!({
            "ok": self.ok,
            "tool_name": self.tool_name,
            "tool_call_id": self.tool_call_id,
            "status": self.status,
            "content_digest": self.content_digest,
            "result_ref": self.result_ref,
            "error_code": self.error_code,
            "reference_ids": self.reference_ids,
            "observation_id": self.observation_id,
            "truncated": self.truncated,
            "redacted": self.redacted,
        });
        if self.visibility != ToolObservationVisibility::ReferenceOnly {
            payload["content"] = json!(self.content_preview);
            payload["content_preview"] = json!(self.content_preview);
        }
        payload
    }
}
