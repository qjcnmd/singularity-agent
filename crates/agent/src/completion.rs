//! Tool-result occurrences and completion verification reducer.
//!
//! The reducer is the sole owner of workspace revision and terminal command evidence. Public
//! scope helpers only project the reducer's result; they do not maintain a second state machine.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use singularity_tools::{
    ToolFailureKind, ToolResult, WorkspaceChangeSummary, WorkspaceMutation, WorkspaceObservation,
    WorkspaceRevision,
};

use super::{
    AgentVerification, AgentVerificationRequirement, EXACT_VERIFICATION_REQUIRED,
    MAX_VERIFICATION_REQUIREMENTS, POST_MUTATION_VERIFICATION_REQUIRED, TOOL_COMMAND, TOOL_EDIT,
    TOOL_PATCH, TOOL_SELECTION_FAILURE_GROUP, TOOL_SELECTION_FAILURE_PREFIX,
    is_repairable_tool_result, is_sha256_fingerprint,
};

/// 由 tool 结果和 approval 检查点共享的完成门禁状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub(super) struct CompletionTracker {
    workspace_mutated: bool,
    #[serde(default)]
    pub(super) workspace_revision: Option<WorkspaceRevision>,
    successful_command_count: u32,
    #[serde(default)]
    required_command_counts: BTreeMap<String, u32>,
    #[serde(default)]
    terminal_command_scope_digests: Vec<String>,
    #[serde(default)]
    terminal_command_revisions: Vec<WorkspaceRevision>,
    unresolved_failures: BTreeSet<String>,
}

impl CompletionTracker {
    pub(super) fn from_requirements(
        requirements: &[AgentVerificationRequirement],
    ) -> Result<Self, String> {
        if requirements.len() > MAX_VERIFICATION_REQUIREMENTS {
            return Err(format!(
                "verification requirements must not contain more than {MAX_VERIFICATION_REQUIREMENTS} entries"
            ));
        }
        let mut required_command_counts = BTreeMap::new();
        for requirement in requirements {
            if !is_sha256_fingerprint(&requirement.command_scope_digest) {
                return Err("verification requirement command digest is invalid".to_string());
            }
            if requirement.required_success_count == 0 {
                return Err(
                    "verification requirement success count must be greater than zero".to_string(),
                );
            }
            let count = required_command_counts
                .entry(requirement.command_scope_digest.clone())
                .or_insert(0u32);
            *count = count
                .checked_add(requirement.required_success_count)
                .ok_or_else(|| {
                    "verification requirement success count exceeds the supported range".to_string()
                })?;
        }
        Ok(Self {
            required_command_counts,
            ..Self::default()
        })
    }

    /// Activate exact command requirements once a real workspace mutation creates a verification
    /// boundary. Read-only turns keep the legacy requirement state unchanged until this point.
    pub(super) fn activate_requirements(
        &mut self,
        requirements: &[AgentVerificationRequirement],
    ) -> Result<(), String> {
        let next = Self::from_requirements(requirements)?;
        if self.required_command_counts.is_empty() {
            self.required_command_counts = next.required_command_counts;
            return Ok(());
        }
        if self.required_command_counts == next.required_command_counts {
            Ok(())
        } else {
            Err("verification requirements changed after execution began".to_string())
        }
    }

    /// Replace exact requirements after a new mutation invalidates the prior evidence window.
    pub(super) fn replace_requirements(
        &mut self,
        requirements: &[AgentVerificationRequirement],
    ) -> Result<(), String> {
        let next = Self::from_requirements(requirements)?;
        self.required_command_counts = next.required_command_counts;
        self.clear_terminal_command_observations();
        Ok(())
    }

    pub(super) fn workspace_mutated(&self) -> bool {
        self.workspace_mutated
    }

    pub(super) fn observe(&mut self, tool_result: &ToolResult) {
        self.observe_with_window(tool_result, None);
    }

    fn observe_with_window(&mut self, tool_result: &ToolResult, terminal_window: Option<usize>) {
        let failure_group = match tool_result.failure_kind.as_ref() {
            Some(ToolFailureKind::Visibility) => TOOL_SELECTION_FAILURE_GROUP,
            _ => match tool_result.tool_name.as_str() {
                TOOL_EDIT | TOOL_PATCH => "workspace_mutation",
                TOOL_COMMAND => "verification",
                tool_name => tool_name,
            },
        };

        let observed_mutation = tool_result
            .workspace_observation()
            .map(|observation| self.observe_workspace_observation(observation));
        if matches!(tool_result.tool_name.as_str(), TOOL_EDIT | TOOL_PATCH)
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
                !failure.starts_with(failure_group)
                    && !failure.starts_with(TOOL_SELECTION_FAILURE_PREFIX)
            });
            if matches!(tool_result.tool_name.as_str(), TOOL_EDIT | TOOL_PATCH) {
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
                } else if observed_mutation.is_none() {
                    self.mark_workspace_revision_invalid("verification_observation_missing");
                }
            }
        } else if is_repairable_tool_result(tool_result) {
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

    pub(super) fn clear_user_input_invalidation(&mut self) {
        self.unresolved_failures
            .remove("workspace_revision:user_input_revision");
    }

    pub(super) fn clear_terminal_command_observations(&mut self) {
        self.terminal_command_scope_digests.clear();
        self.terminal_command_revisions.clear();
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
        if !self.required_command_counts.is_empty() {
            return EXACT_VERIFICATION_REQUIRED.to_string();
        }
        POST_MUTATION_VERIFICATION_REQUIRED.to_string()
    }

    pub(super) fn feedback(&self) -> String {
        if !self.unresolved_failures.is_empty() {
            return format!(
                "Do not finalize yet. Resolve these failures and rerun the relevant verification: {}.",
                self.unresolved_failures
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !self.required_command_counts.is_empty() {
            return format!(
                "Do not finalize yet. Run every exact verification command required by the task as the final successful command sequence after the latest workspace mutation. {} of {} required successful command results are currently satisfied.",
                self.satisfied_command_count(),
                self.required_command_count()
            );
        }
        "Do not finalize yet. Run a relevant verification command after the latest workspace mutation, inspect its result, and only then provide the final answer."
            .to_string()
    }

    pub(super) fn summary(&self) -> AgentVerification {
        let required_command_count = self.required_command_count();
        let satisfied_command_count = self.satisfied_command_count();
        let required = self.workspace_mutated || required_command_count > 0;
        AgentVerification {
            required,
            passed: required && self.allows_final(),
            successful_command_count: self.successful_command_count,
            required_command_count: if required_command_count > 0 {
                required_command_count
            } else {
                u32::from(self.workspace_mutated)
            },
            satisfied_command_count: if required_command_count > 0 {
                satisfied_command_count
            } else {
                u32::from(self.workspace_mutated && !self.terminal_command_scope_digests.is_empty())
            },
            unresolved_failures: self.unresolved_failures.iter().cloned().collect(),
            final_review_verdict: None,
        }
    }

    pub(super) fn verification_satisfied(&self) -> bool {
        if self.required_command_counts.is_empty() {
            return !self.workspace_mutated
                || (!self.terminal_command_scope_digests.is_empty()
                    && self.terminal_command_scope_digests.len()
                        == self.terminal_command_revisions.len()
                    && self
                        .terminal_command_revisions
                        .iter()
                        .all(|revision| Some(*revision) == self.workspace_revision));
        }
        usize::try_from(self.required_command_count()).is_ok_and(|required_count| {
            self.terminal_command_scope_digests.len() == required_count
                && self.terminal_command_revisions.len() == required_count
                && self
                    .terminal_command_revisions
                    .iter()
                    .all(|revision| Some(*revision) == self.workspace_revision)
                && self.terminal_command_counts() == self.required_command_counts
        })
    }

    pub(super) fn required_command_count(&self) -> u32 {
        self.required_command_counts
            .values()
            .copied()
            .fold(0u32, u32::saturating_add)
    }

    pub(super) fn terminal_command_scope_digests(&self) -> Vec<String> {
        self.terminal_command_scope_digests.clone()
    }

    pub(super) fn satisfied_command_count(&self) -> u32 {
        let terminal_counts = self.terminal_command_counts();
        self.required_command_counts
            .iter()
            .map(|(digest, required)| {
                terminal_counts
                    .get(digest)
                    .copied()
                    .unwrap_or(0)
                    .min(*required)
            })
            .fold(0u32, u32::saturating_add)
    }

    pub(super) fn terminal_command_window_len(&self) -> usize {
        usize::try_from(self.required_command_count().max(1)).unwrap_or(usize::MAX)
    }

    pub(super) fn terminal_command_counts(&self) -> BTreeMap<String, u32> {
        let mut counts = BTreeMap::new();
        for digest in &self.terminal_command_scope_digests {
            let count = counts.entry(digest.clone()).or_insert(0u32);
            *count = count.saturating_add(1);
        }
        counts
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
        self.terminal_command_revisions
            .iter()
            .all(|revision| Some(*revision) == self.workspace_revision)
    }
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
