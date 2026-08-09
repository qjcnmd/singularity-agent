//! Canonical typed tool-result occurrence codec.
//!
//! Tool results are execution facts. They remain ordered so transcript reconstruction, compaction,
//! approval pauses, and owner-loss handling all share one representation. This codec persists
//! execution facts only.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_tools::{
    ToolFailureKind, ToolResult, WorkspaceChangeSummary, WorkspaceObservation,
};

/// Controls whether an occurrence is shown in the model transcript or retained only for
/// checkpoint/replay accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultVisibility {
    Hidden,
    Visible,
    Compacted,
    Omitted,
}

/// One model-visible or checkpoint-only tool result occurrence.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultOccurrence {
    pub(super) result: ToolResult,
    visibility: ToolResultVisibility,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ToolResultOccurrenceWire {
    pub(super) result: ToolResult,
    #[serde(default)]
    pub(super) visibility: Option<ToolResultVisibility>,
    #[serde(default)]
    pub(super) result_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) context_token_count: Option<u32>,
    pub(super) audit_metadata: Option<Value>,
    #[serde(default)]
    pub(super) workspace_observation: Option<WorkspaceObservation>,
    #[serde(default)]
    pub(super) workspace_change_summary: Option<WorkspaceChangeSummary>,
}

impl Serialize for ToolResultOccurrence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ToolResultOccurrenceWire {
            result: self.result.clone(),
            visibility: Some(self.visibility),
            result_id: self.result.result_id.clone(),
            context_token_count: self.result.context_token_count(),
            audit_metadata: self.result.audit_metadata().cloned(),
            workspace_observation: self.result.workspace_observation().cloned(),
            workspace_change_summary: self.result.workspace_change_summary().cloned(),
        }
        .serialize(serializer)
    }
}

impl Eq for ToolResultOccurrence {}

impl<'de> Deserialize<'de> for ToolResultOccurrence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ToolResultOccurrenceWire::deserialize(deserializer)?;
        let visibility = wire.visibility.ok_or_else(|| {
            serde::de::Error::custom("tool result occurrence visibility is missing")
        })?;
        Self::from_wire(wire, visibility).map_err(serde::de::Error::custom)
    }
}

impl ToolResultOccurrence {
    pub(super) fn new(result: ToolResult, visibility: ToolResultVisibility) -> Self {
        Self { result, visibility }
    }

    /// Encode the canonical occurrence payload used by private trace/checkpoint envelopes.
    pub fn encode_trace_payload(&self) -> Result<Value, String> {
        serde_json::to_value(self)
            .map_err(|error| format!("tool result occurrence trace serialization failed: {error}"))
    }

    pub fn result(&self) -> &ToolResult {
        &self.result
    }

    pub(super) fn into_result(self) -> ToolResult {
        self.result
    }

    pub fn visibility(&self) -> ToolResultVisibility {
        self.visibility
    }

    pub(super) fn set_visibility(&mut self, visibility: ToolResultVisibility) {
        self.visibility = visibility;
    }

    pub(super) fn from_wire(
        wire: ToolResultOccurrenceWire,
        visibility: ToolResultVisibility,
    ) -> Result<Self, String> {
        Self::from_parts(
            wire.result,
            visibility,
            wire.result_id,
            wire.context_token_count,
            wire.audit_metadata,
            wire.workspace_observation,
            wire.workspace_change_summary,
        )
    }

    pub(super) fn from_parts(
        mut result: ToolResult,
        visibility: ToolResultVisibility,
        result_id: Option<String>,
        context_token_count: Option<u32>,
        audit_metadata: Option<Value>,
        workspace_observation: Option<WorkspaceObservation>,
        workspace_change_summary: Option<WorkspaceChangeSummary>,
    ) -> Result<Self, String> {
        result.result_id = result_id;
        if result.failure_kind == Some(ToolFailureKind::Approval) && result.ok {
            return Err("approval checkpoint hidden tool result binding is invalid".to_string());
        }
        let reconstructable = result.reconstruct_context_token_count();
        let lower_bound = result.context_token_count_lower_bound();
        let context_token_count = match context_token_count {
            Some(value) if reconstructable.is_some_and(|expected| expected != value) => {
                return Err("tool result context token accounting is inconsistent".to_string());
            }
            Some(value) if value >= lower_bound => Some(value),
            Some(_) => {
                return Err("tool result context token accounting is inconsistent".to_string());
            }
            None if visibility == ToolResultVisibility::Hidden
                && result.failure_kind == Some(ToolFailureKind::Approval) =>
            {
                None
            }
            None if let Some(reconstructed) = reconstructable => Some(reconstructed),
            None => return Err("tool result context token accounting is missing".to_string()),
        };
        if let Some(value) = context_token_count {
            result = result.with_context_token_count(value);
        }
        if let Some(audit_metadata) = audit_metadata {
            result = result.with_audit(audit_metadata);
        }
        if let Some(observation) = workspace_observation {
            result = result.with_workspace_observation(observation);
        }
        if let Some(summary) = workspace_change_summary {
            result = result.with_workspace_change_summary(summary);
        }
        let occurrence = Self { result, visibility };
        occurrence.validate()?;
        Ok(occurrence)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self.result.tool_call_id.trim().is_empty() {
            return Err("tool result occurrence tool call id is missing".to_string());
        }
        if self.visibility == ToolResultVisibility::Hidden
            && self.result.failure_kind != Some(ToolFailureKind::Approval)
        {
            return Err("hidden tool result occurrence is not an approval result".to_string());
        }
        if !(self.visibility == ToolResultVisibility::Hidden
            && self.result.failure_kind == Some(ToolFailureKind::Approval))
            && self.result.context_token_count().is_none()
        {
            return Err("tool result occurrence context token accounting is missing".to_string());
        }
        Ok(())
    }
}

/// Return bounded command result IDs that are useful to audit consumers.
pub fn terminal_command_scope_digests(
    tool_results: &[ToolResult],
    max_count: usize,
) -> Vec<String> {
    if max_count == 0 {
        return Vec::new();
    }
    tool_results
        .iter()
        .filter_map(successful_command_scope_digest)
        .take(max_count)
        .map(str::to_string)
        .collect()
}

/// Return a validated command scope digest carried by a successful unchanged observation.
pub fn successful_command_scope_digest(tool_result: &ToolResult) -> Option<&str> {
    (tool_result.tool_name == super::TOOL_COMMAND
        && tool_result.ok
        && tool_result
            .workspace_observation()
            .is_some_and(|observation| {
                observation.mutation() == singularity_tools::WorkspaceMutation::Unchanged
                    && observation.revision().is_some()
            }))
    .then_some(tool_result.result_id.as_deref())
    .flatten()
    .filter(|digest| super::is_sha256_fingerprint(digest))
}
