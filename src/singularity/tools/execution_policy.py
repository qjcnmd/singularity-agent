from __future__ import annotations

from pathlib import Path
from typing import Any, Protocol

from singularity.observability.redaction import TraceRedactor
from singularity.policy import (
    ApprovalGate,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyComponent,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
)
from singularity.policy.audit import redact_resource_identifier
from singularity.policy.config import PolicyConfig
from singularity.policy.exceptions import (
    ApprovalDenied,
    ApprovalRequired,
    PolicyAskUserRequired,
    PolicyDenied,
    PolicyEscalationRequired,
    SandboxRequired,
)
from singularity.tools.execution_pipeline import ToolExecutionPipelineState
from singularity.tools.execution_resources import redacted_resource_details, resources_for
from singularity.tools.models import ToolExecutionBackendKind, ToolResult, ToolSpec


class ToolPolicyEngineProtocol(Protocol):
    config: PolicyConfig

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        ...


class ToolExecutionPolicyGate:
    def __init__(
        self,
        *,
        policy_engine: ToolPolicyEngineProtocol,
        approval_gate: ApprovalGate | Any | None,
        workspace_root: Path,
        trace: Any | None,
        planner: Any | None,
        redactor: TraceRedactor,
        argument_summary: Any,
    ) -> None:
        self.policy_engine = policy_engine
        self.approval_gate = approval_gate
        self.workspace_root = workspace_root
        self.trace = trace
        self.planner = planner
        self.redactor = redactor
        self.argument_summary = argument_summary

    def enforce(self, state: ToolExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.validated_args is not None
        policy_result, approval_grant_id, policy_decision_id = self._enforce_policy(
            tool_name=state.tool_name,
            spec=state.spec,
            validated_args=state.validated_args,
            tool_call_id=state.tool_call_id,
        )
        state.approval_grant_id = approval_grant_id
        state.policy_decision_id = policy_decision_id
        if (
            policy_result is not None
            and _delegates_policy_decision(state.spec, policy_result)
        ):
            return None
        if policy_result is not None:
            state.remember_replay = True
            return policy_result
        return None

    def _enforce_policy(
        self,
        *,
        tool_name: str,
        spec: ToolSpec,
        validated_args: dict[str, Any],
        tool_call_id: str | None,
    ) -> tuple[ToolResult | None, str | None, str | None]:
        request = self._policy_request(
            tool_name=tool_name,
            spec=spec,
            validated_args=validated_args,
            tool_call_id=tool_call_id,
        )
        decision = self.policy_engine.enforce(request)
        if decision.outcome == DecisionOutcome.ALLOW:
            return None, decision.approval_grant_id, decision.decision_id
        # CommandExecutor and WorkspaceMutationManager are the authoritative
        # execution boundaries for delegated tools. They must consume review
        # grants exactly once; ToolExecutor only enforces hard denials here.
        if (
            decision.outcome != DecisionOutcome.DENY
            and (spec.uses_command_executor or spec.uses_mutation_manager)
        ):
            return None, None, decision.decision_id
        if decision.outcome == DecisionOutcome.REQUIRE_REVIEW and self.approval_gate is not None:
            grant_store_trusted = True
            if hasattr(self.approval_gate, "is_grant_store_trusted"):
                grant_store_trusted = self.approval_gate.is_grant_store_trusted(self.workspace_root)
            if hasattr(self.approval_gate, "consume_matching_grant") and grant_store_trusted:
                existing_grant = self.approval_gate.consume_matching_grant(request)
                if existing_grant is not None:
                    allowed = _decision_allowed_by_grant(decision, existing_grant.grant_id)
                    return None, existing_grant.grant_id, allowed.decision_id
            try:
                grant = self.approval_gate.resolve(request, decision)
            except (
                ApprovalDenied,
                ApprovalRequired,
                PolicyAskUserRequired,
                PolicyDenied,
                PolicyEscalationRequired,
                SandboxRequired,
            ):
                self._record_policy_observation(request, decision)
                return self._policy_failure(request, decision), None, decision.decision_id
            if grant is None:
                self._record_policy_observation(request, decision)
                return self._policy_failure(request, decision), None, decision.decision_id
            if hasattr(self.approval_gate, "register_grant"):
                self.approval_gate.register_grant(grant)
            consumed_grant = (
                self.approval_gate.consume_grant(grant)
                if hasattr(self.approval_gate, "consume_grant")
                else grant
            )
            if consumed_grant is None:
                self._record_policy_observation(request, decision)
                return self._policy_failure(request, decision), None, decision.decision_id
            allowed = _decision_allowed_by_grant(decision, consumed_grant.grant_id)
            return None, consumed_grant.grant_id, allowed.decision_id
        self._record_policy_observation(request, decision)
        return self._policy_failure(request, decision), None, decision.decision_id

    def _policy_request(
        self,
        *,
        tool_name: str,
        spec: ToolSpec,
        validated_args: dict[str, Any],
        tool_call_id: str | None,
    ) -> PolicyRequest:
        resources = resources_for(spec, validated_args, self.workspace_root)
        resource = resources[0] if resources else ResourceRef("workspace", tool_name, workspace_relative=True)
        resource_details = [item.to_dict() for item in resources] or [resource.to_dict()]
        related_resources = resource_details[1:]
        if related_resources and not resource.metadata.get("related_resources"):
            resource = ResourceRef(
                resource.resource_type,
                resource.identifier,
                normalized_identifier=resource.normalized_identifier,
                workspace_relative=resource.workspace_relative,
                sensitive=resource.sensitive,
                metadata={**resource.metadata, "related_resources": related_resources},
            )
        operation = spec.operation or OperationKind.READ_FILE
        capability = spec.capabilities[0] if spec.capabilities else Capability.READ_WORKSPACE
        return PolicyRequest(
            session_id=getattr(self.planner, "session_id", self.trace.run_id if self.trace else "tool_session"),
            task_id=getattr(self.planner, "task_id", self.trace.run_id if self.trace else "tool_task"),
            phase_id=getattr(getattr(self.planner, "state", None), "current_phase", "tool_dispatch"),
            action_id=tool_call_id or "tool_dispatch",
            component=PolicyComponent.TOOL,
            operation=operation,
            capability=capability,
            subject=PolicySubject(subject_type="component", name="ToolExecutor"),
            resource=resource,
            reason=f"Dispatch tool {tool_name}",
            proposed_by_model=True,
            risk_tags=list(spec.risk_tags),
            metadata={
                "tool_name": tool_name,
                "argument_fingerprint": self.argument_summary(validated_args)["hash"],
                "permission_level": spec.permission_level.value,
                "risk_tags": list(spec.risk_tags),
                "delegated_executor": spec.uses_mutation_manager or spec.uses_command_executor,
                "timeout": spec.timeout_seconds,
                "max_output_chars": spec.max_output_chars,
                "backend": spec.execution_backend.value,
                "resources": resource_details,
                **_policy_argument_metadata(validated_args),
            },
            touches_workspace=capability
            in {
                Capability.READ_WORKSPACE,
                Capability.LIST_DIRECTORY,
                Capability.MUTATE_WORKSPACE,
                Capability.CREATE_FILE,
                Capability.DELETE_FILE,
                Capability.MOVE_FILE,
            },
            touches_secrets=any(item.sensitive for item in resources)
            or resource.sensitive
            or capability in {Capability.READ_SECRET, Capability.READ_ENV},
            destructive=capability == Capability.DELETE_FILE,
            long_running=operation == OperationKind.START_LONG_PROCESS,
            workspace_root=str(self.workspace_root),
        )

    def _policy_failure(self, request: PolicyRequest, decision: PolicyDecision) -> ToolResult:
        return ToolResult.failure(
            code=_policy_error_code(decision.outcome),
            message=self.redactor.redact_text(decision.reason),
            details={
                "policy": self.redactor.redact_value(decision.to_dict()),
                "request": self._safe_request_details(request),
            },
            metadata={"policy_decision_id": decision.decision_id},
        )

    def _safe_request_details(self, request: PolicyRequest) -> dict[str, Any]:
        payload = request.to_dict()
        payload["resource"]["identifier"] = redact_resource_identifier(
            request.resource.identifier
        )
        payload["resource"]["normalized_identifier"] = (
            redact_resource_identifier(request.resource.normalized_identifier)
            if request.resource.normalized_identifier
            else None
        )
        payload["metadata"] = {
            key: (
                redacted_resource_details(value)
                if key == "resources"
                else value
            )
            for key, value in payload.get("metadata", {}).items()
            if key != "arguments"
        }
        return self.redactor.redact_value(payload)

    def _record_policy_observation(self, request: PolicyRequest, decision: PolicyDecision) -> None:
        if self.planner is None or not hasattr(self.planner, "record_policy_observation"):
            return
        self.planner.record_policy_observation(
            {
                "outcome": decision.outcome.value,
                "component": request.component.value,
                "operation": request.operation.value,
                "reason": decision.reason,
                "risk_level": decision.risk_level.value,
                "resource": redact_resource_identifier(request.resource.identifier),
                "resources": redacted_resource_details(
                    request.metadata.get("resources")
                ),
                "decision_id": decision.decision_id,
            }
        )


def _policy_argument_metadata(args: dict[str, Any]) -> dict[str, Any]:
    metadata: dict[str, Any] = {}
    for key in (
        "argv",
        "shell",
        "filesystem_mode",
        "network_mode",
        "purpose",
        "risk_acceptance_reason",
    ):
        if key in args:
            metadata[key] = args[key]
    if args.get("argv"):
        metadata["command"] = " ".join(str(part) for part in args["argv"])
    elif args.get("shell"):
        metadata["command"] = str(args["shell"])
    return metadata


def _delegates_policy_decision(spec: ToolSpec, result: ToolResult) -> bool:
    if result.error_code != "sandbox_required":
        return False
    if not spec.delegates_policy_constraints:
        return False
    return spec.execution_backend == ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNNER


def _policy_error_code(outcome: DecisionOutcome) -> str:
    mapping = {
        DecisionOutcome.DENY: "policy_denied",
        DecisionOutcome.REQUIRE_REVIEW: "approval_required",
        DecisionOutcome.SANDBOX_REQUIRED: "sandbox_required",
        DecisionOutcome.ASK_USER: "policy_ask_user_required",
        DecisionOutcome.ESCALATE: "policy_escalation_required",
    }
    return mapping.get(outcome, "policy_denied")


def _decision_allowed_by_grant(decision: PolicyDecision, grant_id: str) -> PolicyDecision:
    return decision.model_copy_with(
        outcome=DecisionOutcome.ALLOW,
        reason="Action allowed by matching ApprovalGrant.",
        approval_grant_id=grant_id,
        required_approval=None,
    )
