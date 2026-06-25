from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from singularity.policy.config import ApprovalMode, PolicyConfig, SecurityMode
from singularity.policy.models import (
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
from singularity.policy.risk import RiskAssessment

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
            from singularity.policy.models import ApprovalRequirement

            required = ApprovalRequirement(
                message=result.user_message or result.reason,
                scope=approval_scope_for_request(request),
                review_kind=result.review_kind,
                details={
                    "component": request.component.value,
                    "operation": request.operation.value,
                    "resource": request.resource.identifier,
                    "risk_level": risk.level.value,
                },
            )
        return PolicyDecision(
            request_id=request.request_id,
            outcome=result.outcome,
            risk_level=risk.level,
            risk_tags=list(risk.tags),
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

        disabled = _disabled_by_config(request, config)
        if disabled is not None:
            return disabled

        # Trust boundary: hard-deny any write/command operation that targets
        # the workspace-local policy directory. This prevents the model from
        # forging approval grants or audit entries through shell writes.
        if _targets_workspace_policy_dir(request, config):
            return RuleResult(
                DecisionOutcome.DENY,
                "Writes to the workspace policy directory are denied to prevent approval forgery.",
                "hard_deny_workspace_policy_dir_write",
            )

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
                "Administrator privilege requests must be handled outside Singularity.",
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

        if config.security_mode == SecurityMode.STRICT and _strict_command_requires_sandbox(request):
            return RuleResult(
                DecisionOutcome.SANDBOX_REQUIRED,
                "Strict security mode requires command execution through an isolated sandbox.",
                "strict_command_sandbox_required",
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

        if config.approval_mode == ApprovalMode.AUTO_SAFE and _auto_safe_component_allow(request, risk):
            return RuleResult(DecisionOutcome.ALLOW, "Low-risk component action allowed by auto_safe mode.", "auto_safe_component_allow")

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
                "require_review_pipeline_action",
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


def _auto_safe_component_allow(request: PolicyRequest, risk: RiskAssessment) -> bool:
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
    if request.component.value == "tool" and request.metadata.get("delegated_executor"):
        return True
    return request.operation in {
        OperationKind.READ_FILE,
        OperationKind.LIST_DIRECTORY,
        OperationKind.SEARCH,
        OperationKind.MUTATE_FILE,
        OperationKind.CREATE_FILE,
        OperationKind.ROLLBACK,
        OperationKind.EXECUTE_COMMAND,
        OperationKind.EXECUTE_PROJECT_CODE,
        OperationKind.VERIFICATION,
    }


def _disabled_by_config(
    request: PolicyRequest,
    config: PolicyConfig,
) -> RuleResult | None:
    operation = request.operation
    capability = request.capability
    if (
        not config.allow_workspace_reads
        and operation in {OperationKind.READ_FILE, OperationKind.LIST_DIRECTORY, OperationKind.SEARCH}
        and capability in {Capability.READ_WORKSPACE, Capability.LIST_DIRECTORY}
    ):
        return RuleResult(DecisionOutcome.DENY, "Workspace reads are disabled by policy config.", "config_deny_workspace_read")
    if (
        not config.allow_workspace_mutation_with_review
        and operation in {
            OperationKind.MUTATE_FILE,
            OperationKind.CREATE_FILE,
            OperationKind.DELETE_FILE,
            OperationKind.ROLLBACK,
        }
    ):
        return RuleResult(DecisionOutcome.DENY, "Workspace mutation review is disabled by policy config.", "config_deny_workspace_mutation")
    if (
        not config.allow_command_with_review
        and operation in {
            OperationKind.EXECUTE_COMMAND,
            OperationKind.EXECUTE_PROJECT_CODE,
            OperationKind.START_LONG_PROCESS,
            OperationKind.KILL_PROCESS,
            OperationKind.VERIFICATION,
        }
    ):
        return RuleResult(DecisionOutcome.DENY, "Command execution review is disabled by policy config.", "config_deny_command")
    if (
        not config.allow_network_with_review
        and (
            operation == OperationKind.NETWORK_ACCESS
            or request.requires_network
            or capability == Capability.NETWORK_ACCESS
        )
    ):
        return RuleResult(DecisionOutcome.DENY, "Network access review is disabled by policy config.", "config_deny_network")
    if not config.allow_package_install_with_review and operation == OperationKind.PACKAGE_INSTALL:
        return RuleResult(DecisionOutcome.DENY, "Package install review is disabled by policy config.", "config_deny_package_install")
    return None


def _compat_local_command_allow(request: PolicyRequest, risk: RiskAssessment) -> bool:
    if request.component.value != "command":
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


def _strict_command_requires_sandbox(request: PolicyRequest) -> bool:
    if request.component.value != "command":
        return False
    return request.operation in {
        OperationKind.EXECUTE_COMMAND,
        OperationKind.EXECUTE_PROJECT_CODE,
        OperationKind.START_LONG_PROCESS,
        OperationKind.VERIFICATION,
    }


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


_POLICY_DIR_WRITE_OPERATIONS = {
    OperationKind.MUTATE_FILE,
    OperationKind.CREATE_FILE,
    OperationKind.DELETE_FILE,
    OperationKind.ROLLBACK,
    OperationKind.CHANGE_CONFIG,
    OperationKind.EXECUTE_COMMAND,
    OperationKind.EXECUTE_PROJECT_CODE,
    OperationKind.START_LONG_PROCESS,
    OperationKind.VERIFICATION,
    OperationKind.PACKAGE_INSTALL,
}


def _targets_workspace_policy_dir(request: PolicyRequest, config: PolicyConfig) -> bool:
    """Detect attempts to write to ``<workspace>/.singularity/policy/``.

    Trust boundary: the workspace-local policy directory previously stored
    approval grants and audit logs. Allowing the model to write there would
    let it forge grants and bypass human approval. This guard hard-denies
    any write or command operation whose target path resolves under that
    directory, and also blocks command strings that reference it.
    """
    if request.operation not in _POLICY_DIR_WRITE_OPERATIONS:
        return False

    workspace_root = Path(config.workspace_root).expanduser().resolve(strict=False)
    policy_dir = workspace_root / ".singularity" / "policy"

    candidate_identifiers: list[str] = []
    if request.resource.resource_type in {"file", "directory", "workspace", "config"}:
        candidate_identifiers.append(
            request.resource.normalized_identifier or request.resource.identifier
        )
    resources_raw = request.metadata.get("resources")
    if isinstance(resources_raw, list):
        for item in resources_raw:
            if not isinstance(item, dict):
                continue
            resource_type = str(item.get("resource_type") or "")
            if resource_type and resource_type not in {"file", "directory", "workspace", "config"}:
                continue
            identifier = item.get("normalized_identifier") or item.get("identifier")
            if identifier:
                candidate_identifiers.append(str(identifier))

    for identifier in candidate_identifiers:
        raw = Path(str(identifier)).expanduser()
        candidate = raw if raw.is_absolute() else workspace_root / raw
        try:
            resolved = candidate.resolve(strict=False)
        except OSError:
            continue
        if _is_within_policy_dir(resolved, policy_dir):
            return True

    command_text = str(
        request.metadata.get("command")
        or request.metadata.get("shell")
        or (request.resource.identifier if request.resource.resource_type == "command" else "")
        or ""
    )
    if command_text and _command_references_policy_dir(command_text):
        return True

    return False


def _is_within_policy_dir(path: Path, policy_dir: Path) -> bool:
    try:
        path_key = os.path.normcase(os.path.normpath(str(path.resolve(strict=False))))
        dir_key = os.path.normcase(os.path.normpath(str(policy_dir.resolve(strict=False))))
        return os.path.commonpath([path_key, dir_key]) == dir_key
    except (OSError, ValueError):
        return False


def _command_references_policy_dir(command: str) -> bool:
    normalized = command.replace("\\", "/").lower()
    return ".singularity/policy/" in normalized
