//! Tool-result occurrences and completion verification reducer.
//!
//! The reducer is the sole owner of workspace revision and terminal command evidence. Public
//! scope helpers only project the reducer's result; they do not maintain a second state machine.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_tools::{
    ToolFailureKind, ToolResult, WorkspaceChangeSummary, WorkspaceMutation, WorkspaceObservation,
    WorkspaceRevision,
};

use super::{
    AgentVerification, POST_MUTATION_VERIFICATION_REQUIRED, TOOL_COMMAND, TOOL_PATCH,
    TOOL_SELECTION_FAILURE_GROUP, TOOL_SELECTION_FAILURE_PREFIX, is_repairable_tool_result,
    is_sha256_fingerprint,
};

/// 由 tool 结果和 approval 检查点共享的完成门禁状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(super) struct CompletionTracker {
    workspace_mutated: bool,
    #[serde(default)]
    pub(super) workspace_revision: Option<WorkspaceRevision>,
    successful_command_count: u32,
    #[serde(default)]
    terminal_command_scope_digests: Vec<String>,
    #[serde(default)]
    terminal_command_revisions: Vec<WorkspaceRevision>,
    /// User input invalidates prior terminal evidence until a later unchanged command succeeds.
    #[serde(default)]
    verification_required_after_user_input: bool,
    unresolved_failures: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RepairFailureState {
    pub(super) signature: String,
    pub(super) consecutive_count: u32,
}

/// 一个 tool result occurrence 的模型投影状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolResultVisibility {
    Hidden,
    Visible,
    Compacted,
    Omitted,
}

/// 一个 tool result occurrence 的唯一运行时记录。
///
/// 结果本体以及其可见性共同维护 occurrence 顺序；token accounting、审计 metadata 和
/// workspace observation 随结果一起进入集中式 checkpoint 编解码，而不再依赖平行数组。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ToolResultOccurrence {
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

    pub(super) fn result(&self) -> &ToolResult {
        &self.result
    }

    pub(super) fn into_result(self) -> ToolResult {
        self.result
    }

    pub(super) fn visibility(&self) -> ToolResultVisibility {
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
            Some(context_token_count)
                if reconstructable.is_some_and(|expected| expected != context_token_count) =>
            {
                return Err(
                    "approval checkpoint tool result context token accounting is inconsistent"
                        .to_string(),
                );
            }
            Some(context_token_count) if context_token_count >= lower_bound => {
                Some(context_token_count)
            }
            Some(_) => {
                return Err(
                    "approval checkpoint tool result context token accounting is inconsistent"
                        .to_string(),
                );
            }
            None if visibility == ToolResultVisibility::Hidden
                && result.failure_kind == Some(ToolFailureKind::Approval) =>
            {
                None
            }
            None if let Some(reconstructed) = reconstructable => Some(reconstructed),
            None => {
                return Err(
                    "approval checkpoint tool result context token accounting is missing"
                        .to_string(),
                );
            }
        };
        if let Some(context_token_count) = context_token_count {
            result = result.with_context_token_count(context_token_count);
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

/// Project the authoritative reducer state for evaluation and completion consumers.
pub fn terminal_command_scope_digests(
    tool_results: &[ToolResult],
    max_count: usize,
) -> Vec<String> {
    if max_count == 0 {
        return Vec::new();
    }
    let mut tracker = CompletionTracker::default();
    for tool_result in tool_results {
        tracker.observe_with_window(tool_result, Some(max_count));
    }
    tracker.terminal_command_scope_digests
}

/// Return the validated command scope digest carried by one successful unchanged observation.
pub fn successful_command_scope_digest(tool_result: &ToolResult) -> Option<&str> {
    (tool_result.tool_name == TOOL_COMMAND
        && tool_result.ok
        && tool_result
            .workspace_observation()
            .is_some_and(|observation| {
                observation.mutation() == WorkspaceMutation::Unchanged
                    && observation.revision().is_some()
            }))
    .then_some(tool_result.result_id.as_deref())
    .flatten()
    .filter(|digest| is_sha256_fingerprint(digest))
}

impl CompletionTracker {
    pub(super) fn observe(&mut self, tool_result: &ToolResult) {
        self.observe_with_window(tool_result, None);
    }

    fn observe_with_window(&mut self, tool_result: &ToolResult, terminal_window: Option<usize>) {
        let failure_group = match tool_result.failure_kind.as_ref() {
            Some(ToolFailureKind::Visibility) => Some(TOOL_SELECTION_FAILURE_GROUP),
            _ => match tool_result.tool_name.as_str() {
                TOOL_PATCH => Some("workspace_mutation"),
                TOOL_COMMAND => Some("verification"),
                // Read-only failures remain in the model-visible transcript. They do not define
                // completion state because the model may obtain equivalent evidence another way.
                _ => None,
            },
        };

        let observed_mutation = tool_result
            .workspace_observation()
            .map(|observation| self.observe_workspace_observation(observation));
        if tool_result.tool_name == TOOL_PATCH
            && tool_result.ok
            && !matches!(
                observed_mutation,
                Some(Some((WorkspaceMutation::Changed, _)))
            )
        {
            self.mark_workspace_revision_invalid("mutation_observation_missing");
            self.workspace_mutated = true;
            self.clear_terminal_command_observations();
        }
        if tool_result.ok {
            self.unresolved_failures.retain(|failure| {
                failure_group.is_none_or(|group| !failure.starts_with(group))
                    && !failure.starts_with(TOOL_SELECTION_FAILURE_PREFIX)
            });
            if tool_result.tool_name == TOOL_PATCH {
                self.workspace_mutated = true;
                self.clear_terminal_command_observations();
            } else if tool_result.tool_name == TOOL_COMMAND {
                self.successful_command_count = self.successful_command_count.saturating_add(1);
                if let Some(Some((WorkspaceMutation::Unchanged, revision))) = observed_mutation {
                    let window_len =
                        terminal_window.unwrap_or_else(|| self.terminal_command_window_len());
                    self.record_terminal_command_observation(
                        successful_command_scope_digest(tool_result),
                        revision,
                        window_len,
                    );
                    self.clear_superseded_workspace_input_failures();
                } else if observed_mutation.is_none() {
                    self.mark_workspace_revision_invalid("verification_observation_missing");
                }
            }
        } else if is_repairable_tool_result(tool_result)
            && let Some(failure_group) = failure_group
        {
            let error_code = tool_result
                .error_code
                .as_deref()
                .unwrap_or("tool_execution_failed");
            self.unresolved_failures
                .insert(format!("{failure_group}:{error_code}"));
        }
    }

    pub(super) fn observe_workspace_observation(
        &mut self,
        observation: &WorkspaceObservation,
    ) -> Option<(WorkspaceMutation, WorkspaceRevision)> {
        let Some(revision) = observation.revision() else {
            self.mark_workspace_revision_invalid("revision_missing");
            return None;
        };
        let mutation = observation.mutation();
        if mutation == WorkspaceMutation::Unknown {
            self.mark_workspace_revision_invalid("change_unknown");
            return None;
        }
        let valid_revision = match (self.workspace_revision, mutation) {
            (None, WorkspaceMutation::Unchanged) => revision == WorkspaceRevision::initial(),
            (None, WorkspaceMutation::Changed) => {
                WorkspaceRevision::initial().next() == Some(revision)
            }
            (None, WorkspaceMutation::Unknown) => false,
            (Some(current), WorkspaceMutation::Unchanged) => current == revision,
            (Some(current), WorkspaceMutation::Changed) => current.next() == Some(revision),
            (Some(_), WorkspaceMutation::Unknown) => false,
        };
        if !valid_revision {
            self.mark_workspace_revision_invalid("revision_mismatch");
            return None;
        }
        self.workspace_revision = Some(revision);
        if mutation == WorkspaceMutation::Changed {
            self.workspace_mutated = true;
            self.clear_terminal_command_observations();
        }
        Some((mutation, revision))
    }

    pub(super) fn mark_workspace_revision_invalid(&mut self, reason: &str) {
        self.clear_terminal_command_observations();
        self.unresolved_failures
            .insert(format!("workspace_revision:{reason}"));
    }

    pub(super) fn clear_terminal_command_observations(&mut self) {
        self.terminal_command_scope_digests.clear();
        self.terminal_command_revisions.clear();
    }

    /// Invalidate terminal verification after a steer/follow-up input.
    pub(super) fn invalidate_after_user_input(&mut self) {
        self.clear_terminal_command_observations();
        self.verification_required_after_user_input = true;
    }

    pub(super) fn requires_post_input_verification(&self) -> bool {
        self.verification_required_after_user_input
    }

    /// Clear only side-effect-free mutation input failures superseded by complete verification.
    fn clear_superseded_workspace_input_failures(&mut self) {
        if !self.workspace_mutated || !self.verification_satisfied() {
            return;
        }
        self.unresolved_failures.retain(|failure| {
            !matches!(
                failure.as_str(),
                "workspace_mutation:invalid_tool_arguments"
                    | "workspace_mutation:invalid_tool_input"
            )
        });
    }

    pub(super) fn record_terminal_command_observation(
        &mut self,
        scope_digest: Option<&str>,
        revision: WorkspaceRevision,
        max_count: usize,
    ) {
        let Some(scope_digest) = scope_digest else {
            self.clear_terminal_command_observations();
            return;
        };
        self.terminal_command_scope_digests
            .push(scope_digest.to_string());
        self.terminal_command_revisions.push(revision);
        self.verification_required_after_user_input = false;
        let excess = self
            .terminal_command_scope_digests
            .len()
            .saturating_sub(max_count);
        if excess > 0 {
            self.terminal_command_scope_digests.drain(..excess);
            self.terminal_command_revisions.drain(..excess);
        }
    }

    pub(super) fn allows_final(&self) -> bool {
        self.unresolved_failures.is_empty() && self.verification_satisfied()
    }

    pub(super) fn has_unresolved_failures(&self) -> bool {
        !self.unresolved_failures.is_empty()
    }

    pub(super) fn rejection_reason(&self) -> String {
        if !self.unresolved_failures.is_empty() {
            return format!(
                "completion gate rejected final answer: unresolved failures: {}",
                self.unresolved_failures
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        POST_MUTATION_VERIFICATION_REQUIRED.to_string()
    }

    pub(super) fn feedback(&self) -> String {
        if !self.unresolved_failures.is_empty() {
            return format!(
                "Do not finalize yet. Resolve these failures and rerun the relevant verification. Current failures: {}. The terminal verification command must succeed. After it finishes, leave no unaddressed semantic workspace changes: do not redirect verification output into new workspace logs or reports, and do not modify source files. Trusted toolchain cache updates may be treated as semantic Unchanged by the workspace observation and do not require cleanup. If the command causes another semantic change, clean up or stabilize that change, then rerun the relevant verification without creating further semantic changes.",
                self.unresolved_failures
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        "Do not finalize yet. Run a relevant verification command after the latest workspace mutation and inspect its result. The terminal verification command must succeed. After it finishes, leave no unaddressed semantic workspace changes: do not redirect verification output into new workspace logs or reports, and do not modify source files. Trusted toolchain cache updates may be treated as semantic Unchanged by the workspace observation and do not require cleanup. If the command causes another semantic change, clean up or stabilize that change, then rerun the relevant verification without creating further semantic changes. Only then provide the final answer."
            .to_string()
    }

    pub(super) fn summary(&self) -> AgentVerification {
        let required = self.workspace_mutated;
        AgentVerification {
            required,
            passed: required && self.allows_final(),
            successful_command_count: self.successful_command_count,
            required_command_count: u32::from(self.workspace_mutated),
            satisfied_command_count: u32::from(
                self.workspace_mutated && self.verification_satisfied(),
            ),
            unresolved_failures: self.unresolved_failures.iter().cloned().collect(),
        }
    }

    pub(super) fn verification_satisfied(&self) -> bool {
        !self.workspace_mutated
            || (self.terminal_command_scope_digests.len() == 1
                && self.terminal_command_revisions.len() == 1
                && self
                    .terminal_command_revisions
                    .iter()
                    .all(|revision| Some(*revision) == self.workspace_revision))
    }

    pub(super) fn terminal_command_window_len(&self) -> usize {
        1
    }

    pub(super) fn is_consistent(&self) -> bool {
        if self.workspace_mutated && self.workspace_revision.is_none() {
            return false;
        }
        if self.terminal_command_scope_digests.len() != self.terminal_command_revisions.len() {
            return false;
        }
        if self
            .terminal_command_scope_digests
            .iter()
            .any(|digest| !is_sha256_fingerprint(digest))
        {
            return false;
        }
        if self.verification_required_after_user_input
            && (!self.terminal_command_scope_digests.is_empty()
                || !self.terminal_command_revisions.is_empty())
        {
            return false;
        }
        self.terminal_command_revisions
            .iter()
            .all(|revision| Some(*revision) == self.workspace_revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERMINAL_VERIFICATION_FEEDBACK: &str = "The terminal verification command must succeed. After it finishes, leave no unaddressed semantic workspace changes: do not redirect verification output into new workspace logs or reports, and do not modify source files. Trusted toolchain cache updates may be treated as semantic Unchanged by the workspace observation and do not require cleanup. If the command causes another semantic change, clean up or stabilize that change, then rerun the relevant verification without creating further semantic changes.";

    #[test]
    fn feedback_with_unresolved_failures_requires_unchanged_terminal_verification() {
        let mut tracker = CompletionTracker {
            workspace_mutated: true,
            ..CompletionTracker::default()
        };
        tracker
            .unresolved_failures
            .insert("verification:command_exit_nonzero".to_string());

        assert_eq!(
            tracker.feedback(),
            format!(
                "Do not finalize yet. Resolve these failures and rerun the relevant verification. Current failures: verification:command_exit_nonzero. {TERMINAL_VERIFICATION_FEEDBACK}"
            )
        );
    }

    #[test]
    fn feedback_after_mutation_requires_unchanged_terminal_verification() {
        let tracker = CompletionTracker {
            workspace_mutated: true,
            ..CompletionTracker::default()
        };

        assert_eq!(
            tracker.feedback(),
            format!(
                "Do not finalize yet. Run a relevant verification command after the latest workspace mutation and inspect its result. {TERMINAL_VERIFICATION_FEEDBACK} Only then provide the final answer."
            )
        );
    }
}
