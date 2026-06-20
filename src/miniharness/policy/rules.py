from __future__ import annotations

from dataclasses import dataclass

from miniharness.policy.config import ApprovalMode, PolicyConfig, SecurityMode
from miniharness.policy.models import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConstraints,
    PolicyDecision,
    PolicyRequest,
    RiskLevel,
    RiskTag,
    approval_scope_for_request,
    policy_context_summary,
)
from miniharness.policy.risk import RiskAssessment

READ_ONLY_FILESYSTEM_MODES = {"READ_ONLY_WORKSPACE", "read_only", "read_only_workspace"}


@dataclass(frozen=True)
class RuleResult:
    outcome: DecisionOutcome
    reason: str
    rule_id: str
    constraints: PolicyConstraints = PolicyConstraints()
    review_kind: str = "generic"
    user_message: str = ""


class DefaultLocalPolicyRules:
    def decide(
        self,
        request: PolicyRequest,
        *,
        risk: RiskAssessment,
        config: PolicyConfig,
    ) -> PolicyDecision:
        result = self._decide(request, risk=risk, config=config)
        required = None
        if result.outcome == DecisionOutcome.REQUIRE_REVIEW:
            from miniharness.policy.models import ApprovalRequirement

            required = ApprovalRequirement(
                message=result.user_message or result.reason,
                scope=approval_scope_for_request(request),
                review_kind=result.review_kind,
                details={
                    "runtime": request.runtime.value,
                    "operation": request.operation.value,
                    "resource": request.resource.identifier,
                    "risk_level": risk.level.value,
                },
            )
        return PolicyDecision(
            request_id=request.request_id,
            outcome=result.outcome,
            risk_level=risk.level,
            risk_tags=risk.tags,
            reason=result.reason,
            user_message=result.user_message,
            constraints=result.constraints,
            required_approval=required,
            rule_ids=[result.rule_id],
            audit_severity=_severity(result.outcome, risk.level),
            context_summary=policy_context_summary(request, result.outcome, result.reason),
        )

    def _decide(
        self,
        request: PolicyRequest,
        *,
        risk: RiskAssessment,
        config: PolicyConfig,
    ) -> RuleResult:
        tags = set(risk.tags)
        operation = request.operation
        capability = request.capability

        if RiskTag.SECRETS_EXFILTRATION in tags or (
            RiskTag.SECRET_ACCESS in tags and RiskTag.NETWORK in tags
        ):
            return RuleResult(
                DecisionOutcome.DENY,
                "Action combines secret access with network risk.",
                "hard_deny_secret_exfiltration",
            )
        if RiskTag.SECRET_ACCESS in tags and (
            risk.level == RiskLevel.CRITICAL or config.deny_secret_access_by_default
        ):
            return RuleResult(
                DecisionOutcome.DENY,
                "Highly sensitive secret access is denied by default.",
                "hard_deny_secret_access",
            )
        if _outside_write_or_delete(request, tags) and config.deny_outside_workspace_write:
            return RuleResult(
                DecisionOutcome.DENY,
                "Workspace-outside write/delete is denied.",
                "hard_deny_outside_workspace_write",
            )
        if risk.level == RiskLevel.CRITICAL and RiskTag.DESTRUCTIVE in tags:
            return RuleResult(
                DecisionOutcome.DENY,
                "Critical destructive action is denied.",
                "hard_deny_destructive",
            )
        if "sudo" in request.resource.identifier.lower() or "runas" in request.resource.identifier.lower():
            return RuleResult(
                DecisionOutcome.ESCALATE,
                "Administrator privilege requests must be handled outside Miniharness.",
                "escalate_admin_privilege",
            )
        if "encodedcommand" in request.resource.identifier.lower():
            return RuleResult(
                DecisionOutcome.DENY,
                "Encoded shell commands are denied.",
                "hard_deny_encoded_command",
            )
        if "curl" in request.resource.identifier.lower() and "|" in request.resource.identifier:
            return RuleResult(
                DecisionOutcome.DENY,
                "Remote scripts piped into interpreters are denied.",
                "hard_deny_remote_script_pipe",
            )

        if config.approval_mode == ApprovalMode.READ_ONLY:
            if operation in {
                OperationKind.READ_FILE,
                OperationKind.LIST_DIRECTORY,
                OperationKind.SEARCH,
            } and capability in {
                Capability.READ_WORKSPACE,
                Capability.LIST_DIRECTORY,
            }:
                return RuleResult(DecisionOutcome.ALLOW, "Read-only operation allowed.", "read_only_allow")
            return RuleResult(
                DecisionOutcome.DENY,
                "read_only mode only allows low-risk workspace reads.",
                "read_only_deny_non_read",
            )

        if config.approval_mode == ApprovalMode.REVIEW_ALL:
            return RuleResult(
                DecisionOutcome.REQUIRE_REVIEW,
                "review_all mode requires user review.",
                "review_all",
                review_kind=_review_kind(request),
            )

        if operation in {
            OperationKind.READ_FILE,
            OperationKind.LIST_DIRECTORY,
            OperationKind.SEARCH,
        } and capability in {Capability.READ_WORKSPACE, Capability.LIST_DIRECTORY}:
            return RuleResult(DecisionOutcome.ALLOW, "Workspace read is allowed.", "auto_allow_workspace_read")

        if capability == Capability.EXECUTE_GENERATED_CODE or RiskTag.EXECUTES_GENERATED_CODE in tags:
            return RuleResult(
                DecisionOutcome.SANDBOX_REQUIRED,
                "Generated code execution requires a sandbox backend.",
                "sandbox_generated_code",
                constraints=PolicyConstraints(
                    sandbox_required=True,
                    hard_isolation_required=True,
                    filesystem_mode="copy_on_write_workspace",
                    network_allowed=False,
                    max_duration_seconds=request.metadata.get("timeout"),
                    max_output_chars=request.metadata.get("max_output_chars"),
                    env_redaction=True,
                ),
            )

        if config.security_mode == SecurityMode.COMPAT and _compat_local_command_allow(request, risk):
            return RuleResult(
                DecisionOutcome.ALLOW,
                "Plain local command allowed by compat security mode.",
                "compat_local_command_allow",
            )

        if operation == OperationKind.VERIFICATION:
            return RuleResult(
                DecisionOutcome.SANDBOX_REQUIRED,
                "Verification command execution requires an isolated sandbox.",
                "sandbox_verification",
                constraints=PolicyConstraints(
                    sandbox_required=True,
                    hard_isolation_required=True,
                    filesystem_mode="copy_on_write_workspace",
                    network_allowed=False,
                    max_duration_seconds=request.metadata.get("timeout"),
                    max_output_chars=request.metadata.get("max_output_chars"),
                    env_redaction=True,
                ),
            )

        if config.approval_mode == ApprovalMode.AUTO_SAFE and _auto_safe_runtime_allow(request, risk):
            return RuleResult(DecisionOutcome.ALLOW, "Low-risk runtime action allowed by auto_safe mode.", "auto_safe_runtime_allow")

        if operation in {
            OperationKind.MUTATE_FILE,
            OperationKind.CREATE_FILE,
            OperationKind.DELETE_FILE,
            OperationKind.ROLLBACK,
            OperationKind.EXECUTE_COMMAND,
            OperationKind.EXECUTE_PROJECT_CODE,
            OperationKind.PACKAGE_INSTALL,
            OperationKind.NETWORK_ACCESS,
            OperationKind.START_LONG_PROCESS,
            OperationKind.KILL_PROCESS,
            OperationKind.CHANGE_CONFIG,
            OperationKind.VERIFICATION,
        }:
            return RuleResult(
                DecisionOutcome.REQUIRE_REVIEW,
                f"{operation.value} requires local CLI review.",
                "require_review_runtime_action",
                constraints=PolicyConstraints(
                    filesystem_mode=(
                        "workspace_write"
                        if operation
                        in {
                            OperationKind.MUTATE_FILE,
                            OperationKind.CREATE_FILE,
                            OperationKind.DELETE_FILE,
                            OperationKind.ROLLBACK,
                        }
                        else "read_only"
                    ),
                    network_allowed=RiskTag.NETWORK in tags,
                    max_duration_seconds=request.metadata.get("timeout"),
                    env_redaction=True,
                ),
                review_kind=_review_kind(request),
            )

        if request.resource.resource_type == "env":
            return RuleResult(
                DecisionOutcome.REQUIRE_REVIEW,
                "Environment access requires review.",
                "require_review_env",
                review_kind="config",
            )

        return RuleResult(DecisionOutcome.ASK_USER, "Policy needs more information.", "ask_user_insufficient_context")


def _outside_write_or_delete(request: PolicyRequest, tags: set[RiskTag]) -> bool:
    return RiskTag.OUTSIDE_WORKSPACE in tags and request.operation in {
        OperationKind.MUTATE_FILE,
        OperationKind.CREATE_FILE,
        OperationKind.DELETE_FILE,
        OperationKind.ROLLBACK,
        OperationKind.CHANGE_CONFIG,
    }


def _auto_safe_runtime_allow(request: PolicyRequest, risk: RiskAssessment) -> bool:
    if request.operation == OperationKind.START_LONG_PROCESS:
        return bool(request.metadata.get("risk_acceptance_reason"))
    if risk.level in {RiskLevel.HIGH, RiskLevel.CRITICAL}:
        return False
    if (
        request.operation in {OperationKind.EXECUTE_COMMAND, OperationKind.EXECUTE_PROJECT_CODE}
        and RiskTag.MUTATES_FILES in risk.tags
        and request.metadata.get("filesystem_mode") in READ_ONLY_FILESYSTEM_MODES
    ):
        return False
    if request.operation in {
        OperationKind.DELETE_FILE,
        OperationKind.PACKAGE_INSTALL,
        OperationKind.NETWORK_ACCESS,
        OperationKind.CHANGE_CONFIG,
    }:
        return False
    if request.runtime.value == "tool" and request.metadata.get("delegated_runtime"):
        return True
    return request.operation in {
        OperationKind.READ_FILE,
        OperationKind.LIST_DIRECTORY,
        OperationKind.SEARCH,
        OperationKind.MUTATE_FILE,
        OperationKind.CREATE_FILE,
        OperationKind.EXECUTE_COMMAND,
        OperationKind.EXECUTE_PROJECT_CODE,
        OperationKind.VERIFICATION,
    }


def _compat_local_command_allow(request: PolicyRequest, risk: RiskAssessment) -> bool:
    if request.runtime.value != "command":
        return False
    if request.operation == OperationKind.VERIFICATION:
        if request.capability != Capability.EXECUTE_PROJECT_CODE:
            return False
    elif request.operation == OperationKind.EXECUTE_COMMAND:
        if request.capability != Capability.EXECUTE_COMMAND:
            return False
    else:
        return False
    if request.requires_network or request.touches_workspace or request.touches_secrets:
        return False
    if request.destructive or request.long_running:
        return False
    if risk.level in {RiskLevel.HIGH, RiskLevel.CRITICAL}:
        return False
    blocked_tags = {
        RiskTag.NETWORK,
        RiskTag.PACKAGE_MANAGER,
        RiskTag.SUPPLY_CHAIN,
        RiskTag.LONG_RUNNING,
        RiskTag.DESTRUCTIVE,
        RiskTag.IRREVERSIBLE,
        RiskTag.MUTATES_FILES,
        RiskTag.MUTATES_CONFIG,
        RiskTag.MUTATES_LOCKFILE,
        RiskTag.EXECUTES_GENERATED_CODE,
        RiskTag.SECRET_ACCESS,
        RiskTag.SECRETS_EXFILTRATION,
    }
    if request.operation != OperationKind.VERIFICATION:
        blocked_tags.add(RiskTag.EXECUTES_PROJECT_CODE)
    return not (set(risk.tags) & blocked_tags)


def _review_kind(request: PolicyRequest) -> str:
    mapping = {
        OperationKind.EXECUTE_COMMAND: "command",
        OperationKind.EXECUTE_PROJECT_CODE: "command",
        OperationKind.MUTATE_FILE: "mutation",
        OperationKind.CREATE_FILE: "mutation",
        OperationKind.DELETE_FILE: "delete",
        OperationKind.ROLLBACK: "rollback",
        OperationKind.NETWORK_ACCESS: "network",
        OperationKind.PACKAGE_INSTALL: "package",
        OperationKind.START_LONG_PROCESS: "long_process",
        OperationKind.CHANGE_CONFIG: "config",
        OperationKind.VERIFICATION: "command",
    }
    return mapping.get(request.operation, "generic")


def _severity(outcome: DecisionOutcome, risk_level: RiskLevel) -> str:
    if outcome in {DecisionOutcome.DENY, DecisionOutcome.ESCALATE}:
        return "error"
    if outcome in {DecisionOutcome.REQUIRE_REVIEW, DecisionOutcome.SANDBOX_REQUIRED}:
        return "warning"
    if risk_level in {RiskLevel.HIGH, RiskLevel.CRITICAL}:
        return "warning"
    return "info"
