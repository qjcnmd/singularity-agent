from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any
from uuid import uuid4

from singularity.command import CommandPolicyResult, CommandRequest


class ProjectLanguage(str, Enum):
    PYTHON = "python"
    JAVASCRIPT = "javascript"
    TYPESCRIPT = "typescript"
    RUST = "rust"
    GO = "go"
    JAVA = "java"
    UNKNOWN = "unknown"


class WorkspaceKind(str, Enum):
    SINGLE_PROJECT = "single_project"
    MONOREPO = "monorepo"
    UNKNOWN = "unknown"


class CheckKind(str, Enum):
    SYNTAX = "syntax"
    FORMAT = "format"
    LINT = "lint"
    TYPECHECK = "typecheck"
    UNIT_TEST = "unit_test"
    INTEGRATION_TEST = "integration_test"
    BUILD = "build"
    VERIFICATION_SMOKE = "verification_smoke"
    SECURITY = "security"
    CUSTOM = "custom"
    MANUAL_REVIEW = "manual_review"


class CheckStatus(str, Enum):
    PASSED = "passed"
    FAILED = "failed"
    SKIPPED = "skipped"
    BLOCKED = "blocked"
    FLAKY = "flaky"
    TIMEOUT = "timeout"
    INCONCLUSIVE = "inconclusive"


class FailureType(str, Enum):
    PROJECT_PROFILE_UNKNOWN = "project_profile_unknown"
    COMMAND_DISCOVERY_FAILED = "command_discovery_failed"
    VERIFICATION_PLAN_FAILED = "verification_plan_failed"
    CHECK_POLICY_DENIED = "check_policy_denied"
    CHECK_REVIEW_REQUIRED = "check_review_required"
    CHECK_BLOCKED = "check_blocked"
    COMMAND_EXECUTION_FAILED = "command_execution_failed"
    OUTPUT_PARSE_FAILED = "output_parse_failed"
    SYNTAX_ERROR = "syntax_error"
    TYPE_ERROR = "type_error"
    LINT_ERROR = "lint_error"
    FORMAT_ERROR = "format_error"
    UNIT_TEST_FAILURE = "unit_test_failure"
    INTEGRATION_TEST_FAILURE = "integration_test_failure"
    BUILD_FAILURE = "build_failure"
    MISSING_DEPENDENCY = "missing_dependency"
    MISSING_COMMAND = "missing_command"
    ENVIRONMENT_ERROR = "environment_error"
    CONFIGURATION_ERROR = "configuration_error"
    TIMEOUT = "timeout"
    FLAKY_FAILURE = "flaky_failure"
    EXTERNAL_SERVICE_UNAVAILABLE = "external_service_unavailable"
    PERMISSION_DENIED = "permission_denied"
    SANDBOX_LIMITATION = "sandbox_limitation"
    SANDBOX_VIOLATION = "sandbox_violation"
    INCONCLUSIVE_RESULT = "inconclusive_result"
    REPAIR_BUDGET_EXCEEDED = "repair_budget_exceeded"
    UNKNOWN_FAILURE = "unknown_failure"


class VerificationDecision(str, Enum):
    ALLOW = "allow"
    REQUIRE_REVIEW = "require_review"
    DENY = "deny"
    BLOCKED = "blocked"


class CompletionStatus(str, Enum):
    READY = "ready"
    READY_WITH_WARNINGS = "ready_with_warnings"
    BLOCKED = "blocked"
    FAILED = "failed"
    NEEDS_REVIEW = "needs_review"


@dataclass(frozen=True)
class DiscoveredCommand:
    name: str
    kind: CheckKind
    request: CommandRequest
    source: str
    confidence: float = 1.0
    description: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "kind": self.kind.value,
            "command": self.request.redacted_display_command(),
            "command_hash": self.request.command_hash(),
            "argv": self.request.redacted_argv(),
            "shell": self.request.redacted_shell(),
            "cwd": self.request.cwd,
            "purpose": self.request.purpose.value,
            "timeout_seconds": self.request.resource_limits.timeout_seconds,
            "source": self.source,
            "confidence": self.confidence,
            "description": self.description,
        }


@dataclass(frozen=True)
class ProjectProfile:
    languages: list[ProjectLanguage]
    language: ProjectLanguage
    package_manager: str | None
    framework: str | None
    test_frameworks: list[str]
    lint_tools: list[str]
    typecheck_tools: list[str]
    build_tools: list[str]
    workspace_kind: WorkspaceKind
    available_commands: list[DiscoveredCommand] = field(default_factory=list)
    evidence_files: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "languages": [language.value for language in self.languages],
            "language": self.language.value,
            "package_manager": self.package_manager,
            "framework": self.framework,
            "test_frameworks": self.test_frameworks,
            "lint_tools": self.lint_tools,
            "typecheck_tools": self.typecheck_tools,
            "build_tools": self.build_tools,
            "workspace_kind": self.workspace_kind.value,
            "available_commands": [
                command.to_dict() for command in self.available_commands
            ],
            "evidence_files": self.evidence_files,
        }


@dataclass(frozen=True)
class ImpactAnalysis:
    changed_files: list[str]
    affected_modules: list[str]
    likely_tests: list[str]
    requires_full_test: bool
    requires_build: bool
    requires_typecheck: bool
    requires_manual_review: bool
    risk_reasons: list[str]
    risk_level: str
    transaction_id: str | None = None
    changeset_id: str | None = None
    affected_symbols: list[str] = field(default_factory=list)
    dependent_files: list[str] = field(default_factory=list)
    test_mappings: list[dict[str, Any]] = field(default_factory=list)
    mapping_confidence: float = 0.0
    index_source: str | None = None
    index_stale: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "changed_files": self.changed_files,
            "affected_modules": self.affected_modules,
            "likely_tests": self.likely_tests,
            "requires_full_test": self.requires_full_test,
            "requires_build": self.requires_build,
            "requires_typecheck": self.requires_typecheck,
            "requires_manual_review": self.requires_manual_review,
            "risk_reasons": self.risk_reasons,
            "risk_level": self.risk_level,
            "transaction_id": self.transaction_id,
            "changeset_id": self.changeset_id,
            "affected_symbols": self.affected_symbols,
            "dependent_files": self.dependent_files,
            "test_mappings": self.test_mappings,
            "mapping_confidence": self.mapping_confidence,
            "index_source": self.index_source,
            "index_stale": self.index_stale,
        }


@dataclass
class VerificationCheck:
    kind: CheckKind
    command: CommandRequest | None
    scope: str
    required: bool
    timeout: float
    risk_tags: list[str]
    failure_policy: str
    id: str = field(default_factory=lambda: f"check_{uuid4().hex[:12]}")
    policy_decision: VerificationDecision | None = None
    policy_reasons: list[str] = field(default_factory=list)
    skip_reason: str | None = None
    source: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "kind": self.kind.value,
            "command": self.command.redacted_display_command() if self.command else None,
            "command_hash": self.command.command_hash() if self.command else None,
            "argv": self.command.redacted_argv() if self.command else None,
            "shell": self.command.redacted_shell() if self.command else None,
            "cwd": self.command.cwd if self.command else None,
            "purpose": self.command.purpose.value if self.command else None,
            "scope": self.scope,
            "required": self.required,
            "timeout": self.timeout,
            "risk_tags": self.risk_tags,
            "failure_policy": self.failure_policy,
            "policy_decision": (
                self.policy_decision.value if self.policy_decision else None
            ),
            "policy_reasons": self.policy_reasons,
            "skip_reason": self.skip_reason,
            "source": self.source,
        }


@dataclass
class VerificationPlan:
    project_profile: ProjectProfile
    impact_analysis: ImpactAnalysis
    required_checks: list[VerificationCheck]
    optional_checks: list[VerificationCheck]
    skipped_checks: list[VerificationCheck]
    blocked_checks: list[VerificationCheck]
    id: str = field(default_factory=lambda: f"vplan_{uuid4().hex[:12]}")
    transaction_id: str | None = None
    changeset_id: str | None = None

    def all_checks(self) -> list[VerificationCheck]:
        return [
            *self.required_checks,
            *self.optional_checks,
            *self.skipped_checks,
            *self.blocked_checks,
        ]

    def executable_checks(self) -> list[VerificationCheck]:
        return [*self.required_checks, *self.optional_checks]

    def to_dict(self) -> dict[str, Any]:
        return {
            "verification_plan_id": self.id,
            "transaction_id": self.transaction_id,
            "changeset_id": self.changeset_id,
            "project_profile": self.project_profile.to_dict(),
            "impact_analysis": self.impact_analysis.to_dict(),
            "required_checks": [check.to_dict() for check in self.required_checks],
            "optional_checks": [check.to_dict() for check in self.optional_checks],
            "skipped_checks": [check.to_dict() for check in self.skipped_checks],
            "blocked_checks": [check.to_dict() for check in self.blocked_checks],
        }


@dataclass(frozen=True)
class ParsedFailure:
    file: str | None
    line: int | None
    symbol: str | None
    test_name: str | None
    message: str
    stack_excerpt: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "file": self.file,
            "line": self.line,
            "symbol": self.symbol,
            "test_name": self.test_name,
            "message": self.message,
            "stack_excerpt": self.stack_excerpt,
        }


@dataclass(frozen=True)
class RepairHint:
    target_file: str | None
    line: int | None
    test_name: str | None
    message: str
    next_action: str
    confidence: float = 0.6

    def to_dict(self) -> dict[str, Any]:
        return {
            "target_file": self.target_file,
            "line": self.line,
            "test_name": self.test_name,
            "message": self.message,
            "next_action": self.next_action,
            "confidence": self.confidence,
        }


@dataclass(frozen=True)
class VerificationEvidence:
    command_id: str | None
    command: str | None
    exit_code: int | None
    output_excerpt: str
    artifact_path: str | None
    parsed_failures: list[ParsedFailure]
    duration_ms: int
    timestamp: str
    stdout_excerpt: str = ""
    stderr_excerpt: str = ""
    sandbox_id: str | None = None
    sandbox_backend: str | None = None
    sandbox_status: str | None = None
    sandbox_artifacts: list[dict[str, Any]] = field(default_factory=list)
    sandbox_changed_files: dict[str, Any] = field(default_factory=dict)
    sandbox_violations: list[dict[str, Any]] = field(default_factory=list)
    capability_summary: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "command_id": self.command_id,
            "command": self.command,
            "exit_code": self.exit_code,
            "output_excerpt": self.output_excerpt,
            "stdout_excerpt": self.stdout_excerpt,
            "stderr_excerpt": self.stderr_excerpt,
            "artifact_ref": self.artifact_path,
            "artifact_path": self.artifact_path,
            "parsed_failures": [
                failure.to_dict() for failure in self.parsed_failures
            ],
            "duration_ms": self.duration_ms,
            "timestamp": self.timestamp,
            "sandbox_id": self.sandbox_id,
            "sandbox_backend": self.sandbox_backend,
            "sandbox_status": self.sandbox_status,
            "sandbox_artifacts": self.sandbox_artifacts,
            "sandbox_changed_files": self.sandbox_changed_files,
            "sandbox_violations": self.sandbox_violations,
            "capability_summary": self.capability_summary,
        }


@dataclass(frozen=True)
class VerificationResult:
    check_id: str
    kind: CheckKind
    status: CheckStatus
    failure_type: FailureType | None
    evidence: VerificationEvidence
    repair_hints: list[RepairHint]
    confidence_impact: float
    duration_ms: int
    attempts: list[VerificationEvidence] = field(default_factory=list)
    policy_decision: CommandPolicyResult | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "check_id": self.check_id,
            "kind": self.kind.value,
            "status": self.status.value,
            "failure_type": self.failure_type.value if self.failure_type else None,
            "evidence": self.evidence.to_dict(),
            "repair_hints": [hint.to_dict() for hint in self.repair_hints],
            "confidence_impact": self.confidence_impact,
            "duration_ms": self.duration_ms,
            "attempts": [attempt.to_dict() for attempt in self.attempts],
            "policy_decision": (
                self.policy_decision.to_dict() if self.policy_decision else None
            ),
        }


@dataclass(frozen=True)
class RepairBudget:
    max_iterations: int = 3
    max_total_commands: int = 20
    max_total_time_seconds: int = 600
    max_same_failure_retries: int = 2
    stop_on_new_high_risk_change: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "max_iterations": self.max_iterations,
            "max_total_commands": self.max_total_commands,
            "max_total_time_seconds": self.max_total_time_seconds,
            "max_same_failure_retries": self.max_same_failure_retries,
            "stop_on_new_high_risk_change": self.stop_on_new_high_risk_change,
        }


@dataclass
class RepairLoopState:
    budget: RepairBudget = field(default_factory=RepairBudget)
    iterations: int = 0
    total_commands: int = 0
    total_time_seconds: float = 0.0
    failure_fingerprints: dict[str, int] = field(default_factory=dict)
    blocked_reason: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "budget": self.budget.to_dict(),
            "iterations": self.iterations,
            "total_commands": self.total_commands,
            "total_time_seconds": self.total_time_seconds,
            "failure_fingerprints": self.failure_fingerprints,
            "blocked_reason": self.blocked_reason,
        }


@dataclass(frozen=True)
class CompletionAssessment:
    status: CompletionStatus
    confidence: float
    passed_checks: list[str]
    failed_checks: list[str]
    skipped_checks: list[str]
    warnings: list[str]
    remaining_risks: list[str]

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status.value,
            "confidence": self.confidence,
            "passed_checks": self.passed_checks,
            "failed_checks": self.failed_checks,
            "skipped_checks": self.skipped_checks,
            "warnings": self.warnings,
            "remaining_risks": self.remaining_risks,
        }
