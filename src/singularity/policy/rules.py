from __future__ import annotations

import os
import re
import shlex
from dataclasses import dataclass, field
from pathlib import Path

from singularity.policy.config import PolicyConfig
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
from singularity.policy.permissions import (
    ApprovalPolicy,
    NetworkAccess,
    PermissionProfile,
    PermissionProfileName,
    ProtectedPathRule,
)
from singularity.policy.risk import RiskAssessment


@dataclass(frozen=True)
class RuleResult:
    outcome: DecisionOutcome
    reason: str
    rule_id: str
    constraints: PolicyConstraints = field(default_factory=PolicyConstraints)
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
        profile = config.permission_profile
        if profile is None:  # PolicyConfig constructs this; fail closed if bypassed.
            result = RuleResult(
                DecisionOutcome.DENY,
                "No session permission profile is configured.",
                "deny_missing_permission_profile",
            )
        else:
            result = self._decide(request, risk=risk, profile=profile)
            if (
                result.outcome == DecisionOutcome.REQUIRE_REVIEW
                and profile.approval_policy == ApprovalPolicy.NEVER
            ):
                result = RuleResult(
                    DecisionOutcome.DENY,
                    f"{result.reason} Approval policy is never.",
                    f"{result.rule_id}_approval_never",
                    constraints=result.constraints,
                    review_kind=result.review_kind,
                )

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
            context_summary=policy_context_summary(
                request, result.outcome, result.reason
            ),
        )

    def _decide(
        self,
        request: PolicyRequest,
        *,
        risk: RiskAssessment,
        profile: PermissionProfile,
    ) -> RuleResult:
        tags = set(risk.tags)
        operation = request.operation

        if request.metadata.get("command_missing"):
            return RuleResult(
                DecisionOutcome.DENY,
                "Command request must provide argv or shell.",
                "hard_deny_command_parse_error",
            )
        if request.metadata.get("cwd_outside_workspace"):
            return RuleResult(
                DecisionOutcome.DENY,
                "Command cwd is outside the current workspace.",
                "hard_deny_cwd_outside_workspace",
            )

        protected = _protected_path_violation(request, profile, tags)
        if protected is not None:
            return RuleResult(
                DecisionOutcome.DENY,
                f"Protected path access denied: {protected.description or protected.pattern}.",
                "hard_deny_protected_path",
            )

        if RiskTag.SECRETS_EXFILTRATION in tags or (
            RiskTag.SECRET_ACCESS in tags and RiskTag.NETWORK in tags
        ):
            return RuleResult(
                DecisionOutcome.DENY,
                "Action combines secret access with network risk.",
                "hard_deny_secret_exfiltration",
            )
        if RiskTag.SECRET_ACCESS in tags:
            return RuleResult(
                DecisionOutcome.DENY,
                "Sensitive credential or secret access is denied.",
                "hard_deny_secret_access",
            )
        command_text = _command_text(request).lower()
        if "encodedcommand" in command_text or " -enc " in command_text:
            return RuleResult(
                DecisionOutcome.DENY,
                "Encoded shell commands are denied.",
                "hard_deny_encoded_command",
            )
        if "curl" in command_text and "|" in command_text:
            return RuleResult(
                DecisionOutcome.DENY,
                "Remote scripts piped into interpreters are denied.",
                "hard_deny_remote_script_pipe",
            )

        if operation in {
            OperationKind.READ_FILE,
            OperationKind.LIST_DIRECTORY,
            OperationKind.SEARCH,
        }:
            return RuleResult(
                DecisionOutcome.ALLOW,
                "Filesystem read is allowed outside protected paths.",
                "allow_filesystem_read",
            )

        if _always_review(request, tags, command_text):
            return _review_result(request, "High-risk action requires user review.")

        if _is_network_action(request, tags):
            if profile.network_access == NetworkAccess.DENIED:
                return _review_result(
                    request,
                    "Network access is outside the current session permission boundary.",
                )
            if profile.profile == PermissionProfileName.DANGER_FULL_ACCESS:
                return RuleResult(
                    DecisionOutcome.ALLOW,
                    "Network access is allowed by the session permission profile.",
                    "allow_profile_network",
                )
            return _sandbox_result(
                request,
                profile,
                reason="Network-enabled execution requires OS sandbox enforcement.",
                rule_id="sandbox_profile_network",
            )

        if operation in {
            OperationKind.MUTATE_FILE,
            OperationKind.CREATE_FILE,
            OperationKind.ROLLBACK,
            OperationKind.CHANGE_CONFIG,
        }:
            path = _primary_path(request, profile)
            paths = _path_candidates(request, profile)
            if (
                profile.profile != PermissionProfileName.READ_ONLY
                and (path is not None or paths)
                and all(profile.is_writable_path(candidate) for candidate in (paths or (path,)))
            ):
                return RuleResult(
                    DecisionOutcome.ALLOW,
                    "Write is within a session-authorized writable root.",
                    "allow_profile_writable_path",
                )
            return _review_result(
                request,
                "Write is outside the current session write boundary.",
            )

        if operation in {
            OperationKind.EXECUTE_COMMAND,
            OperationKind.EXECUTE_PROJECT_CODE,
            OperationKind.START_LONG_PROCESS,
            OperationKind.VERIFICATION,
        }:
            if profile.profile == PermissionProfileName.READ_ONLY:
                return _review_result(
                    request,
                    "Command execution requires review in read-only mode.",
                )
            if profile.profile == PermissionProfileName.DANGER_FULL_ACCESS:
                review_reason = _danger_full_access_review_reason(request, tags)
                if review_reason is not None:
                    return _review_result(request, review_reason)
                return RuleResult(
                    DecisionOutcome.ALLOW,
                    "Local command execution is allowed by danger-full-access.",
                    "allow_profile_local_command",
                )
            return _sandbox_result(
                request,
                profile,
                reason="Local command execution requires OS sandbox enforcement.",
                rule_id="sandbox_profile_local_command",
            )

        if request.resource.resource_type == "env":
            return _review_result(request, "Environment access requires user review.")

        return _review_result(
            request,
            "Action is not covered by an automatic session permission.",
        )


def _review_result(request: PolicyRequest, reason: str) -> RuleResult:
    return RuleResult(
        DecisionOutcome.REQUIRE_REVIEW,
        reason,
        "require_review_permission_boundary",
        constraints=PolicyConstraints(
            filesystem_mode="read-only",
            network_allowed=False,
            max_duration_seconds=request.metadata.get("timeout"),
            max_output_chars=request.metadata.get("max_output_chars"),
            env_redaction=True,
        ),
        review_kind=_review_kind(request),
    )


def _sandbox_result(
    request: PolicyRequest,
    profile: PermissionProfile,
    *,
    reason: str,
    rule_id: str,
) -> RuleResult:
    return RuleResult(
        DecisionOutcome.SANDBOX_REQUIRED,
        reason,
        rule_id,
        constraints=PolicyConstraints(
            filesystem_mode=profile.profile.value,
            network_allowed=profile.network_access == NetworkAccess.ALLOWED,
            max_duration_seconds=request.metadata.get("timeout"),
            max_output_chars=request.metadata.get("max_output_chars"),
            env_redaction=True,
            sandbox_required=True,
            hard_isolation_required=True,
        ),
    )


def _always_review(
    request: PolicyRequest, tags: set[RiskTag], command_text: str
) -> bool:
    if request.operation in {
        OperationKind.DELETE_FILE,
        OperationKind.PACKAGE_INSTALL,
        OperationKind.KILL_PROCESS,
    }:
        return True
    if (
        request.operation == OperationKind.START_LONG_PROCESS
        and not request.metadata.get("risk_acceptance_reason")
    ):
        return True
    if request.destructive or RiskTag.DESTRUCTIVE in tags:
        return True
    if RiskTag.PACKAGE_MANAGER in tags or RiskTag.SUPPLY_CHAIN in tags:
        return True
    command_risks = set(request.metadata.get("command_risk_tags") or [])
    if command_risks.intersection({"DESTRUCTIVE", "SYSTEM_MUTATION", "VCS_MUTATION"}):
        return True
    if "sudo" in command_text or "runas" in command_text:
        return True
    return _is_vcs_mutation(command_text)


def _danger_full_access_review_reason(
    request: PolicyRequest,
    tags: set[RiskTag],
) -> str | None:
    command_risks = set(request.metadata.get("command_risk_tags") or [])
    if request.metadata.get("shell"):
        return "Shell commands require review because parsing is delegated to the shell."
    purpose = str(request.metadata.get("command_purpose") or "")
    if (
        RiskTag.MUTATES_FILES in tags
        and purpose != "FORMATTER"
        and not request.metadata.get("risk_acceptance_reason")
    ):
        return "Workspace-writing commands require an explicit risk acceptance reason."
    if "WRITE_WORKSPACE" in command_risks and purpose != "FORMATTER" and not request.metadata.get("risk_acceptance_reason"):
        return "Workspace-writing commands require an explicit risk acceptance reason."
    if "LONG_RUNNING" in command_risks and not request.metadata.get("risk_acceptance_reason"):
        return "Long-running process sessions require explicit ownership."
    return None


def _is_vcs_mutation(command_text: str) -> bool:
    try:
        argv = shlex.split(command_text, posix=os.name != "nt")
    except ValueError:
        argv = command_text.split()
    if not argv or Path(argv[0]).stem.lower() != "git":
        return False
    return len(argv) > 1 and argv[1].lower() in {
        "add",
        "am",
        "apply",
        "branch",
        "checkout",
        "cherry-pick",
        "clean",
        "commit",
        "fetch",
        "merge",
        "mv",
        "pull",
        "push",
        "rebase",
        "reset",
        "restore",
        "revert",
        "rm",
        "stash",
        "switch",
        "tag",
    }


def _is_network_action(request: PolicyRequest, tags: set[RiskTag]) -> bool:
    return (
        request.operation == OperationKind.NETWORK_ACCESS
        or request.capability == Capability.NETWORK_ACCESS
        or request.requires_network
        or RiskTag.NETWORK in tags
    )


def _protected_path_violation(
    request: PolicyRequest,
    profile: PermissionProfile,
    tags: set[RiskTag],
) -> ProtectedPathRule | None:
    access = _path_access(request, tags)
    for candidate in _path_candidates(request, profile):
        rule = profile.matching_protected_rule(candidate, access=access)
        if rule is not None and rule.hard_deny:
            return rule
    return None


def _path_access(request: PolicyRequest, tags: set[RiskTag]) -> str:
    if request.operation in {
        OperationKind.MUTATE_FILE,
        OperationKind.CREATE_FILE,
        OperationKind.DELETE_FILE,
        OperationKind.ROLLBACK,
        OperationKind.CHANGE_CONFIG,
    } or RiskTag.MUTATES_FILES in tags:
        return "write"
    if request.operation in {
        OperationKind.EXECUTE_COMMAND,
        OperationKind.EXECUTE_PROJECT_CODE,
        OperationKind.PACKAGE_INSTALL,
        OperationKind.START_LONG_PROCESS,
        OperationKind.VERIFICATION,
    }:
        return "execute"
    return "read"


def _path_candidates(
    request: PolicyRequest, profile: PermissionProfile
) -> tuple[Path, ...]:
    candidates: list[Path] = []
    if request.resource.resource_type in {"file", "directory", "workspace", "config"}:
        candidates.append(_resolve_request_path(request.resource.identifier, profile))
    resources = request.metadata.get("resources")
    if isinstance(resources, list):
        for item in resources:
            if not isinstance(item, dict):
                continue
            if str(item.get("resource_type") or "") not in {
                "",
                "file",
                "directory",
                "workspace",
                "config",
            }:
                continue
            identifier = item.get("normalized_identifier") or item.get("identifier")
            if identifier:
                candidates.append(_resolve_request_path(str(identifier), profile))
    files_changed = request.metadata.get("files_changed")
    if isinstance(files_changed, list):
        for item in files_changed:
            if isinstance(item, str) and item:
                candidates.append(_resolve_request_path(item, profile))
    if request.resource.resource_type == "command":
        for token in _command_path_tokens(_command_text(request)):
            candidates.append(_resolve_request_path(token, profile))
    return tuple(dict.fromkeys(candidates))


def _primary_path(
    request: PolicyRequest, profile: PermissionProfile
) -> Path | None:
    if request.resource.resource_type not in {"file", "directory", "workspace", "config"}:
        return None
    return _resolve_request_path(
        request.resource.normalized_identifier or request.resource.identifier,
        profile,
    )


def _resolve_request_path(value: str, profile: PermissionProfile) -> Path:
    raw = Path(value).expanduser()
    if not raw.is_absolute():
        raw = profile.workspace_roots[0] / raw
    return raw.resolve(strict=False)


def _command_text(request: PolicyRequest) -> str:
    return str(
        request.metadata.get("command")
        or request.metadata.get("shell")
        or (request.resource.identifier if request.resource.resource_type == "command" else "")
        or ""
    )


def _command_path_tokens(command: str) -> tuple[str, ...]:
    try:
        argv = shlex.split(command, posix=os.name != "nt")
    except ValueError:
        argv = command.split()
    lexical_tokens = re.split(r"[\s\"'<>|;]+", command)
    tokens: list[str] = []
    for token in (*argv[1:], *lexical_tokens[1:]):
        candidate = token.strip("'\";,()[]{}")
        if not candidate or candidate.startswith("-") or "://" in candidate:
            continue
        normalized = candidate.replace("\\", "/")
        if (
            "/" in normalized
            or normalized.startswith(".")
            or Path(candidate).suffix.lower()
            in {".env", ".json", ".pem", ".key", ".pfx", ".p12"}
        ):
            tokens.append(candidate)
    return tuple(tokens)


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
    if outcome in {
        DecisionOutcome.REQUIRE_REVIEW,
        DecisionOutcome.SANDBOX_REQUIRED,
    }:
        return "warning"
    if risk_level in {RiskLevel.HIGH, RiskLevel.CRITICAL}:
        return "warning"
    return "info"
