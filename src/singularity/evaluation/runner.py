from __future__ import annotations

import difflib
import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from uuid import uuid4

from rich.console import Console

from singularity.config import ProductionConfig, adaptive_default_max_turns
from singularity.context.redaction import ContextRedactor
from singularity.evaluation.models import (
    BenchmarkAdapterKind,
    BenchmarkTask,
    BenchmarkVisibility,
    ExpectedOutcomeKind,
    WorkspaceSnapshotKind,
)
from singularity.evaluation.store import GoldenTaskStore
from singularity.interaction import InteractionMode
from singularity.observability.redaction import TraceRedactor
from singularity.policy.permissions import ApprovalPolicy, NetworkAccess, PermissionProfileName

EVALUATION_TASK_SET_SCHEMA_VERSION = "evaluation.task_set/v1"
EVALUATION_RESULT_SCHEMA_VERSION = "evaluation.result/v1"
EVALUATION_METRICS_SCHEMA_VERSION = "evaluation.metrics/v1"

_PATCH_REDACTOR = ContextRedactor()
_MIMO_PRICING_SOURCE_URL = "https://platform.xiaomimimo.com/docs/pricing"
_MIMO_PRICING_RETRIEVED_AT = "2026-07-01"
_TOKEN_PRICING_PER_1M: dict[str, dict[str, Any]] = {
    "mimo-v2.5": {
        "input": 0.14,
        "cached_input": 0.0028,
        "output": 0.28,
        "currency": "USD",
        "source_url": _MIMO_PRICING_SOURCE_URL,
        "retrieved_at": _MIMO_PRICING_RETRIEVED_AT,
    }
}


class EvaluationSetupError(RuntimeError):
    def __init__(self, message: str, *, environment_blocker: bool) -> None:
        super().__init__(message)
        self.environment_blocker = environment_blocker


@dataclass(frozen=True)
class EvaluationWorkspace:
    kind: str
    path: str | None = None
    files: dict[str, str] = field(default_factory=dict)
    start_commit: str | None = None

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> EvaluationWorkspace:
        kind = str(payload.get("type") or payload.get("kind") or "").strip()
        start_commit = payload.get("start_commit")
        if kind in {"fixture", "fixture_workspace", "inline_files"}:
            files = payload.get("files") or payload.get("inline_files") or {}
            if not isinstance(files, dict) or not files:
                raise ValueError("evaluation fixture workspace requires files.")
            return cls(kind="fixture", files={str(key): str(value) for key, value in files.items()})
        if kind in {"repo", "path"}:
            path = str(payload.get("path") or "").strip()
            if not path:
                raise ValueError("evaluation repo workspace requires path.")
            return cls(kind="repo", path=path, start_commit=str(start_commit) if start_commit else None)
        raise ValueError(f"Unsupported evaluation workspace type: {kind}")

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {"type": self.kind}
        if self.path:
            payload["path"] = self.path
        if self.files:
            payload["files"] = dict(sorted(self.files.items()))
        if self.start_commit:
            payload["start_commit"] = self.start_commit
        return payload


@dataclass(frozen=True)
class EvaluationTask:
    task_id: str
    workspace: EvaluationWorkspace
    user_task: str
    allowed_paths: list[str]
    verification_command: str
    success: dict[str, Any]
    task_type: str = ""
    description: str = ""
    allowed_tools: list[str] = field(default_factory=list)
    tool_policy: str = "read_write"
    strategy: dict[str, Any] = field(default_factory=dict)
    expected_file_changes: list[str] = field(default_factory=list)
    completion_standard: str = ""
    risk_tags: list[str] = field(default_factory=list)
    prepare_commands: list[str] = field(default_factory=list)
    public_verification_command: str = ""
    hidden_verification_command: str = ""
    verification_prepare_commands: list[str] = field(default_factory=list)
    verification_timeout_seconds: int = 120
    model_visible_verification_command: str = ""
    fixture_metadata: dict[str, Any] = field(default_factory=dict)
    hidden_test_patch: dict[str, Any] = field(default_factory=dict)
    test_patch: str = ""

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> EvaluationTask:
        workspace_payload = _workspace_payload(payload)
        prepare_commands = payload.get("prepare_commands")
        if prepare_commands is None:
            single = payload.get("prepare_command")
            prepare_commands = [single] if single else []
        if not isinstance(prepare_commands, list):
            raise ValueError("evaluation prepare_commands must be a list.")
        verification_prepare_commands = payload.get("verification_prepare_commands") or []
        if not isinstance(verification_prepare_commands, list):
            raise ValueError("evaluation verification_prepare_commands must be a list.")
        task = cls(
            task_id=str(payload.get("task_id") or "").strip(),
            workspace=EvaluationWorkspace.from_dict(workspace_payload),
            user_task=str(payload.get("user_task") or payload.get("prompt") or "").strip(),
            allowed_paths=[str(item) for item in payload.get("allowed_paths") or []],
            verification_command=str(payload.get("verification_command") or "").strip(),
            success=_dict(payload.get("success"), "success"),
            task_type=str(payload.get("task_type") or "").strip(),
            description=str(payload.get("description") or "").strip(),
            allowed_tools=[str(item) for item in payload.get("allowed_tools") or []],
            tool_policy=str(payload.get("tool_policy") or "read_write").strip(),
            strategy=_dict(payload.get("strategy") or {}, "strategy"),
            expected_file_changes=[
                str(item) for item in payload.get("expected_file_changes") or []
            ],
            completion_standard=str(payload.get("completion_standard") or "").strip(),
            risk_tags=[str(item) for item in payload.get("risk_tags") or []],
            prepare_commands=[str(item) for item in prepare_commands if str(item).strip()],
            public_verification_command=str(payload.get("public_verification_command") or "").strip(),
            hidden_verification_command=str(payload.get("hidden_verification_command") or "").strip(),
            verification_prepare_commands=[str(item) for item in verification_prepare_commands if str(item).strip()],
            verification_timeout_seconds=int(payload.get("verification_timeout_seconds") or 120),
            model_visible_verification_command=str(payload.get("model_visible_verification_command") or "").strip(),
            fixture_metadata=_dict(payload.get("fixture_metadata") or {}, "fixture_metadata"),
            hidden_test_patch=_dict(payload.get("hidden_test_patch") or {}, "hidden_test_patch"),
            test_patch=str(payload.get("test_patch") or ""),
        )
        task._validate()
        return task

    def _validate(self) -> None:
        if not self.task_id:
            raise ValueError("evaluation task requires task_id.")
        if not self.user_task:
            raise ValueError(f"evaluation task {self.task_id} requires user_task.")
        if not self.allowed_paths:
            raise ValueError(f"evaluation task {self.task_id} requires allowed_paths.")
        if not self.verification_command:
            raise ValueError(f"evaluation task {self.task_id} requires verification_command.")
        if not self.success:
            raise ValueError(f"evaluation task {self.task_id} requires success.")
        if self.tool_policy not in {"read_write", "read_only", "review_all", "non_interactive"}:
            raise ValueError(f"evaluation task {self.task_id} has unsupported tool_policy.")
        removed = {"approval_mode", "security_mode"} & set(self.strategy)
        if removed:
            names = ", ".join(sorted(removed))
            raise ValueError(f"evaluation task {self.task_id} uses removed strategy fields: {names}.")
        _permission_profile_for_task(self)
        _approval_policy_for_task(self)
        _network_access_for_task(self)
        _strategy_max_turns_for_task(self)
        if self.workspace.kind == "repo" and not self.workspace.start_commit and not self.prepare_commands:
            raise ValueError(f"evaluation repo task {self.task_id} requires start_commit or prepare_command.")

    def to_dict(self) -> dict[str, Any]:
        payload = {
            "task_id": self.task_id,
            "workspace": self.workspace.to_dict(),
            "user_task": self.user_task,
            "allowed_paths": list(self.allowed_paths),
            "verification_command": self.verification_command,
            "success": dict(self.success),
            "task_type": self.task_type,
            "description": self.description,
            "allowed_tools": list(self.allowed_tools),
            "tool_policy": self.tool_policy,
            "strategy": dict(self.strategy),
            "expected_file_changes": list(self.expected_file_changes),
            "completion_standard": self.completion_standard,
            "risk_tags": list(self.risk_tags),
            "verification_timeout_seconds": self.verification_timeout_seconds,
        }
        if self.prepare_commands:
            payload["prepare_commands"] = list(self.prepare_commands)
        if self.public_verification_command:
            payload["public_verification_command"] = self.public_verification_command
        if self.hidden_verification_command:
            payload["hidden_verification_command"] = self.hidden_verification_command
        if self.verification_prepare_commands:
            payload["verification_prepare_commands"] = list(self.verification_prepare_commands)
        if self.model_visible_verification_command:
            payload["model_visible_verification_command"] = self.model_visible_verification_command
        if self.fixture_metadata:
            payload["fixture_metadata"] = dict(self.fixture_metadata)
        if self.hidden_test_patch:
            payload["hidden_test_patch"] = dict(self.hidden_test_patch)
        if self.test_patch:
            payload["test_patch"] = self.test_patch
        return payload


@dataclass(frozen=True)
class EvaluationTaskSet:
    tasks: list[EvaluationTask]
    base_dir: Path
    schema_version: str = EVALUATION_TASK_SET_SCHEMA_VERSION

    @classmethod
    def from_dict(cls, payload: dict[str, Any], *, base_dir: Path) -> EvaluationTaskSet:
        schema_version = str(payload.get("schema_version") or "")
        if schema_version != EVALUATION_TASK_SET_SCHEMA_VERSION:
            raise ValueError(f"Unsupported evaluation schema_version: {schema_version}")
        tasks_payload = payload.get("tasks")
        if not isinstance(tasks_payload, list) or not tasks_payload:
            raise ValueError("evaluation manifest requires tasks.")
        return cls(tasks=[EvaluationTask.from_dict(item) for item in tasks_payload], base_dir=base_dir)

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "tasks": [task.to_dict() for task in self.tasks],
        }


@dataclass(frozen=True)
class CommandEvalResult:
    command: str
    exit_code: int | None
    duration_seconds: float
    timed_out: bool = False
    error_summary: str = ""
    raw_command: str = ""
    resolved_argv: list[str] = field(default_factory=list)
    interpreter_strategy: dict[str, Any] = field(default_factory=dict)
    failure_category: str = ""

    @property
    def passed(self) -> bool:
        return self.exit_code == 0 and not self.timed_out

    def to_dict(self) -> dict[str, Any]:
        return {
            "command": self.command,
            "exit_code": self.exit_code,
            "duration_seconds": self.duration_seconds,
            "timed_out": self.timed_out,
            "passed": self.passed,
            "error_summary": self.error_summary,
            "raw_command": self.raw_command or self.command,
            "resolved_argv": list(self.resolved_argv),
            "interpreter_strategy": dict(self.interpreter_strategy),
            "failure_category": self.failure_category,
        }


@dataclass(frozen=True)
class EvaluationTaskResult:
    task_id: str
    tests_passed: bool
    infrastructure_blocked: bool
    prompt_tokens: int
    cached_tokens: int
    request_cache_hit_rate: float
    run_cache_hit_rate: float
    tool_calls: int
    files_changed: list[str]
    duration_seconds: float
    error_summary: str
    workspace: str
    trace: str
    verification_workspace: str = ""
    patch: dict[str, Any] = field(default_factory=dict)
    checks: dict[str, Any] = field(default_factory=dict)
    verification: CommandEvalResult | None = None
    agent_completed: bool = False
    evaluation_passed: bool = False
    patch_applicable: bool = False
    allowed_scope_passed: bool = False
    public_verification_passed: bool = False
    hidden_verification_passed: bool = False
    sandbox_enforcement_passed: bool = True
    evaluator_visibility_audit_passed: bool = True
    local_process_fallback_count: int = 0
    repair_attempt_count: int = 0
    repair_execution_count: int = 0
    miscompletion_count: int = 0
    blocked_reason: str = ""
    failure_category: str = ""
    request_cache_hit_rates: dict[str, float] = field(default_factory=dict)
    status: str = "unknown"
    turn_count: int = 0
    verification_result: dict[str, Any] = field(default_factory=dict)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)
    final_report_status: str = ""
    policy_blocks: int = 0
    token_usage: dict[str, Any] = field(default_factory=dict)
    cache_usage: dict[str, Any] = field(default_factory=dict)
    trace_artifact_refs: list[str] = field(default_factory=list)
    reproducible_environment: dict[str, Any] = field(default_factory=dict)
    capability_summary: dict[str, Any] = field(default_factory=dict)
    capability_sla: dict[str, Any] = field(default_factory=dict)
    timing: dict[str, Any] = field(default_factory=dict)
    baseline_failed: bool = False
    baseline_checks: dict[str, Any] = field(default_factory=dict)
    patch_applied: bool = False
    fail_to_pass_satisfied: bool = False
    verification_misconfiguration_reason: str = ""
    evaluation_metrics: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "status": self.status,
            "tests_passed": self.tests_passed,
            "infrastructure_blocked": self.infrastructure_blocked,
            "turn_count": self.turn_count,
            "prompt_tokens": self.prompt_tokens,
            "cached_tokens": self.cached_tokens,
            "request_cache_hit_rate": self.request_cache_hit_rate,
            "run_cache_hit_rate": self.run_cache_hit_rate,
            "tool_calls": self.tool_calls,
            "files_changed": list(self.files_changed),
            "duration_seconds": self.duration_seconds,
            "error_summary": self.error_summary,
            "workspace": self.workspace,
            "trace": self.trace,
            "verification_workspace": self.verification_workspace,
            "patch": self.patch,
            "checks": self.checks,
            "verification": self.verification.to_dict() if self.verification else None,
            "agent_completed": self.agent_completed,
            "evaluation_passed": self.evaluation_passed,
            "patch_applicable": self.patch_applicable,
            "allowed_scope_passed": self.allowed_scope_passed,
            "public_verification_passed": self.public_verification_passed,
            "hidden_verification_passed": self.hidden_verification_passed,
            "sandbox_enforcement_passed": self.sandbox_enforcement_passed,
            "evaluator_visibility_audit_passed": self.evaluator_visibility_audit_passed,
            "local_process_fallback_count": self.local_process_fallback_count,
            "repair_attempt_count": self.repair_attempt_count,
            "repair_execution_count": self.repair_execution_count,
            "miscompletion_count": self.miscompletion_count,
            "blocked_reason": self.blocked_reason,
            "failure_category": self.failure_category,
            "request_cache_hit_rates": dict(sorted(self.request_cache_hit_rates.items())),
            "verification_result": self.verification_result,
            "contract_satisfaction": self.contract_satisfaction,
            "final_report_status": self.final_report_status,
            "policy_blocks": self.policy_blocks,
            "token_usage": self.token_usage,
            "cache_usage": self.cache_usage,
            "trace_artifact_refs": list(self.trace_artifact_refs),
            "reproducible_environment": self.reproducible_environment,
            "capability_summary": self.capability_summary,
            "capability_sla": self.capability_sla,
            "timing": self.timing,
            "baseline_failed": self.baseline_failed,
            "baseline_checks": self.baseline_checks,
            "patch_applied": self.patch_applied,
            "fail_to_pass_satisfied": self.fail_to_pass_satisfied,
            "verification_misconfiguration_reason": self.verification_misconfiguration_reason,
            "evaluation_metrics": self.evaluation_metrics,
        }


class SingularityPrivateBenchmarkAdapter:
    def load(self, path: Path | str) -> EvaluationTaskSet:
        task_path = Path(path)
        tasks = [
            self._convert(task)
            for task in GoldenTaskStore(task_path).load()
            if task.adapter == BenchmarkAdapterKind.SINGULARITY_PRIVATE
            and task.visibility == BenchmarkVisibility.PRIVATE
        ]
        if not tasks:
            raise ValueError("No private Singularity benchmark tasks found.")
        return EvaluationTaskSet(tasks=tasks, base_dir=task_path.parent.resolve(strict=False))

    def _convert(self, task: BenchmarkTask) -> EvaluationTask:
        command = _first_test_command(task)
        if not command:
            raise ValueError(f"Private benchmark task {task.task_id} requires a test expected_outcome.")
        metadata = dict(task.input.metadata)
        if task.workspace_snapshot.kind == WorkspaceSnapshotKind.INLINE_FILES:
            workspace = EvaluationWorkspace(kind="fixture", files=dict(task.workspace_snapshot.inline_files))
        elif task.workspace_snapshot.kind == WorkspaceSnapshotKind.GIT_REF:
            repo_path = metadata.get("repo_path") or metadata.get("repo")
            if not repo_path:
                raise ValueError(
                    f"Private git_ref benchmark task {task.task_id} requires input.metadata.repo_path."
                )
            workspace = EvaluationWorkspace(
                kind="repo",
                path=str(repo_path),
                start_commit=task.workspace_snapshot.git_ref,
            )
        else:
            raise ValueError(
                f"Private benchmark task {task.task_id} uses unsupported snapshot kind: "
                f"{task.workspace_snapshot.kind.value}"
            )
        return EvaluationTask(
            task_id=task.task_id,
            workspace=workspace,
            user_task=task.input.prompt,
            allowed_paths=_allowed_paths_for_task(task, metadata),
            verification_command=command,
            success={"type": "verification_exit_code", "exit_code": 0},
            description=task.description,
            allowed_tools=list(task.allowed_tools)
            or [str(item) for item in metadata.get("allowed_tools_config") or []],
            tool_policy=str(task.strategy.get("tool_policy") or metadata.get("tool_policy") or "read_write"),
            strategy=dict(task.strategy),
            expected_file_changes=list(task.expected_file_changes),
            completion_standard=task.completion_standard,
            risk_tags=list(task.risk_tags),
            verification_prepare_commands=[
                str(command) for command in metadata.get("verification_prepare_commands") or []
            ],
        )


class EvaluationRunner:
    def __init__(
        self,
        *,
        output_root: Path | str | None = None,
        run_id: str | None = None,
        max_turns: int | None = None,
        model: str | None = None,
        base_url: str | None = None,
        baseline_result_path: Path | str | None = None,
        env_root: Path | str | None = None,
        bootstrap_cls: Any | None = None,
        console: Console | None = None,
    ) -> None:
        self.output_root = Path(output_root or Path.cwd() / "work" / "evaluations").resolve(strict=False)
        self.run_id = run_id or f"eval_{uuid4().hex[:8]}"
        self.max_turns = max_turns
        self.model = model
        self.base_url = base_url
        self.baseline_result_path = Path(baseline_result_path) if baseline_result_path else None
        self.env_root = (
            Path(env_root).expanduser().resolve(strict=False)
            if env_root is not None
            else Path.cwd().resolve(strict=False)
        )
        if bootstrap_cls is None:
            from singularity.kernel import KernelBootstrap

            bootstrap_cls = KernelBootstrap
        self.bootstrap_cls = bootstrap_cls
        self.console = console or Console()
        self.redactor = TraceRedactor()

    @property
    def run_dir(self) -> Path:
        return self.output_root / self.run_id

    def run(self, manifest: EvaluationTaskSet) -> dict[str, Any]:
        started = time.perf_counter()
        self.run_dir.mkdir(parents=True, exist_ok=True)
        results = [self.run_task(task, manifest_base=manifest.base_dir) for task in manifest.tasks]
        previous = _previous_evaluation_result(
            self.output_root,
            current_run_id=self.run_id,
            baseline_result_path=self.baseline_result_path,
        )
        payload = {
            "schema_version": EVALUATION_RESULT_SCHEMA_VERSION,
            "run_id": self.run_id,
            "output_dir": str(self.run_dir),
            "summary": summarize_evaluation_results(results),
            "tasks": [result.to_dict() for result in results],
            "duration_seconds": round(time.perf_counter() - started, 3),
        }
        if previous:
            payload["regression"] = compare_evaluation_results(previous, payload)
        result_path = self.run_dir / "result.json"
        result_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        report_path = self.run_dir / "report.json"
        markdown_path = self.run_dir / "report.md"
        report_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        markdown_path.write_text(evaluation_report_markdown(payload), encoding="utf-8")
        regression = payload.get("regression")
        regression_artifact_path: Path | None = None
        if isinstance(regression, dict):
            regression_path = self.run_dir / "regression.json"
            regression_artifact_path = regression_path
            regression_md_path = self.run_dir / "regression.md"
            regression_path.write_text(
                json.dumps(regression, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            regression_md_path.write_text(
                evaluation_regression_markdown(regression),
                encoding="utf-8",
            )
            payload["regression_path"] = str(regression_path)
            payload["regression_markdown_path"] = str(regression_md_path)
        from singularity.evaluation.failure_case_replay import FailureCaseReplayRunner

        failure_cases_path = self.run_dir / "failure_cases.json"
        failure_cases = FailureCaseReplayRunner(
            report_path=report_path,
            regression_path=regression_artifact_path,
        ).write(failure_cases_path)
        payload["failure_cases_path"] = str(failure_cases_path)
        payload["failure_case_count"] = len(failure_cases)
        payload["result_path"] = str(result_path)
        payload["report_path"] = str(report_path)
        payload["markdown_path"] = str(markdown_path)
        result_path.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        report_path.write_text(
            json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        markdown_path.write_text(evaluation_report_markdown(payload), encoding="utf-8")
        return payload

    def run_task(self, task: EvaluationTask, *, manifest_base: Path) -> EvaluationTaskResult:
        started = time.perf_counter()
        evaluation_timing: dict[str, Any] = {}
        task_dir = self.run_dir / _safe_name(task.task_id)
        workspace = task_dir / "workspace"
        trace_path = ""
        verification: CommandEvalResult | None = None
        public_verification: CommandEvalResult | None = None
        hidden_verification: CommandEvalResult | None = None
        files_changed: list[str] = []
        errors: list[str] = []
        usage: dict[str, Any] = {}
        tool_calls = 0
        success_ok = False
        tests_passed = False
        kernel = None
        before_snapshot: dict[str, str] = {}
        before_text_snapshot: dict[str, str] = {}
        baseline_workspace = task_dir / "baseline-workspace"
        verification_workspace = task_dir / "verification-workspace"
        patch_payload: dict[str, Any] = {}
        checks: dict[str, Any] = {}
        agent_status = ""
        final_report_payload: dict[str, Any] = {}
        trace_summary: dict[str, Any] = {}
        turn_count = 0
        policy_blocks = 0
        trace_artifact_refs: list[str] = []
        contract_satisfaction: dict[str, Any] = {}
        reproducible_environment: dict[str, Any] = {}
        baseline_checks: dict[str, Any] = {}
        baseline_failed = False
        baseline_verification_workspace = task_dir / "baseline-verification-workspace"
        patch_applied = False
        fail_to_pass_satisfied = False
        verification_misconfiguration_reason = ""
        try:
            phase_started = time.perf_counter()
            _reset_dir(task_dir, root=self.run_dir)
            evaluation_timing["run_root_reset_time_seconds"] = time.perf_counter() - phase_started
            try:
                phase_started = time.perf_counter()
                self._materialize_workspace(
                    task,
                    workspace=workspace,
                    manifest_base=manifest_base,
                    timing=evaluation_timing,
                )
                evaluation_timing["workspace_materialization_time_seconds"] = (
                    time.perf_counter() - phase_started
                )
            except EvaluationSetupError as exc:
                evaluation_timing["workspace_materialization_time_seconds"] = (
                    time.perf_counter() - phase_started
                )
                errors.append(str(exc))
                return self._task_result(
                    task=task,
                    workspace=workspace,
                    trace=trace_path,
                    started=started,
                    verification=None,
                    verification_workspace=verification_workspace,
                    files_changed=[],
                    usage={},
                    tool_calls=0,
                    errors=errors,
                    patch={},
                    checks=_checks_payload(None, None),
                    success=False,
                    tests_passed=False,
                    infrastructure_blocked=exc.environment_blocker,
                    agent_status="",
                    final_report_payload={},
                    trace_summary={},
                    turn_count=0,
                    policy_blocks=0,
                    trace_artifact_refs=[],
                    contract_satisfaction={},
                    reproducible_environment=_setup_environment(task, manifest_base=manifest_base),
                    baseline_checks={},
                    evaluation_timing=evaluation_timing,
                )
            config = ProductionConfig.from_cli(
                project_root=workspace,
                max_turns=self.max_turns or _strategy_max_turns_for_task(task) or adaptive_default_max_turns(task.user_task),
                model=self.model,
                base_url=self.base_url,
                env_root=self.env_root,
                permission_profile=_permission_profile_for_task(task),
                approval_policy=_approval_policy_for_task(task),
                network_access=_network_access_for_task(task),
                interaction_mode=InteractionMode.NON_INTERACTIVE,
                raw_artifacts=False,
                profile=f"evaluation:{task.task_id}:{task.tool_policy}",
                cli_overrides={
                    "max_turns",
                    "model",
                    "base_url",
                    "permission_profile",
                    "approval_policy",
                    "network_access",
                    "interaction_mode",
                    "raw_artifacts",
                    "profile",
                },
            )
            reproducible_environment = _reproducible_environment(
                task,
                workspace=workspace,
                manifest_base=manifest_base,
                output_root=self.output_root,
                baseline_result_path=self.baseline_result_path,
                config=config,
                max_turns=config.max_turns,
            )
            dependency_setup_started = time.perf_counter()
            dependency_cache_env, dependency_cache = _dependency_setup_cache(
                task,
                workspace=workspace,
                output_root=self.output_root,
            )
            reproducible_environment["dependency_setup_cache"] = dependency_cache
            if dependency_cache.get("hit") is not True:
                for command in task.prepare_commands:
                    prepared = _run_shell(
                        command,
                        cwd=workspace,
                        timeout_seconds=120,
                        redactor=self.redactor,
                        env_overrides=dependency_cache_env,
                    )
                    if not prepared.passed:
                        errors.append(f"prepare failed: {prepared.error_summary or command}")
                        return self._task_result(
                            task=task,
                            workspace=workspace,
                            trace=trace_path,
                            started=started,
                            verification=prepared,
                            verification_workspace=verification_workspace,
                            files_changed=[],
                            usage={},
                            tool_calls=0,
                            errors=errors,
                            patch={},
                            checks=_checks_payload(None, prepared),
                            success=False,
                            tests_passed=False,
                            infrastructure_blocked=False,
                            reproducible_environment=reproducible_environment,
                            evaluation_timing={
                                **evaluation_timing,
                                "dependency_setup_time_seconds": time.perf_counter()
                                - dependency_setup_started,
                            },
                        )
                _finalize_dependency_setup_cache(
                    dependency_cache,
                    workspace=workspace,
                )
            evaluation_timing["dependency_setup_time_seconds"] = (
                time.perf_counter() - dependency_setup_started
            )
            before_snapshot = _snapshot_files(workspace)
            before_text_snapshot = _read_text_files(workspace)
            phase_started = time.perf_counter()
            shutil.copytree(workspace, baseline_workspace, ignore=_copy_ignore)
            evaluation_timing["baseline_workspace_copy_time_seconds"] = (
                time.perf_counter() - phase_started
            )
            if _requires_baseline_verification(task):
                phase_started = time.perf_counter()
                baseline_verification = _run_baseline_verification(
                    task,
                    baseline_workspace=baseline_workspace,
                    baseline_verification_workspace=baseline_verification_workspace,
                    root=task_dir,
                    redactor=self.redactor,
                )
                evaluation_timing["baseline_verification_time_seconds"] = (
                    time.perf_counter() - phase_started
                )
                baseline_checks = baseline_verification["checks"]
                baseline_failed = bool(baseline_verification["baseline_failed"])
                verification_misconfiguration_reason = str(
                    baseline_verification.get("verification_misconfiguration_reason") or ""
                )
                if baseline_verification["status"] == "baseline_already_passing":
                    errors.append("baseline already passing before agent changes")
                    return self._task_result(
                        task=task,
                        workspace=workspace,
                        trace=trace_path,
                        started=started,
                        verification=None,
                        verification_workspace=verification_workspace,
                        files_changed=[],
                        usage={},
                        tool_calls=0,
                        errors=errors,
                        patch={},
                        checks=_checks_payload(None, None),
                        success=False,
                        tests_passed=False,
                        infrastructure_blocked=False,
                        final_report_payload={},
                        trace_summary={},
                        turn_count=0,
                        policy_blocks=0,
                        trace_artifact_refs=[],
                        contract_satisfaction={},
                        reproducible_environment=reproducible_environment,
                        baseline_failed=False,
                        baseline_checks=baseline_checks,
                        status_override="invalid_public_task",
                        failure_category_override="baseline_already_passing",
                        evaluation_timing=evaluation_timing,
                    )
                if baseline_verification["status"] == "verification_misconfigured":
                    errors.append(f"verification misconfigured: {verification_misconfiguration_reason}")
                    return self._task_result(
                        task=task,
                        workspace=workspace,
                        trace=trace_path,
                        started=started,
                        verification=None,
                        verification_workspace=verification_workspace,
                        files_changed=[],
                        usage={},
                        tool_calls=0,
                        errors=errors,
                        patch={},
                        checks=_checks_payload(None, None),
                        success=False,
                        tests_passed=False,
                        infrastructure_blocked=False,
                        final_report_payload={},
                        trace_summary={},
                        turn_count=0,
                        policy_blocks=0,
                        trace_artifact_refs=[],
                        contract_satisfaction={},
                        reproducible_environment=reproducible_environment,
                        baseline_failed=False,
                        baseline_checks=baseline_checks,
                        verification_misconfiguration_reason=verification_misconfiguration_reason,
                        status_override="verification_misconfigured",
                        failure_category_override="verification_misconfigured",
                        evaluation_timing=evaluation_timing,
                    )
            goal = _task_goal(task)
            phase_started = time.perf_counter()
            kernel = self.bootstrap_cls(project_root=workspace, config=config, console=self.console).boot(goal)
            _apply_benchmark_constraints(kernel, task)
            agent_result = kernel.run_task(goal)
            evaluation_timing["agent_loop_time_seconds"] = time.perf_counter() - phase_started
            trace_path = _trace_path(kernel)
            trace_summary = _trace_summary(kernel, agent_result)
            final_report_payload = _final_report_payload(agent_result)
            usage = dict(trace_summary.get("model_usage_summary") or {})
            tool_calls = _safe_int(trace_summary.get("tool_calls")) or _safe_int(usage.get("tool_calls_proposed"))
            turn_count = _turn_count(agent_result, usage)
            policy_blocks = _policy_blocks(final_report_payload, trace_summary)
            trace_artifact_refs = _trace_artifact_refs(final_report_payload, trace_summary)
            agent_status = _agent_status(agent_result)
            environment_blocker_reason = ""
            if _infrastructure_blocked(agent_result, usage=usage, tool_calls=tool_calls):
                environment_blocker_reason = "model provider unavailable"
            elif _sandbox_environment_blocked(kernel, agent_status=agent_status):
                environment_blocker_reason = _sandbox_environment_blocker_reason(kernel)
            elif _final_report_environment_blocked(final_report_payload):
                environment_blocker_reason = _final_report_environment_blocker_reason(final_report_payload)
            if environment_blocker_reason:
                errors.append(f"environment blocker: {environment_blocker_reason}")
                files_changed = _changed_files(workspace, before_snapshot=before_snapshot)
                patch_payload = _patch_payload(before_text_snapshot, workspace)
                contract_satisfaction = _contract_satisfaction(
                    task,
                    files_changed=files_changed,
                    allowed_scope=True,
                    verification=None,
                    public_verification=None,
                    agent_status=agent_status,
                    final_report_status=_final_report_status(final_report_payload, agent_status=agent_status),
                    policy_blocks=policy_blocks,
                    patch=patch_payload,
                    final_report_payload=final_report_payload,
                )
                return self._task_result(
                    task=task,
                    workspace=workspace,
                    trace=trace_path,
                    started=started,
                    verification=None,
                    verification_workspace=verification_workspace,
                    files_changed=files_changed,
                    usage=usage,
                    tool_calls=tool_calls,
                    errors=errors,
                    patch=patch_payload,
                    checks=_checks_payload(None, None),
                    success=False,
                    tests_passed=False,
                    infrastructure_blocked=True,
                    agent_status=agent_status,
                    final_report_payload=final_report_payload,
                    trace_summary=trace_summary,
                    turn_count=turn_count,
                    policy_blocks=policy_blocks,
                    trace_artifact_refs=trace_artifact_refs,
                    contract_satisfaction=contract_satisfaction,
                    reproducible_environment=reproducible_environment,
                    baseline_failed=baseline_failed,
                    baseline_checks=baseline_checks,
                    evaluation_timing=evaluation_timing,
                )
            files_changed = _changed_files(workspace, before_snapshot=before_snapshot)
            patch_payload = _patch_payload(before_text_snapshot, workspace)
            phase_started = time.perf_counter()
            applicable = _prepare_verification_workspace(
                source_workspace=workspace,
                verification_workspace=verification_workspace,
                baseline_workspace=baseline_workspace,
                before_snapshot=before_text_snapshot,
                root=task_dir,
                test_patch=task.test_patch,
            )
            evaluation_timing["verification_workspace_copy_time_seconds"] = (
                time.perf_counter() - phase_started
            )
            patch_payload["applicable"] = applicable
            patch_applied = applicable
            public_command = _public_verification_command(task)
            hidden_command = _hidden_verification_command(task)
            if task.verification_prepare_commands:
                if public_command:
                    phase_started = time.perf_counter()
                    public_verification = _run_shell(
                        public_command,
                        cwd=verification_workspace,
                        timeout_seconds=task.verification_timeout_seconds,
                        redactor=self.redactor,
                    )
                    evaluation_timing["public_verification_time_seconds"] = (
                        time.perf_counter() - phase_started
                    )
                else:
                    public_verification = CommandEvalResult(
                        command="",
                        exit_code=0,
                        duration_seconds=0.0,
                        error_summary="hidden verifier only",
                        interpreter_strategy={
                            "schema_version": "evaluation.command_interpreter/v1",
                            "mode": "not_run",
                            "reason": "hidden_verifier_only",
                            "shell": False,
                            "harness_executable": sys.executable,
                        },
                    )
            else:
                phase_started = time.perf_counter()
                public_verification = _run_shell(
                    public_command,
                    cwd=verification_workspace,
                    timeout_seconds=task.verification_timeout_seconds,
                    redactor=self.redactor,
                )
                evaluation_timing["public_verification_time_seconds"] = (
                    time.perf_counter() - phase_started
                )
            verification_prepare_started = time.perf_counter()
            for command in task.verification_prepare_commands:
                prepared = _run_shell(command, cwd=verification_workspace, timeout_seconds=120, redactor=self.redactor)
                if not prepared.passed:
                    errors.append(f"verification prepare failed: {prepared.error_summary or command}")
                    checks = _checks_payload(public_verification, prepared)
                    allowed_ok = _allowed_scope_ok(files_changed, task.allowed_paths)
                    contract_satisfaction = _contract_satisfaction(
                        task,
                        files_changed=files_changed,
                        allowed_scope=allowed_ok,
                        verification=prepared,
                        public_verification=public_verification,
                        agent_status=agent_status,
                        final_report_status=_final_report_status(final_report_payload, agent_status=agent_status),
                        policy_blocks=policy_blocks,
                        patch=patch_payload,
                        final_report_payload=final_report_payload,
                    )
                    return self._task_result(
                        task=task,
                        workspace=workspace,
                        trace=trace_path,
                        started=started,
                        verification=prepared,
                        verification_workspace=verification_workspace,
                        files_changed=files_changed,
                        usage=usage,
                        tool_calls=tool_calls,
                        errors=errors,
                        patch=patch_payload,
                        checks=checks,
                        success=False,
                        tests_passed=False,
                        infrastructure_blocked=False,
                        agent_status=agent_status,
                        final_report_payload=final_report_payload,
                        trace_summary=trace_summary,
                        turn_count=turn_count,
                        policy_blocks=policy_blocks,
                        trace_artifact_refs=trace_artifact_refs,
                        contract_satisfaction=contract_satisfaction,
                        reproducible_environment=reproducible_environment,
                        public_verification=public_verification,
                        hidden_verification=prepared,
                        evaluation_timing=evaluation_timing,
                    )
            evaluation_timing["verification_prepare_time_seconds"] = (
                time.perf_counter() - verification_prepare_started
            )
            phase_started = time.perf_counter()
            hidden_verification = _run_shell(
                hidden_command,
                cwd=verification_workspace,
                timeout_seconds=task.verification_timeout_seconds,
                redactor=self.redactor,
            )
            evaluation_timing["hidden_verification_time_seconds"] = (
                time.perf_counter() - phase_started
            )
            verification = hidden_verification
            checks = _checks_payload(public_verification, hidden_verification)
            tests_passed = verification.passed
            allowed_ok = _allowed_scope_ok(files_changed, task.allowed_paths)
            criterion_ok = _success_criterion_ok(
                task.success,
                verification=verification,
                workspace=verification_workspace,
                agent_status=agent_status,
                policy_blocks=policy_blocks,
            )
            agent_completed = agent_status == "completed"
            if not agent_completed:
                errors.append(f"agent status: {getattr(agent_result.status, 'value', agent_result.status)}")
            if not tests_passed:
                errors.append(f"verification failed: {verification.error_summary or verification.command}")
            if not public_verification.passed:
                errors.append(f"public verification failed: {public_verification.error_summary or public_verification.command}")
            patch_ok = _patch_applicable_for_task(task, patch=patch_payload, files_changed=files_changed)
            if not patch_ok:
                if not files_changed and not agent_completed:
                    errors.append("agent blocked before target file changes")
                else:
                    errors.append("patch could not be applied to clean verification workspace")
            if not allowed_ok:
                errors.append("changed files outside allowed_paths")
            if not criterion_ok:
                errors.append("success criterion failed")
            fail_to_pass_satisfied = bool(baseline_failed and tests_passed and public_verification.passed)
            contract_satisfaction = _contract_satisfaction(
                task,
                files_changed=files_changed,
                allowed_scope=allowed_ok,
                verification=verification,
                public_verification=public_verification,
                agent_status=agent_status,
                final_report_status=_final_report_status(final_report_payload, agent_status=agent_status),
                policy_blocks=policy_blocks,
                patch=patch_payload,
                final_report_payload=final_report_payload,
            )
            success_ok = bool(
                agent_completed
                and (not _requires_baseline_verification(task) or baseline_failed)
                and tests_passed
                and public_verification.passed
                and patch_ok
                and allowed_ok
                and criterion_ok
            )
            if _expected_blocked_success(task.success):
                success_ok = bool(
                    agent_status in {"blocked", "failed"}
                    and tests_passed
                    and public_verification.passed
                    and patch_ok
                    and allowed_ok
                    and criterion_ok
                )
        except Exception as exc:
            errors.append(self.redactor.redact_text(str(exc)) or type(exc).__name__)
            if kernel is not None:
                trace_path = trace_path or _trace_path(kernel)
                trace_summary = trace_summary or _trace_summary_from_kernel(kernel)
                if not agent_status:
                    agent_status = _agent_status_from_trace(Path(trace_path) if trace_path else None)
                usage = usage or dict(trace_summary.get("model_usage_summary") or {})
                if not tool_calls:
                    tool_calls = _tool_calls_from_trace(Path(trace_path) if trace_path else None, trace_summary)
                if not turn_count:
                    turn_count = _turn_count_from_trace(Path(trace_path) if trace_path else None, usage)
        finally:
            close_resources = getattr(kernel, "close_resources", None) if kernel is not None else None
            if callable(close_resources):
                phase_started = time.perf_counter()
                close_resources()
                evaluation_timing["resource_cleanup_time_seconds"] = (
                    time.perf_counter() - phase_started
                )
        return self._task_result(
            task=task,
            workspace=workspace,
            trace=trace_path,
            started=started,
            verification=verification,
            verification_workspace=verification_workspace,
            files_changed=files_changed,
            usage=usage,
            tool_calls=tool_calls,
            errors=errors,
            patch=patch_payload,
            checks=checks,
            success=success_ok,
            tests_passed=tests_passed,
            infrastructure_blocked=False,
            agent_status=agent_status,
            final_report_payload=final_report_payload,
            trace_summary=trace_summary,
            turn_count=turn_count,
            policy_blocks=policy_blocks,
            trace_artifact_refs=trace_artifact_refs,
            contract_satisfaction=contract_satisfaction,
            reproducible_environment=reproducible_environment,
            public_verification=public_verification,
            hidden_verification=hidden_verification,
            baseline_failed=baseline_failed,
            baseline_checks=baseline_checks,
            patch_applied=patch_applied,
            fail_to_pass_satisfied=fail_to_pass_satisfied,
            verification_misconfiguration_reason=verification_misconfiguration_reason,
            evaluation_timing=evaluation_timing,
        )

    def _materialize_workspace(
        self,
        task: EvaluationTask,
        *,
        workspace: Path,
        manifest_base: Path,
        timing: dict[str, Any] | None = None,
    ) -> None:
        timing = timing if timing is not None else {}
        if task.workspace.kind == "fixture":
            workspace.mkdir(parents=True, exist_ok=True)
            for relative, content in task.workspace.files.items():
                _write_workspace_file(workspace, relative, content)
            return
        source_value = str(task.workspace.path or "")
        if _is_remote_git_url(source_value):
            if not task.workspace.start_commit:
                raise EvaluationSetupError(
                    f"remote evaluation repo task {task.task_id} requires start_commit.",
                    environment_blocker=False,
                )
            try:
                operation_started = time.perf_counter()
                _run_git(["clone", "--quiet", "--filter=blob:none", source_value, str(workspace)], cwd=manifest_base)
                timing["repo_clone_time_seconds"] = time.perf_counter() - operation_started
                timing["repo_fetch_time_seconds"] = None
                operation_started = time.perf_counter()
                _run_git(["checkout", "--quiet", task.workspace.start_commit], cwd=workspace)
                timing["repo_checkout_time_seconds"] = time.perf_counter() - operation_started
            except RuntimeError as exc:
                raise EvaluationSetupError(
                    f"setup/environment blocker: failed to materialize remote repo {source_value}: {exc}",
                    environment_blocker=True,
                ) from exc
            return
        source = Path(str(task.workspace.path or ""))
        if not source.is_absolute():
            source = (manifest_base / source).resolve(strict=False)
        if not source.exists():
            raise EvaluationSetupError(
                f"evaluation repo workspace path not found: {source}",
                environment_blocker=False,
            )
        if task.workspace.start_commit and _is_git_repo(source):
            try:
                operation_started = time.perf_counter()
                _run_git(["clone", "--quiet", "--shared", str(source), str(workspace)], cwd=manifest_base)
                timing["repo_clone_time_seconds"] = time.perf_counter() - operation_started
                timing["repo_fetch_time_seconds"] = None
                operation_started = time.perf_counter()
                _run_git(["checkout", "--quiet", task.workspace.start_commit], cwd=workspace)
                timing["repo_checkout_time_seconds"] = time.perf_counter() - operation_started
            except RuntimeError as exc:
                raise EvaluationSetupError(
                    f"evaluation repo checkout failed: {exc}",
                    environment_blocker=False,
                ) from exc
            return
        shutil.copytree(source, workspace, ignore=_copy_ignore)

    def _task_result(
        self,
        *,
        task: EvaluationTask,
        workspace: Path,
        trace: str,
        started: float,
        verification: CommandEvalResult | None,
        verification_workspace: Path,
        files_changed: list[str],
        usage: dict[str, Any],
        tool_calls: int,
        errors: list[str],
        patch: dict[str, Any],
        checks: dict[str, Any],
        success: bool,
        tests_passed: bool,
        infrastructure_blocked: bool,
        agent_status: str = "",
        final_report_payload: dict[str, Any] | None = None,
        trace_summary: dict[str, Any] | None = None,
        turn_count: int = 0,
        policy_blocks: int = 0,
        trace_artifact_refs: list[str] | None = None,
        contract_satisfaction: dict[str, Any] | None = None,
        reproducible_environment: dict[str, Any] | None = None,
        public_verification: CommandEvalResult | None = None,
        hidden_verification: CommandEvalResult | None = None,
        baseline_failed: bool = False,
        baseline_checks: dict[str, Any] | None = None,
        patch_applied: bool = False,
        fail_to_pass_satisfied: bool = False,
        verification_misconfiguration_reason: str = "",
        status_override: str = "",
        failure_category_override: str = "",
        evaluation_timing: dict[str, Any] | None = None,
    ) -> EvaluationTaskResult:
        request_rates = _float_map(usage.get("request_cache_hit_rates") or {})
        final_report_payload = final_report_payload or {}
        trace_summary = trace_summary or {}
        trace_path = Path(trace) if trace else None
        trace_events = _read_trace_events(trace_path)
        sandbox_audit = _sandbox_enforcement_audit(task, trace_events, trace_summary)
        visibility_audit = _evaluator_visibility_audit(task, trace_path)
        if success and not sandbox_audit["passed"]:
            errors.append(str(sandbox_audit["reason"]))
        if success and not visibility_audit["passed"]:
            errors.append(str(visibility_audit["reason"]))
        success = bool(success and sandbox_audit["passed"] and visibility_audit["passed"])
        status = status_override or _result_status(
            success=success,
            tests_passed=tests_passed,
            infrastructure_blocked=infrastructure_blocked,
            agent_status=agent_status,
            verification=verification,
            policy_blocks=policy_blocks,
            errors=errors,
        )
        verification_result = {
            "status": "passed" if verification and verification.passed else "failed" if verification else "not_run",
            "command": verification.command if verification else task.verification_command,
            "checks": checks or _checks_payload(None, verification),
        }
        final_report_status = _final_report_status(
            final_report_payload,
            agent_status=agent_status,
        )
        token_usage = {
            "input_tokens": _safe_int(usage.get("input_tokens")),
            "output_tokens": _safe_int(usage.get("output_tokens")),
            "total_tokens": _safe_int(usage.get("total_tokens")),
            "cached_input_tokens": _safe_int(usage.get("cached_input_tokens")),
            "reasoning_tokens": _safe_int(usage.get("reasoning_tokens")),
        }
        cache_usage = {
            "request_cache_hit_rate": _average_rate(request_rates),
            "run_cache_hit_rate": _safe_float(usage.get("run_cache_hit_rate")),
            "request_cache_hit_rates": request_rates,
            "cache_miss_reasons": dict(usage.get("cache_miss_reasons") or {}),
            "cache_attribution_sources": dict(usage.get("cache_attribution_sources") or {}),
        }
        report_status = final_report_status or agent_status
        agent_completed = _agent_completed(report_status, agent_status=agent_status)
        evaluation_passed = success
        patch_applicable = _patch_applicable_for_task(task, patch=patch, files_changed=files_changed)
        allowed_scope_passed = _allowed_scope_ok(files_changed, task.allowed_paths)
        public_verification_passed = _check_passed(checks, "public")
        hidden_verification_passed = _check_passed(checks, "hidden")
        repair_attempt_count = _repair_attempt_count(final_report_payload)
        repair_execution_count = _repair_execution_count(final_report_payload)
        miscompletion_count = int(agent_completed and not evaluation_passed)
        blocked_reason = _blocked_reason(
            final_report_payload,
            agent_status=agent_status,
            errors=errors,
            verification=verification,
        )
        failure_category = failure_category_override or _failure_category(
            final_report_payload,
            status=status,
            verification=verification,
            infrastructure_blocked=infrastructure_blocked,
            policy_blocks=policy_blocks,
            errors=errors,
        )
        capability_summary = _build_capability_summary(
            trace=trace_path,
            trace_summary=trace_summary,
            checks=checks or _checks_payload(None, verification),
            verification=verification,
            public_verification=public_verification or verification,
            hidden_verification=hidden_verification or verification,
            final_report_status=final_report_status,
            agent_status=agent_status,
            wall_time_seconds=round(time.perf_counter() - started, 3),
            evaluation_timing=evaluation_timing,
        )
        capability_summary["sandbox_enforcement"] = sandbox_audit
        capability_summary["evaluator_visibility_audit"] = visibility_audit
        capability_sla = _build_capability_sla(capability_summary)
        result_patch = patch or {"diff": "", "applicable": False, "changed_files": []}
        result_checks = checks or _checks_payload(None, verification)
        result_trace_artifacts = list(trace_artifact_refs or _trace_artifact_refs(final_report_payload, trace_summary))
        result_reproducible_environment = reproducible_environment or {}
        result_contract_satisfaction = contract_satisfaction or _contract_satisfaction(
            task,
            files_changed=files_changed,
            allowed_scope=allowed_scope_passed,
            verification=verification,
            public_verification=None,
            agent_status=agent_status,
            final_report_status=final_report_status,
            policy_blocks=policy_blocks,
            patch=result_patch,
            final_report_payload=final_report_payload,
        )
        evaluation_metrics = _build_evaluation_metrics(
            task=task,
            evaluation_passed=evaluation_passed,
            tests_passed=tests_passed,
            public_verification_passed=public_verification_passed,
            hidden_verification_passed=hidden_verification_passed,
            patch_applicable=patch_applicable,
            allowed_scope_passed=allowed_scope_passed,
            patch_applied=patch_applied,
            files_changed=files_changed,
            patch=result_patch,
            checks=result_checks,
            verification=verification,
            capability_summary=capability_summary,
            trace=trace_path,
            token_usage=token_usage,
            cache_usage=cache_usage,
            turn_count=turn_count or _safe_int(usage.get("requests")),
            tool_calls=tool_calls,
            agent_completed=agent_completed,
            miscompletion_count=miscompletion_count,
            repair_attempt_count=repair_attempt_count,
            repair_execution_count=repair_execution_count,
            blocked_reason=blocked_reason,
            failure_category=failure_category,
            final_report_status=final_report_status,
            agent_status=agent_status,
            policy_blocks=policy_blocks,
            trace_artifact_refs=result_trace_artifacts,
            reproducible_environment=result_reproducible_environment,
            baseline_failed=baseline_failed,
            baseline_checks=baseline_checks or {},
            fail_to_pass_satisfied=fail_to_pass_satisfied,
            verification_misconfiguration_reason=verification_misconfiguration_reason,
            error_summary=self.redactor.redact_text("; ".join(dict.fromkeys(errors)))[:1000],
        )
        return EvaluationTaskResult(
            task_id=task.task_id,
            tests_passed=tests_passed,
            infrastructure_blocked=infrastructure_blocked,
            status=status,
            turn_count=turn_count or _safe_int(usage.get("requests")),
            prompt_tokens=_safe_int(usage.get("input_tokens")),
            cached_tokens=_safe_int(usage.get("cached_input_tokens")),
            request_cache_hit_rate=_average_rate(request_rates),
            run_cache_hit_rate=_safe_float(usage.get("run_cache_hit_rate")),
            tool_calls=tool_calls,
            files_changed=[_display_path(path) for path in files_changed],
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=self.redactor.redact_text("; ".join(dict.fromkeys(errors)))[:1000],
            workspace=str(workspace),
            trace=trace,
            verification_workspace=str(verification_workspace) if verification_workspace else "",
            patch=result_patch,
            checks=result_checks,
            verification=verification,
            agent_completed=agent_completed,
            evaluation_passed=evaluation_passed,
            patch_applicable=patch_applicable,
            allowed_scope_passed=allowed_scope_passed,
            public_verification_passed=public_verification_passed,
            hidden_verification_passed=hidden_verification_passed,
            sandbox_enforcement_passed=bool(sandbox_audit["passed"]),
            evaluator_visibility_audit_passed=bool(visibility_audit["passed"]),
            local_process_fallback_count=_safe_int(capability_summary.get("local_process_fallback_count")),
            repair_attempt_count=repair_attempt_count,
            repair_execution_count=repair_execution_count,
            miscompletion_count=miscompletion_count,
            blocked_reason=blocked_reason,
            failure_category=failure_category,
            request_cache_hit_rates=request_rates,
            verification_result=verification_result,
            contract_satisfaction=result_contract_satisfaction,
            final_report_status=final_report_status,
            policy_blocks=policy_blocks,
            token_usage=token_usage,
            cache_usage=cache_usage,
            trace_artifact_refs=result_trace_artifacts,
            reproducible_environment=result_reproducible_environment,
            capability_summary=capability_summary,
            capability_sla=capability_sla,
            timing=dict(capability_summary.get("timing") or {}),
            baseline_failed=baseline_failed,
            baseline_checks=baseline_checks or {},
            patch_applied=patch_applied,
            fail_to_pass_satisfied=fail_to_pass_satisfied,
            verification_misconfiguration_reason=verification_misconfiguration_reason,
            evaluation_metrics=evaluation_metrics,
        )


def load_evaluation_task_set(path: Path | str) -> EvaluationTaskSet:
    manifest_path = Path(path)
    payload = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("evaluation manifest must be a JSON object.")
    return EvaluationTaskSet.from_dict(payload, base_dir=manifest_path.parent.resolve(strict=False))


def summarize_evaluation_results(results: list[EvaluationTaskResult]) -> dict[str, Any]:
    task_count = len(results)
    scored_results = [result for result in results if not result.infrastructure_blocked]
    scored_task_count = len(scored_results)
    infrastructure_blocked_count = task_count - scored_task_count
    evaluation_passed_count = sum(1 for result in results if result.evaluation_passed)
    tests_passed_count = sum(1 for result in results if result.tests_passed)
    prompt_tokens = sum(result.prompt_tokens for result in results)
    cached_tokens = sum(result.cached_tokens for result in results)
    resolved_count = sum(
        1 for result in scored_results if (result.evaluation_metrics.get("resolved") or {}).get("value") is True
    )
    fail_to_pass_satisfied_count = sum(
        1
        for result in scored_results
        if ((result.evaluation_metrics.get("swe_bench") or {}).get("fail_to_pass") or {}).get("satisfied") is True
    )
    pass_to_pass_satisfied_count = sum(
        1
        for result in scored_results
        if ((result.evaluation_metrics.get("swe_bench") or {}).get("pass_to_pass") or {}).get("satisfied") is True
    )
    pass_to_pass_not_configured_count = sum(
        1
        for result in results
        if ((result.evaluation_metrics.get("swe_bench") or {}).get("pass_to_pass") or {}).get("status")
        == "not_configured"
    )
    tool_success_rates = [
        _safe_float((result.evaluation_metrics.get("tools") or {}).get("tool_success_rate"))
        for result in scored_results
        if (result.evaluation_metrics.get("tools") or {}).get("tool_success_rate") is not None
    ]
    cost_estimates = [
        _safe_float((result.evaluation_metrics.get("cost") or {}).get("cost_estimate"))
        for result in results
        if (result.evaluation_metrics.get("cost") or {}).get("cost_estimate") is not None
    ]
    scored_cost_estimates = [
        _safe_float((result.evaluation_metrics.get("cost") or {}).get("cost_estimate"))
        for result in scored_results
        if (result.evaluation_metrics.get("cost") or {}).get("cost_estimate") is not None
    ]
    total_cost_estimate = round(sum(cost_estimates), 6)
    scored_cost_estimate = round(sum(scored_cost_estimates), 6)
    failures: dict[str, int] = {}
    miscompletion_count = 0
    for result in results:
        if not result.evaluation_passed:
            reason = result.failure_category if result.failure_category and result.failure_category != "none" else result.status
            failures[reason or "failure"] = failures.get(reason or "failure", 0) + 1
    for result in scored_results:
        miscompletion_count += result.miscompletion_count or int(
            result.agent_completed and not result.evaluation_passed
        )
    agent_completed_count = sum(1 for result in scored_results if result.agent_completed)
    capability_sla = _summarize_capability_sla(results)
    return {
        "task_count": task_count,
        "scored_task_count": scored_task_count,
        "infrastructure_blocked_count": infrastructure_blocked_count,
        "score_status": _score_status(
            task_count=task_count,
            scored_task_count=scored_task_count,
            infrastructure_blocked_count=infrastructure_blocked_count,
        ),
        "task_completion_rate": _rate(agent_completed_count, scored_task_count),
        "tests_passed_count": tests_passed_count,
        "test_pass_rate": _rate(tests_passed_count, scored_task_count),
        "prompt_tokens": prompt_tokens,
        "cached_tokens": cached_tokens,
        "request_cache_hit_rate": _average_rate({result.task_id: result.request_cache_hit_rate for result in scored_results}),
        "run_cache_hit_rate": _rate(cached_tokens, prompt_tokens),
        "tool_calls": sum(result.tool_calls for result in results),
        "resolved_count": resolved_count,
        "resolved_rate": _rate(resolved_count, scored_task_count),
        "fail_to_pass_satisfied_count": fail_to_pass_satisfied_count,
        "pass_to_pass_satisfied_count": pass_to_pass_satisfied_count,
        "pass_to_pass_not_configured_count": pass_to_pass_not_configured_count,
        "average_tool_success_rate": round(sum(tool_success_rates) / len(tool_success_rates), 4)
        if tool_success_rates
        else None,
        "total_cost_estimate": total_cost_estimate,
        "cost_per_resolved": round(scored_cost_estimate / resolved_count, 6) if resolved_count else None,
        "evaluation_passed_rate": _rate(evaluation_passed_count, scored_task_count),
        "verification_pass_rate": _rate(tests_passed_count, scored_task_count),
        "average_turns": round(
            sum(result.turn_count for result in scored_results) / scored_task_count,
            4,
        )
        if scored_task_count
        else 0.0,
        "average_tool_calls": round(
            sum(result.tool_calls for result in scored_results) / scored_task_count,
            4,
        )
        if scored_task_count
        else 0.0,
        "agent_completed_count": agent_completed_count,
        "evaluation_passed_count": evaluation_passed_count,
        "repair_attempt_count": sum(result.repair_attempt_count for result in results),
        "repair_execution_count": sum(result.repair_execution_count for result in results),
        "policy_blocks": sum(result.policy_blocks for result in results),
        "miscompletion_count": miscompletion_count,
        "failure_reasons": dict(sorted(failures.items())),
        "capability_sla": capability_sla,
    }


def compare_evaluation_results(
    baseline: dict[str, Any],
    candidate: dict[str, Any],
) -> dict[str, Any]:
    baseline_summary = _dict(baseline.get("summary") or {}, "baseline.summary")
    candidate_summary = _dict(candidate.get("summary") or {}, "candidate.summary")
    task_diffs: list[dict[str, Any]] = []
    baseline_tasks = {
        str(item.get("task_id")): item
        for item in baseline.get("tasks") or []
        if isinstance(item, dict)
    }
    for item in candidate.get("tasks") or []:
        if not isinstance(item, dict):
            continue
        task_id = str(item.get("task_id") or "")
        previous = baseline_tasks.get(task_id)
        if not previous:
            continue
        task_diffs.append(
            {
                "task_id": task_id,
                "baseline_status": str(previous.get("status") or ""),
                "candidate_status": str(item.get("status") or ""),
                "baseline_success": _evaluation_passed_from_payload(previous),
                "candidate_success": _evaluation_passed_from_payload(item),
                "turn_delta": _safe_int(item.get("turn_count")) - _safe_int(previous.get("turn_count")),
                "tool_call_delta": _payload_tool_calls(item) - _payload_tool_calls(previous),
                "verification_changed": bool(previous.get("tests_passed")) != bool(item.get("tests_passed")),
                "trace_artifact_refs": list(item.get("trace_artifact_refs") or []),
            }
        )
    regressions = [
        diff
        for diff in task_diffs
        if diff["baseline_success"] and not diff["candidate_success"]
    ]
    return {
        "schema_version": "evaluation.regression/v1",
        "baseline_run_id": str(baseline.get("run_id") or ""),
        "candidate_run_id": str(candidate.get("run_id") or ""),
        "summary": {
            "evaluation_passed_rate_delta": _float_delta(
                candidate_summary,
                baseline_summary,
                "evaluation_passed_rate",
            ),
            "verification_pass_rate_delta": _float_delta(candidate_summary, baseline_summary, "verification_pass_rate"),
            "average_turns_delta": _float_delta(candidate_summary, baseline_summary, "average_turns"),
            "average_tool_calls_delta": _float_delta(candidate_summary, baseline_summary, "average_tool_calls"),
            "policy_blocks_delta": _safe_int(candidate_summary.get("policy_blocks"))
            - _safe_int(baseline_summary.get("policy_blocks")),
            "miscompletion_delta": _safe_int(candidate_summary.get("miscompletion_count"))
            - _safe_int(baseline_summary.get("miscompletion_count")),
            "regression_count": len(regressions),
        },
        "task_diffs": task_diffs,
        "regressions": regressions,
    }


def evaluation_report_markdown(payload: dict[str, Any]) -> str:
    summary = _dict(payload.get("summary") or {}, "summary")
    lines = [
        f"# Agent Evaluation `{payload.get('run_id', '')}`",
        "",
        f"- status: `{summary.get('score_status', 'unknown')}`",
        f"- task count: {summary.get('task_count', 0)}",
        f"- evaluation passed rate: {summary.get('evaluation_passed_rate', 0):.4f}",
        f"- verification pass rate: {summary.get('verification_pass_rate', 0):.4f}",
        f"- average turns: {summary.get('average_turns', 0):.4f}",
        f"- average tool calls: {summary.get('average_tool_calls', 0):.4f}",
        f"- policy blocks: {summary.get('policy_blocks', 0)}",
        f"- repair attempts: {summary.get('repair_attempt_count', 0)}",
        f"- repair executions: {summary.get('repair_execution_count', 0)}",
        f"- miscompletion count: {summary.get('miscompletion_count', 0)}",
        "",
        "## Metrics / Scorecard",
        "",
        f"- resolved: {summary.get('resolved_count', 0)} / {summary.get('scored_task_count', 0)} ({summary.get('resolved_rate', 0):.4f})",
        f"- FAIL_TO_PASS satisfied: {summary.get('fail_to_pass_satisfied_count', 0)}",
        f"- PASS_TO_PASS satisfied: {summary.get('pass_to_pass_satisfied_count', 0)}",
        f"- PASS_TO_PASS not configured: {summary.get('pass_to_pass_not_configured_count', 0)}",
        f"- average tool success rate: {_format_optional_rate(summary.get('average_tool_success_rate'))}",
        f"- total cost estimate: {_format_optional_float(summary.get('total_cost_estimate'))}",
        f"- cost per resolved: {_format_optional_float(summary.get('cost_per_resolved'))}",
        "",
        "| task | status | verification | turns | tools | files changed | final report | contract | failures |",
        "| --- | --- | --- | ---: | ---: | --- | --- | --- | --- |",
    ]
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        files = ", ".join(str(item) for item in task.get("files_changed") or []) or "-"
        failures = str(task.get("error_summary") or "-").replace("|", "\\|")
        contract = _dict(task.get("contract_satisfaction") or {}, "contract").get("status", "unknown")
        verification = _dict(task.get("verification_result") or {}, "verification").get("status", "unknown")
        lines.append(
            "| "
            f"`{task.get('task_id', '')}` | "
            f"{task.get('status') or 'unknown'} | "
            f"{verification} | "
            f"{_safe_int(task.get('turn_count'))} | "
            f"{_safe_int(task.get('tool_calls'))} | "
            f"{files} | "
            f"{task.get('final_report_status') or '-'} | "
            f"{contract} | "
            f"{failures} |"
        )
    scorecard_rows: list[str] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        metrics = task.get("evaluation_metrics") or {}
        if not isinstance(metrics, dict):
            continue
        resolved = metrics.get("resolved") or {}
        swe_bench = metrics.get("swe_bench") or {}
        fail_to_pass = swe_bench.get("fail_to_pass") if isinstance(swe_bench, dict) else {}
        pass_to_pass = swe_bench.get("pass_to_pass") if isinstance(swe_bench, dict) else {}
        verification = metrics.get("verification") or {}
        patch = metrics.get("patch") or {}
        trajectory = metrics.get("trajectory") or {}
        tools = metrics.get("tools") or {}
        context = metrics.get("context") or {}
        efficiency = metrics.get("efficiency") or {}
        cost = metrics.get("cost") or {}
        safety = metrics.get("safety") or {}
        failure_reason = str((resolved or {}).get("reason") or task.get("failure_category") or "-").replace("|", "\\|")
        scorecard_rows.append(
            "| "
            f"`{task.get('task_id', '')}` | "
            f"{bool((resolved or {}).get('value'))} | "
            f"{(fail_to_pass or {}).get('satisfied')} | "
            f"{(pass_to_pass or {}).get('status') or (pass_to_pass or {}).get('satisfied')} | "
            f"{(verification or {}).get('tests_passed')} | "
            f"{(patch or {}).get('files_changed_count')} / {(patch or {}).get('out_of_scope_files') or []} | "
            f"{(trajectory or {}).get('turn_count')} / {(tools or {}).get('tool_call_count')} / {(tools or {}).get('tool_success_rate')} | "
            f"{((context or {}).get('compaction') or {}).get('reason') or ((context or {}).get('compaction') or {}).get('status') or '-'} | "
            f"{(efficiency or {}).get('wall_time_seconds')} | "
            f"{_format_optional_float((cost or {}).get('cost_estimate'))} ({(cost or {}).get('cost_source', 'unknown')}) | "
            f"{(safety or {}).get('policy_blocks')} | "
            f"{failure_reason} |"
        )
    if scorecard_rows:
        lines.extend(
            [
                "",
                "| task | resolved | FAIL_TO_PASS | PASS_TO_PASS | verification | patch files / out of scope | turns / tools / success | context / compaction | wall seconds | cost | policy blocks | failure reason |",
                "| --- | --- | --- | --- | --- | --- | --- | --- | ---: | --- | ---: | --- |",
                *scorecard_rows,
            ]
        )
    timing_rows: list[str] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        timing = task.get("timing") or {}
        capability = task.get("capability_summary") or {}
        diagnostics = capability.get("timing_diagnostics") if isinstance(capability, dict) else {}
        diagnostics = diagnostics if isinstance(diagnostics, dict) else {}
        if not isinstance(timing, dict):
            continue
        for name, value in sorted(timing.items()):
            diagnostic = diagnostics.get(name) if isinstance(diagnostics.get(name), dict) else {}
            status = str(diagnostic.get("status") or ("measured" if value is not None else "unavailable"))
            reason = str(diagnostic.get("reason") or "-").replace("|", "\\|")
            rendered_value = "-" if value is None else str(value)
            timing_rows.append(
                f"| `{task.get('task_id', '')}` | `{name}` | {rendered_value} | {status} | {reason} |"
            )
    if timing_rows:
        lines.extend(
            [
                "",
                "## Timing Breakdown",
                "",
                "| task | metric | seconds | status | reason |",
                "| --- | --- | ---: | --- | --- |",
                *timing_rows,
            ]
        )
    sla_rows: list[str] = []
    for task in payload.get("tasks") or []:
        if not isinstance(task, dict):
            continue
        sla = task.get("capability_sla") or {}
        if not isinstance(sla, dict):
            continue
        for name, item in sorted((sla.get("items") or {}).items()):
            if not isinstance(item, dict):
                continue
            actual = item.get("actual_seconds")
            target = item.get("target_seconds")
            if actual is None and "actual_count" in item:
                actual = item.get("actual_count")
                target = item.get("target_count")
            if actual is None and "passed" in item:
                actual = item.get("passed")
                target = True
            sla_rows.append(
                "| "
                f"`{task.get('task_id', '')}` | "
                f"`{name}` | "
                f"{actual} | "
                f"{target} | "
                f"{item.get('status') or 'unknown'} | "
                f"{item.get('blocking') is True} |"
            )
    if sla_rows:
        lines.extend(
            [
                "",
                "## Capability SLA",
                "",
                "| task | item | actual | target | status | blocking |",
                "| --- | --- | ---: | ---: | --- | --- |",
                *sla_rows,
            ]
        )
    regression = payload.get("regression")
    if isinstance(regression, dict):
        lines.extend(["", "## Regression", ""])
        lines.extend(evaluation_regression_markdown(regression).splitlines()[2:])
    return "\n".join(lines) + "\n"


def evaluation_regression_markdown(payload: dict[str, Any]) -> str:
    summary = _dict(payload.get("summary") or {}, "regression.summary")
    lines = [
        f"# Agent Evaluation Regression `{payload.get('baseline_run_id', '')}` -> `{payload.get('candidate_run_id', '')}`",
        "",
        f"- evaluation passed rate delta: {summary.get('evaluation_passed_rate_delta', 0):.4f}",
        f"- verification pass rate delta: {summary.get('verification_pass_rate_delta', 0):.4f}",
        f"- average turns delta: {summary.get('average_turns_delta', 0):.4f}",
        f"- average tool calls delta: {summary.get('average_tool_calls_delta', 0):.4f}",
        f"- policy blocks delta: {summary.get('policy_blocks_delta', 0)}",
        f"- miscompletion delta: {summary.get('miscompletion_delta', 0)}",
        f"- regression count: {summary.get('regression_count', 0)}",
        "",
        "| task | baseline | candidate | turn delta | tool delta | trace artifacts |",
        "| --- | --- | --- | ---: | ---: | --- |",
    ]
    for diff in payload.get("task_diffs") or []:
        if not isinstance(diff, dict):
            continue
        artifacts = ", ".join(str(item) for item in diff.get("trace_artifact_refs") or []) or "-"
        lines.append(
            "| "
            f"`{diff.get('task_id', '')}` | "
            f"{diff.get('baseline_status', '')} | "
            f"{diff.get('candidate_status', '')} | "
            f"{diff.get('turn_delta', 0)} | "
            f"{diff.get('tool_call_delta', 0)} | "
            f"{artifacts} |"
        )
    return "\n".join(lines) + "\n"


class SweBenchAdapter:
    def load(self, _path: Path | str) -> EvaluationTaskSet:
        raise NotImplementedError("SWE-bench adapter boundary is reserved; use SingularityPrivateBenchmarkAdapter today.")


class TerminalBenchAdapter:
    def load(self, _path: Path | str) -> EvaluationTaskSet:
        raise NotImplementedError("Terminal-Bench adapter boundary is reserved; use SingularityPrivateBenchmarkAdapter today.")


def _score_status(*, task_count: int, scored_task_count: int, infrastructure_blocked_count: int) -> str:
    if scored_task_count > 0:
        return "scored"
    if task_count > 0 and infrastructure_blocked_count == task_count:
        return "environment_blocker"
    return "empty"


def _previous_evaluation_result(
    output_root: Path,
    *,
    current_run_id: str,
    baseline_result_path: Path | None = None,
) -> dict[str, Any] | None:
    if baseline_result_path is not None:
        try:
            payload = json.loads(baseline_result_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None
        if isinstance(payload, dict) and _is_supported_evaluation_result(payload):
            return payload
        return None
    candidates: list[tuple[int, Path]] = []
    if not output_root.exists():
        return None
    for path in output_root.glob("*/result.json"):
        if path.parent.name == current_run_id:
            continue
        try:
            candidates.append((path.stat().st_mtime_ns, path))
        except OSError:
            continue
    for _mtime, path in sorted(candidates, reverse=True):
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(payload, dict) and _is_supported_evaluation_result(payload):
            return payload
    return None


def _is_supported_evaluation_result(payload: dict[str, Any]) -> bool:
    return payload.get("schema_version") == EVALUATION_RESULT_SCHEMA_VERSION


def _workspace_payload(payload: dict[str, Any]) -> dict[str, Any]:
    workspace = payload.get("workspace")
    if isinstance(workspace, dict):
        if payload.get("start_commit") and not workspace.get("start_commit"):
            workspace = {**workspace, "start_commit": payload["start_commit"]}
        return workspace
    if "fixture_workspace" in payload:
        fixture = _dict(payload.get("fixture_workspace"), "fixture_workspace")
        return {"type": "fixture", "files": fixture.get("files") or fixture}
    repo_path = payload.get("repo_path") or payload.get("path")
    if repo_path:
        return {"type": "repo", "path": repo_path, "start_commit": payload.get("start_commit")}
    raise ValueError("evaluation task requires workspace, repo_path, or fixture_workspace.")


def _task_goal(task: EvaluationTask) -> str:
    allowed = ", ".join(task.allowed_paths)
    tools = ", ".join(task.allowed_tools) if task.allowed_tools else "default Singularity coding tools"
    risks = ", ".join(task.risk_tags) if task.risk_tags else "none declared"
    expected_changes = ", ".join(task.expected_file_changes) if task.expected_file_changes else "no required file changes declared"
    visible_command = _model_visible_verification_command(task)
    if _requires_baseline_verification(task) and not visible_command:
        verification_instruction = (
            "Before finishing, run the relevant local checks you can infer from the changed code. "
            "Independent evaluator-only public and hidden verification will run after you finish."
        )
    elif _requires_baseline_verification(task):
        verification_instruction = (
            f"Before finishing, run this local smoke verification command: {visible_command}. "
            "Independent evaluator-only public and hidden verification will run after you finish."
        )
    elif visible_command:
        verification_instruction = f"Before finishing, run this verification command: {visible_command}"
    elif task.verification_prepare_commands:
        verification_instruction = (
            "Before finishing, run the relevant visible checks you can infer. "
            "Hidden evaluator setup and independent verification will run after you finish."
        )
    else:
        verification_instruction = f"Before finishing, run this verification command: {task.verification_command}"
    return (
        f"{task.user_task}\n\n"
        f"Allowed modification scope: {allowed}.\n"
        f"Allowed tool strategy: {task.tool_policy}; preferred tools: {tools}.\n"
        f"Expected file changes: {expected_changes}.\n"
        f"Completion standard: {task.completion_standard or 'satisfy the verification command and scope contract'}.\n"
        f"Risk tags: {risks}.\n"
        f"{verification_instruction}\n"
        "Do not read, print, or modify .env files or API keys."
    )


def _reproducible_environment(
    task: EvaluationTask,
    *,
    workspace: Path,
    manifest_base: Path,
    output_root: Path,
    baseline_result_path: Path | None,
    config: ProductionConfig,
    max_turns: int,
) -> dict[str, Any]:
    effective = config.effective_config()
    return {
        "schema_version": "evaluation.environment/v1",
        "task_id": task.task_id,
        "task_type": task.task_type,
        "workspace": _workspace_environment(task, manifest_base=manifest_base),
        "workspace_path": str(workspace),
        "prepare_commands": list(task.prepare_commands),
        "verification_command": task.verification_command,
        "public_verification_command": _public_verification_command(task),
        "hidden_verification_command": _hidden_verification_command(task),
        "verification_prepare_commands": list(task.verification_prepare_commands),
        "verification_timeout_seconds": task.verification_timeout_seconds,
        "allowed_paths": list(task.allowed_paths),
        "allowed_tools": list(task.allowed_tools),
        "expected_file_changes": list(task.expected_file_changes),
        "completion_standard": task.completion_standard,
        "risk_tags": list(task.risk_tags),
        "model_profile": {
            "model": config.model or os.getenv("SINGULARITY_MODEL") or None,
            "base_url": _redacted_url(config.base_url or os.getenv("SINGULARITY_BASE_URL") or ""),
            "profile": config.profile,
            "max_turns": max_turns,
            "sources": {
                "model": effective.get("sources", {}).get("model"),
                "base_url": effective.get("sources", {}).get("base_url"),
                "env_file": effective.get("sources", {}).get("env_file"),
            },
        },
        "policy": {
            "tool_policy": task.tool_policy,
            "permission_profile": config.permission_profile.value,
            "approval_policy": config.approval_policy.value,
            "network_access": config.network_access.value,
            "interaction_mode": config.interaction_mode.value,
        },
        "baseline_artifacts": {
            "baseline_result_path": str(baseline_result_path) if baseline_result_path else None,
            "output_root": str(output_root),
        },
        "runtime": {
            "python": sys.version.split()[0],
            "platform": sys.platform,
            "interpreter_strategy": _interpreter_strategy_summary(),
        },
    }


def _workspace_environment(task: EvaluationTask, *, manifest_base: Path) -> dict[str, Any]:
    if task.workspace.kind == "fixture":
        return {
            "type": "fixture",
            "file_count": len(task.workspace.files),
            "file_names": sorted(task.workspace.files),
            "source": "manifest.inline_files",
        }
    source = task.workspace.path or ""
    if _is_remote_git_url(source):
        return {
            "type": "repo",
            "source": _redacted_url(source) or source,
            "start_commit": task.workspace.start_commit,
            "materialization": "evaluator_remote_clone",
        }
    source_path = Path(source).resolve(strict=False) if source else None
    try:
        source_ref = source_path.relative_to(manifest_base.resolve(strict=False)).as_posix() if source_path else ""
    except ValueError:
        source_ref = str(source_path) if source_path else ""
    return {
        "type": "repo",
        "source": source_ref,
        "start_commit": task.workspace.start_commit,
    }


def _setup_environment(task: EvaluationTask, *, manifest_base: Path) -> dict[str, Any]:
    return {
        "schema_version": "evaluation.environment/v1",
        "task_id": task.task_id,
        "task_type": task.task_type,
        "workspace": _workspace_environment(task, manifest_base=manifest_base),
        "prepare_commands": list(task.prepare_commands),
        "verification_command": task.verification_command,
        "public_verification_command": _public_verification_command(task),
        "hidden_verification_command": _hidden_verification_command(task),
        "verification_prepare_commands": list(task.verification_prepare_commands),
        "allowed_paths": list(task.allowed_paths),
        "expected_file_changes": list(task.expected_file_changes),
        "completion_standard": task.completion_standard,
        "risk_tags": list(task.risk_tags),
        "policy": {
            "tool_policy": task.tool_policy,
            "permission_profile": _permission_profile_for_task(task).value,
            "approval_policy": _approval_policy_for_task(task).value,
            "network_access": _network_access_for_task(task).value,
        },
    }


def _dependency_setup_cache(
    task: EvaluationTask,
    *,
    workspace: Path,
    output_root: Path,
) -> tuple[dict[str, str], dict[str, Any]]:
    if not task.prepare_commands:
        return {}, {
            "schema_version": "evaluation.dependency_setup_cache/v2",
            "enabled": False,
            "strategy": "pip_cache_dir",
            "scope": "evaluator_prepare_commands_only",
            "hit": False,
            "miss_reason": "no_prepare_commands",
            "created_at": "",
            "source_cache_dir": "",
            "workspace_link_or_copy": "",
            "model_visible": False,
            "changes_acl": False,
            "bypasses_windows_sandbox": False,
            "uses_local_process_fallback": False,
        }
    cache_root = output_root.parent / f"{output_root.name}-dependency-cache"
    dependency_files = _dependency_file_digests(workspace)
    key_payload = {
        "schema_version": "evaluation.dependency_setup_cache_key/v1",
        "python": sys.version.split()[0],
        "platform": sys.platform,
        "task_id": task.task_id,
        "workspace": {
            "kind": task.workspace.kind,
            "path": task.workspace.path,
            "start_commit": task.workspace.start_commit,
        },
        "prepare_commands": list(task.prepare_commands),
        "dependency_files": dependency_files,
    }
    cache_key = hashlib.sha256(
        json.dumps(key_payload, ensure_ascii=False, sort_keys=True).encode("utf-8")
    ).hexdigest()
    cache_dir = cache_root / cache_key[:16]
    pip_cache_dir = cache_dir / "pip"
    wheelhouse_dir = cache_dir / "wheelhouse"
    prepared_env_dir = cache_dir / "prepared-venv"
    ready_marker = cache_dir / "prepared-venv.ready.json"
    strategy = "prepared_venv" if _prepare_commands_target_eval_venv(task.prepare_commands) else "pip_cache_dir"
    hit = False
    miss_reason = "cache_not_ready"
    workspace_link_or_copy = ""
    workspace_rebind: dict[str, Any] = {"status": "not_applicable", "files_rewritten": 0}
    restore_time_seconds: float | None = None
    workspace_rebind_time_seconds: float | None = None
    if strategy == "prepared_venv" and prepared_env_dir.is_dir() and ready_marker.is_file():
        marker = _read_dependency_cache_marker(ready_marker, cache_key=cache_key)
        if marker:
            workspace_venv = _prepared_eval_venv_path(workspace)
            restore_started = time.perf_counter()
            _replace_tree(prepared_env_dir, workspace_venv)
            restore_time_seconds = round(time.perf_counter() - restore_started, 3)
            rebind_started = time.perf_counter()
            workspace_rebind = _rebind_prepared_venv_workspace(
                workspace_venv,
                workspace=workspace,
                source_workspace=Path(str(marker.get("source_workspace") or "")),
                source_venv=Path(str(marker.get("source_venv") or "")),
            )
            workspace_rebind_time_seconds = round(time.perf_counter() - rebind_started, 3)
            hit = True
            miss_reason = ""
            workspace_link_or_copy = str(workspace_venv)
        else:
            miss_reason = "cache_marker_invalid"
    cache_dir.mkdir(parents=True, exist_ok=True)
    pip_cache_dir.mkdir(parents=True, exist_ok=True)
    wheelhouse_dir.mkdir(parents=True, exist_ok=True)
    audit = {
        "schema_version": "evaluation.dependency_setup_cache/v2",
        "enabled": bool(task.prepare_commands),
        "strategy": strategy,
        "available_strategies": ["pip_cache_dir", "wheelhouse", "prepared_venv"],
        "scope": "evaluator_prepare_commands_only",
        "cache_key": cache_key,
        "cache_root": str(cache_root),
        "cache_dir": str(cache_dir),
        "pip_cache_dir": str(pip_cache_dir),
        "wheelhouse_dir": str(wheelhouse_dir),
        "prepared_env_dir": str(prepared_env_dir),
        "ready_marker": str(ready_marker),
        "hit": hit,
        "miss_reason": miss_reason,
        "created_at": _utc_timestamp(),
        "source_cache_dir": str(prepared_env_dir) if hit else "",
        "workspace_link_or_copy": workspace_link_or_copy,
        "workspace_rebind": workspace_rebind,
        "restore_time_seconds": restore_time_seconds,
        "workspace_rebind_time_seconds": workspace_rebind_time_seconds,
        "invalidation": {
            "python": sys.version.split()[0],
            "platform": sys.platform,
            "workspace": {
                "kind": task.workspace.kind,
                "path_digest": hashlib.sha256(str(task.workspace.path or "").encode("utf-8")).hexdigest(),
                "start_commit": task.workspace.start_commit,
            },
            "workspace_start_commit": task.workspace.start_commit,
            "prepare_commands_digest": hashlib.sha256(
                json.dumps(list(task.prepare_commands), ensure_ascii=False).encode("utf-8")
            ).hexdigest(),
            "dependency_files": dependency_files,
        },
        "model_visible": False,
        "changes_acl": False,
        "bypasses_windows_sandbox": False,
        "uses_local_process_fallback": False,
    }
    return {"PIP_CACHE_DIR": str(pip_cache_dir)}, audit


def _prepare_commands_target_eval_venv(commands: list[str]) -> bool:
    if not commands:
        return False
    normalized = " ".join(commands).replace("\\", "/")
    return "../.eval-venv" in normalized


def _prepared_eval_venv_path(workspace: Path) -> Path:
    return workspace.parent / ".eval-venv"


def _replace_tree(source: Path, target: Path) -> None:
    if target.exists():
        _make_tree_writable(target)
        shutil.rmtree(target)
    shutil.copytree(source, target)


def _read_dependency_cache_marker(marker_path: Path, *, cache_key: str) -> dict[str, Any]:
    try:
        marker = json.loads(marker_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    if not isinstance(marker, dict):
        return {}
    if marker.get("cache_key") != cache_key:
        return {}
    if marker.get("strategy") != "prepared_venv":
        return {}
    if not marker.get("source_workspace") or not marker.get("source_venv"):
        return {}
    return marker


def _rebind_prepared_venv_workspace(
    venv: Path,
    *,
    workspace: Path,
    source_workspace: Path,
    source_venv: Path,
) -> dict[str, Any]:
    if not source_workspace or not source_venv:
        return {"status": "skipped_missing_marker_paths", "files_rewritten": 0}
    replacements = _path_replacement_pairs(
        {
            source_workspace: workspace,
            source_venv: venv,
        }
    )
    files_rewritten = 0
    for site_packages in _prepared_venv_site_package_dirs(venv):
        for path in _prepared_venv_rebind_files(site_packages):
            try:
                data = path.read_bytes()
            except OSError:
                continue
            if len(data) > 2_000_000:
                continue
            updated = data
            for old, new in replacements:
                updated = updated.replace(old, new)
            if updated != data:
                path.write_bytes(updated)
                files_rewritten += 1
    return {"status": "completed", "files_rewritten": files_rewritten}


def _prepared_venv_site_package_dirs(venv: Path) -> list[Path]:
    candidates = [
        venv / "Lib" / "site-packages",
    ]
    if (venv / "lib").exists():
        candidates.extend(sorted((venv / "lib").glob("python*/site-packages")))
    seen: set[Path] = set()
    result: list[Path] = []
    for path in candidates:
        resolved = path.resolve(strict=False)
        if path.is_dir() and resolved not in seen and resolved.is_relative_to(venv.resolve(strict=False)):
            seen.add(resolved)
            result.append(path)
    return result


def _prepared_venv_rebind_files(site_packages: Path) -> list[Path]:
    candidates: list[Path] = []
    candidates.extend(site_packages.glob("*.pth"))
    candidates.extend(site_packages.glob("*.egg-link"))
    candidates.extend(site_packages.glob("__editable__*.py"))
    candidates.extend(site_packages.glob("__editable__*.pth"))
    candidates.extend(site_packages.glob("*.dist-info/direct_url.json"))
    seen: set[Path] = set()
    result: list[Path] = []
    for path in candidates:
        resolved = path.resolve(strict=False)
        if path.is_file() and resolved not in seen and resolved.is_relative_to(site_packages.resolve(strict=False)):
            seen.add(resolved)
            result.append(path)
    return result


def _path_replacement_pairs(paths: dict[Path, Path]) -> list[tuple[bytes, bytes]]:
    pairs: list[tuple[bytes, bytes]] = []
    seen: set[bytes] = set()
    for old, new in paths.items():
        variants: list[tuple[str, str]] = [
            (str(old), str(new)),
            (old.as_posix(), new.as_posix()),
            (str(old).replace("\\", "\\\\"), str(new).replace("\\", "\\\\")),
        ]
        if old.is_absolute() and new.is_absolute():
            variants.append((old.as_uri(), new.as_uri()))
        for old_text, new_text in variants:
            old_bytes = old_text.encode("utf-8")
            if old_bytes and old_bytes not in seen:
                seen.add(old_bytes)
                pairs.append((old_bytes, new_text.encode("utf-8")))
    return pairs


def _finalize_dependency_setup_cache(audit: dict[str, Any], *, workspace: Path) -> None:
    if audit.get("strategy") != "prepared_venv":
        audit["finalize_status"] = "not_applicable"
        return
    if audit.get("hit") is True:
        audit["finalize_status"] = "skipped_hit"
        return
    workspace_venv = _prepared_eval_venv_path(workspace)
    if not workspace_venv.is_dir():
        audit["miss_reason"] = "prepared_env_not_created"
        audit["finalize_status"] = "skipped_missing_prepared_env"
        return
    prepared_env_dir = Path(str(audit.get("prepared_env_dir") or ""))
    ready_marker = Path(str(audit.get("ready_marker") or ""))
    temp_dir = prepared_env_dir.parent / f"{prepared_env_dir.name}.tmp-{os.getpid()}-{time.time_ns()}"
    marker_payload = {
        "schema_version": "evaluation.dependency_setup_cache_marker/v1",
        "cache_key": audit.get("cache_key"),
        "created_at": _utc_timestamp(),
        "strategy": "prepared_venv",
        "source_workspace": str(workspace),
        "source_workspace_hash": hashlib.sha256(str(workspace).encode("utf-8")).hexdigest(),
        "source_venv": str(workspace_venv),
        "source_venv_hash": hashlib.sha256(str(workspace_venv).encode("utf-8")).hexdigest(),
    }
    marker_tmp = ready_marker.with_name(f"{ready_marker.name}.tmp-{os.getpid()}-{time.time_ns()}")
    try:
        prepared_env_dir.parent.mkdir(parents=True, exist_ok=True)
        if temp_dir.exists():
            _make_tree_writable(temp_dir)
            shutil.rmtree(temp_dir)
        shutil.copytree(workspace_venv, temp_dir)
        if prepared_env_dir.exists():
            _make_tree_writable(prepared_env_dir)
            shutil.rmtree(prepared_env_dir)
        try:
            temp_dir.rename(prepared_env_dir)
        except OSError:
            shutil.move(str(temp_dir), str(prepared_env_dir))
        marker_tmp.write_text(
            json.dumps(marker_payload, ensure_ascii=False, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        marker_tmp.replace(ready_marker)
    except (OSError, shutil.Error) as exc:
        audit["miss_reason"] = "cache_finalize_failed"
        audit["finalize_status"] = "failed"
        audit["finalize_error"] = _dependency_cache_error(exc)
        _cleanup_dependency_cache_temp(temp_dir)
        _cleanup_dependency_cache_temp(marker_tmp)
        return
    audit["source_cache_dir"] = str(prepared_env_dir)
    audit["workspace_link_or_copy"] = str(workspace_venv)
    audit["finalize_status"] = "ready"


def _cleanup_dependency_cache_temp(path: Path) -> None:
    try:
        if path.is_dir():
            _make_tree_writable(path)
            shutil.rmtree(path)
        elif path.exists():
            path.unlink()
    except OSError:
        return


def _dependency_cache_error(exc: BaseException) -> dict[str, Any]:
    text = f"{type(exc).__name__}:{exc}"
    return {
        "type": type(exc).__name__,
        "message_hash": hashlib.sha256(text.encode("utf-8", errors="replace")).hexdigest()[:16],
    }


def _utc_timestamp() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def _dependency_file_digests(workspace: Path) -> dict[str, str | None]:
    candidates = ("requirements.txt", "pyproject.toml", "setup.cfg", "setup.py")
    digests: dict[str, str | None] = {}
    for relative in candidates:
        path = workspace / relative
        if not path.is_file():
            digests[relative] = None
            continue
        digests[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
    return digests


def _redacted_url(value: str) -> str | None:
    if not value:
        return None
    redacted = TraceRedactor().redact_text(value)
    if "@" in redacted:
        scheme, _, rest = redacted.partition("://")
        if rest:
            rest = rest.split("@", 1)[-1]
            return f"{scheme}://[REDACTED]@{rest}" if scheme else f"[REDACTED]@{rest}"
    return redacted


def _trace_path(kernel: Any) -> str:
    trace = getattr(getattr(kernel, "graph", None), "trace", None)
    store = getattr(trace, "store", None)
    run_dir = getattr(store, "run_dir", None)
    if run_dir:
        return str(run_dir)
    path = getattr(trace, "path", None)
    if path:
        return str(path)
    return ""


def _trace_summary(kernel: Any, agent_result: Any) -> dict[str, Any]:
    report = getattr(agent_result, "final_report", None)
    if report is not None and hasattr(report, "to_dict"):
        payload = report.to_dict()
        summary = payload.get("trace_summary")
        if isinstance(summary, dict):
            return summary
    trace = getattr(getattr(kernel, "graph", None), "trace", None)
    context = getattr(kernel, "context", None)
    task_id = getattr(getattr(context, "identity", None), "task_id", None)
    if trace is not None and hasattr(trace, "final_report_summary"):
        try:
            return trace.final_report_summary(task_id=task_id)
        except Exception:
            return {}
    return {}


def _trace_summary_from_kernel(kernel: Any) -> dict[str, Any]:
    trace = getattr(getattr(kernel, "graph", None), "trace", None)
    context = getattr(kernel, "context", None)
    task_id = getattr(getattr(context, "identity", None), "task_id", None)
    if trace is not None and hasattr(trace, "final_report_summary"):
        try:
            return trace.final_report_summary(task_id=task_id)
        except Exception:
            return {}
    return {}


def _agent_status_from_trace(trace: Path | None) -> str:
    events = _read_trace_events(trace)
    for event in reversed(events):
        payload = _event_payload(event)
        outcome = payload.get("execution_outcome")
        if isinstance(outcome, dict):
            status = str(outcome.get("status") or "")
            error_code = str(outcome.get("error_code") or "")
            if error_code == "max_turns_exceeded":
                return "blocked"
            if status:
                return status
        if _event_type(event) == "task.failed":
            return "failed"
    return ""


def _tool_calls_from_trace(trace: Path | None, trace_summary: dict[str, Any]) -> int:
    value = _safe_int(trace_summary.get("tool_calls")) if isinstance(trace_summary, dict) else 0
    if value:
        return value
    return _count_events(_read_trace_events(trace), "tool_protocol.call_started")


def _turn_count_from_trace(trace: Path | None, usage: dict[str, Any]) -> int:
    value = _safe_int(usage.get("requests")) or _safe_int(usage.get("responses"))
    if value:
        return value
    events = _read_trace_events(trace)
    return _count_events(events, "model.request.created")


def _infrastructure_blocked(agent_result: Any, *, usage: dict[str, Any], tool_calls: int) -> bool:
    status = _agent_status(agent_result)
    if status != "failed" or _safe_int(usage.get("input_tokens")) or tool_calls:
        return False
    answer = str(getattr(agent_result, "final_answer", "") or "").lower()
    return any(marker in answer for marker in ("winerror 10013", "network", "socket", "访问权限不允许"))


def _sandbox_environment_blocked(kernel: Any, *, agent_status: str) -> bool:
    if agent_status not in {"blocked", "failed"}:
        return False
    planner = getattr(getattr(kernel, "graph", None), "planner", None)
    evidence = getattr(planner, "evidence", None)
    observations = getattr(evidence, "sandbox_observations", None) or []
    for observation in observations:
        if not isinstance(observation, dict):
            continue
        if observation.get("source") not in {"command", "verification"}:
            continue
        if not observation.get("sandbox_id"):
            continue
        if (
            observation.get("status") == "backend_unavailable"
            or observation.get("enforcement_status") == "backend_unavailable"
        ):
            return True
    return False


def _sandbox_environment_blocker_reason(kernel: Any) -> str:
    planner = getattr(getattr(kernel, "graph", None), "planner", None)
    state = getattr(planner, "state", None)
    for reason in getattr(state, "blocked_reasons", None) or []:
        normalized = str(reason).strip().lower()
        if "sandbox" in normalized and (
            "backend unavailable" in normalized or "backend_unavailable" in normalized
        ):
            return str(reason)
    return "sandbox backend unavailable: required OS isolation could not be enforced"


def _permission_profile_for_task(task: EvaluationTask) -> PermissionProfileName:
    value = str(task.strategy.get("permission_profile") or "").strip().lower()
    if not value:
        value = "read-only" if task.tool_policy == "read_only" else "workspace-write"
    return PermissionProfileName(value)


def _approval_policy_for_task(task: EvaluationTask) -> ApprovalPolicy:
    value = str(task.strategy.get("approval_policy") or "").strip().lower()
    if not value:
        value = "on-request" if task.tool_policy == "review_all" else "never"
    return ApprovalPolicy(value)


def _network_access_for_task(task: EvaluationTask) -> NetworkAccess:
    value = str(task.strategy.get("network_access") or "denied").strip().lower()
    return NetworkAccess(value)


def _strategy_max_turns_for_task(task: EvaluationTask) -> int | None:
    raw_value = task.strategy.get("max_turns")
    if raw_value in (None, ""):
        return None
    try:
        value = int(str(raw_value))
    except (TypeError, ValueError) as exc:
        raise ValueError(f"evaluation task {task.task_id} has invalid strategy.max_turns.") from exc
    if value <= 0:
        raise ValueError(f"evaluation task {task.task_id} has invalid strategy.max_turns.")
    return value


def _apply_benchmark_constraints(kernel: Any, task: EvaluationTask) -> None:
    planner = getattr(getattr(kernel, "graph", None), "planner", None)
    apply_constraints = getattr(planner, "apply_benchmark_constraints", None)
    if not callable(apply_constraints):
        return
    apply_constraints(_model_visible_benchmark_constraints(task))


def _model_visible_benchmark_constraints(task: EvaluationTask) -> dict[str, Any]:
    verification_command = _model_visible_verification_command(task)
    if _requires_baseline_verification(task) and not task.model_visible_verification_command:
        verification_command = ""
    return {
        "task_id": task.task_id,
        "allowed_tools": task.allowed_tools,
        "expected_file_changes": task.expected_file_changes,
        "completion_standard": task.completion_standard,
        "risk_tags": task.risk_tags,
        "verification_command": verification_command,
    }


def _sandbox_enforcement_audit(
    task: EvaluationTask,
    events: list[dict[str, Any]],
    trace_summary: dict[str, Any],
) -> dict[str, Any]:
    required = task.task_type == "public_representative"
    fallback_count = _local_process_fallback_count(events, trace_summary)
    passed = not required or fallback_count == 0
    return {
        "passed": passed,
        "required": required,
        "local_process_fallback_count": fallback_count,
        "reason": "" if passed else "sandbox-required evaluation used a local process fallback",
    }


def _evaluator_visibility_audit(
    task: EvaluationTask,
    trace: Path | None,
) -> dict[str, Any]:
    required = task.task_type == "public_representative"
    if not required:
        return {"passed": True, "status": "not_applicable", "reason": ""}
    if trace is None:
        return {
            "passed": False,
            "status": "unavailable",
            "reason": "model-visible trace was unavailable for evaluator visibility audit",
        }
    projection = {
        "goal": _task_goal(task),
        "constraints": _model_visible_benchmark_constraints(task),
    }
    serialized_projection = json.dumps(projection, ensure_ascii=False, sort_keys=True, default=str)
    serialized_trace = json.dumps(_read_trace_events(trace), ensure_ascii=False, sort_keys=True, default=str)
    combined = f"{serialized_projection}\n{serialized_trace}"
    forbidden_keys = ("fixture_metadata", "hidden_test_patch", "test_patch")
    forbidden_payloads = [
        task.test_patch,
        json.dumps(task.fixture_metadata, ensure_ascii=False, sort_keys=True, default=str)
        if task.fixture_metadata
        else "",
        json.dumps(task.hidden_test_patch, ensure_ascii=False, sort_keys=True, default=str)
        if task.hidden_test_patch
        else "",
    ]
    leaked = any(key in combined for key in forbidden_keys) or any(
        payload and payload in combined for payload in forbidden_payloads
    )
    return {
        "passed": not leaked,
        "status": "passed" if not leaked else "leak_detected",
        "reason": ""
        if not leaked
        else "evaluator-only metadata appeared in model-visible projection or trace",
    }


def _agent_status(agent_result: Any) -> str:
    return str(getattr(getattr(agent_result, "status", None), "value", getattr(agent_result, "status", "")))


def _final_report_payload(agent_result: Any) -> dict[str, Any]:
    report = getattr(agent_result, "final_report", None)
    if report is None:
        return {}
    if hasattr(report, "to_dict"):
        payload = report.to_dict()
        return payload if isinstance(payload, dict) else {}
    if isinstance(report, dict):
        return dict(report)
    return {}


def _turn_count(agent_result: Any, usage: dict[str, Any]) -> int:
    direct = _safe_int(getattr(agent_result, "turn", 0))
    if direct:
        return direct
    return _safe_int(usage.get("requests")) or _safe_int(usage.get("responses"))


def _final_report_status(payload: dict[str, Any], *, agent_status: str) -> str:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    if isinstance(planner, dict) and planner.get("status"):
        return str(planner["status"])
    lifecycle = payload.get("lifecycle_summary") if isinstance(payload, dict) else {}
    if isinstance(lifecycle, dict) and lifecycle.get("status"):
        return str(lifecycle["status"])
    if isinstance(payload, dict) and payload.get("kernel_status"):
        return str(payload["kernel_status"])
    return agent_status


def _evaluation_passed_from_payload(payload: dict[str, Any]) -> bool:
    return bool(payload.get("evaluation_passed"))


def _payload_tool_calls(payload: dict[str, Any]) -> int:
    return _safe_int(payload.get("tool_calls"))


def _failure_repair_summary(payload: dict[str, Any]) -> dict[str, Any]:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    failure_repair = planner.get("failure_repair_summary") if isinstance(planner, dict) else {}
    return failure_repair if isinstance(failure_repair, dict) else {}


def _final_report_environment_blocked(payload: dict[str, Any]) -> bool:
    category = str(_failure_repair_summary(payload).get("latest_failure_category") or "")
    return category in {"environment_error", "sandbox_limitation"}


def _final_report_environment_blocker_reason(payload: dict[str, Any]) -> str:
    return _blocked_reason(payload, agent_status="blocked", errors=[], verification=None) or "environment_error"


def _repair_attempt_count(payload: dict[str, Any]) -> int:
    return _safe_int(_failure_repair_summary(payload).get("repair_attempt_count"))


def _repair_execution_count(payload: dict[str, Any]) -> int:
    return _safe_int(_failure_repair_summary(payload).get("repair_execution_count"))


def _agent_completed(final_report_status: str, *, agent_status: str) -> bool:
    return final_report_status == "completed" or agent_status == "completed"


def _blocked_reason(
    payload: dict[str, Any],
    *,
    agent_status: str,
    errors: list[str],
    verification: CommandEvalResult | None,
) -> str:
    failure_repair = _failure_repair_summary(payload)
    for key in ("latest_blocked_reason", "blocked_reason"):
        value = failure_repair.get(key)
        if value:
            return str(value)
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    if isinstance(planner, dict):
        reasons = planner.get("blocking_reasons")
        if isinstance(reasons, list) and reasons:
            return "; ".join(str(item) for item in reasons)
        if planner.get("blocked_reason"):
            return str(planner["blocked_reason"])
    if agent_status in {"blocked", "failed", "max_turns_exceeded"} and errors:
        return errors[0]
    if verification is not None and not verification.passed:
        return verification.error_summary or verification.failure_category
    return ""


def _failure_category(
    payload: dict[str, Any],
    *,
    status: str,
    verification: CommandEvalResult | None,
    infrastructure_blocked: bool,
    policy_blocks: int,
    errors: list[str],
) -> str:
    if status == "success":
        return "none"
    if infrastructure_blocked:
        return "environment_blocker"
    failure_repair = _failure_repair_summary(payload)
    if failure_repair.get("latest_failure_category"):
        category = str(failure_repair["latest_failure_category"])
        if category in {"environment_error", "sandbox_limitation"}:
            return "environment_blocker"
        return category
    if policy_blocks:
        return "policy_blocked"
    if status in {"blocked", "failed", "max_turns_exceeded"}:
        return status
    if verification is not None and verification.failure_category and verification.failure_category != "none":
        return verification.failure_category
    if status in {"success", "unknown"} and not errors:
        return "none"
    return status or "failure"


def _policy_blocks(payload: dict[str, Any], trace_summary: dict[str, Any]) -> int:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    policy = payload.get("policy_summary") if isinstance(payload, dict) else {}
    shutdown = payload.get("shutdown_summary") if isinstance(payload, dict) else {}
    if isinstance(planner, dict):
        execution = planner.get("execution_trace_summary")
        if isinstance(execution, dict) and execution.get("policy_denials") is not None:
            return _safe_int(execution.get("policy_denials"))
    if isinstance(policy, dict):
        value = (
            policy.get("denied_actions_count")
            or policy.get("skipped_actions_due_to_policy")
            or policy.get("sandbox_required_actions_count")
        )
        if value is not None:
            return _safe_int(value)
    if isinstance(shutdown, dict):
        failures = shutdown.get("component_failures") or shutdown.get("policy_failures") or []
        if isinstance(failures, list):
            return len([item for item in failures if "policy" in str(item).lower()])
    return _safe_int(trace_summary.get("policy_denials"))


def _trace_artifact_refs(payload: dict[str, Any], trace_summary: dict[str, Any]) -> list[str]:
    refs: list[str] = []
    for value in trace_summary.get("key_artifacts") or []:
        refs.append(str(value))
    for value in payload.get("artifacts") or []:
        refs.append(str(value))
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    if isinstance(planner, dict):
        for value in planner.get("artifacts") or []:
            refs.append(str(value))
        artifact = planner.get("artifact_ref")
        if artifact:
            refs.append(str(artifact))
        execution = planner.get("execution_trace_summary")
        if isinstance(execution, dict):
            for value in execution.get("key_artifacts") or []:
                refs.append(str(value))
    return sorted(dict.fromkeys(refs))


def _build_capability_summary(
    *,
    trace: Path | None,
    trace_summary: dict[str, Any],
    checks: dict[str, Any],
    verification: CommandEvalResult | None,
    public_verification: CommandEvalResult | None,
    hidden_verification: CommandEvalResult | None,
    final_report_status: str,
    agent_status: str,
    wall_time_seconds: float,
    evaluation_timing: dict[str, float | None] | None = None,
) -> dict[str, Any]:
    events = _read_trace_events(trace)
    model_requests = _count_events(events, "model.request.created")
    model_results = _count_events(events, "model.response.received") + _count_events(events, "model.request.failed")
    tool_call_envelopes = _count_events(events, "model.tool_call.proposed") + _count_events(events, "tool_protocol.call_started")
    tool_results = _count_events(events, "tool_protocol.call_completed") + _count_events(events, "tool_protocol.result")
    tool_observations = _count_events_with_contains(events, ("tool_observation", "tool.observation"))
    retrieval_calls = _count_events_with_contains(events, ("retrieval",))
    context_rebuilds = _count_events(events, "context.bundle_built") + _count_events(events, "context.rendered_for_model")
    compaction_requested = _count_events(events, "context.compaction_requested")
    compaction_completed = _count_events(events, "context.compaction_completed")
    compaction_failed = _count_events(events, "context.compaction_failed")
    compaction_reason = _compaction_reason(events, compaction_requested=compaction_requested)
    sandbox_backend = _sandbox_backend(events, trace_summary)
    local_fallback_count = _local_process_fallback_count(events, trace_summary)
    sandbox_seconds = _sandbox_time_seconds(events)
    provider_seconds = _provider_time_seconds(events, trace_summary)
    context_seconds = _context_retrieval_compaction_time_seconds(events)
    pytest_seconds = _pytest_time_seconds(public_verification, hidden_verification, verification)
    verification_seconds = _verification_time_seconds(public_verification, hidden_verification, verification)
    verification_checks = _ordered_verification_checks(checks)
    usage = trace_summary.get("model_usage_summary") if isinstance(trace_summary, dict) else {}
    if isinstance(usage, dict):
        model_requests = model_requests or _safe_int(usage.get("requests"))
        model_results = model_results or _safe_int(usage.get("responses")) or model_requests
    trace_timing = _trace_timing_details(events)
    measured_timing = {**trace_timing, **(evaluation_timing or {})}
    detailed_timing, timing_diagnostics = _capability_timing_details(measured_timing)
    wall_phases = _wall_phase_timings(evaluation_timing or {})
    unattributed_time = round(
        max(0.0, wall_time_seconds - sum(wall_phases.values())),
        3,
    )
    return {
        "schema_version": "evaluation.capability_summary/v2",
        "model_turn_request_count": model_requests,
        "model_turn_result_count": model_results,
        "tool_call_envelope_count": tool_call_envelopes,
        "tool_result_count": tool_results,
        "tool_observation_count": tool_observations,
        "retrieval_calls": retrieval_calls,
        "context_package_rebuild_count": context_rebuilds,
        "context_compaction": {
            "requested": compaction_requested,
            "skipped": compaction_requested == 0,
            "completed": compaction_completed,
            "failed": compaction_failed,
            "reason": compaction_reason,
        },
        "sandbox_backend": sandbox_backend,
        "local_process_fallback_count": local_fallback_count,
        "verification_checks": verification_checks,
        "final_report_status": final_report_status,
        "agent_loop_result_status": agent_status,
        "provider_time_by_turn": _provider_time_by_turn(events),
        "turn_diagnostics": _turn_diagnostics(events),
        "sandbox_commands": _sandbox_command_timings(events),
        "sandbox_breakdown": _sandbox_breakdown(events),
        "wall_phases": wall_phases,
        "unattributed_time_seconds": unattributed_time,
        "timing": {
            "wall_time_seconds": wall_time_seconds,
            "provider_time_seconds": provider_seconds,
            "sandbox_time_seconds": sandbox_seconds,
            "context_retrieval_compaction_time_seconds": context_seconds,
            "pytest_time_seconds": pytest_seconds,
            "verification_time_seconds": verification_seconds,
            **detailed_timing,
        },
        "timing_diagnostics": timing_diagnostics,
    }


_CAPABILITY_SLA_THRESHOLDS_SECONDS = {
    "wall": 300.0,
    "agent_loop": 210.0,
    "provider": 55.0,
    "sandbox": 50.0,
    "dependency_setup": 35.0,
}
_CAPABILITY_SLA_OPTIONAL_THRESHOLDS_SECONDS = {
    "verification": 10.0,
    "unattributed_time": 15.0,
}


def _build_capability_sla(capability_summary: dict[str, Any]) -> dict[str, Any]:
    timing = capability_summary.get("timing") if isinstance(capability_summary, dict) else {}
    timing = timing if isinstance(timing, dict) else {}
    wall_phases = capability_summary.get("wall_phases") if isinstance(capability_summary, dict) else {}
    wall_phases = wall_phases if isinstance(wall_phases, dict) else {}
    items: dict[str, dict[str, Any]] = {
        "wall": _duration_sla_item(
            timing.get("wall_time_seconds"),
            target_seconds=_CAPABILITY_SLA_THRESHOLDS_SECONDS["wall"],
            source="capability_summary.timing.wall_time_seconds",
        ),
        "agent_loop": _duration_sla_item(
            wall_phases.get("agent_loop_time_seconds"),
            target_seconds=_CAPABILITY_SLA_THRESHOLDS_SECONDS["agent_loop"],
            source="capability_summary.wall_phases.agent_loop_time_seconds",
        ),
        "provider": _duration_sla_item(
            timing.get("provider_time_seconds"),
            target_seconds=_CAPABILITY_SLA_THRESHOLDS_SECONDS["provider"],
            source="capability_summary.timing.provider_time_seconds",
        ),
        "sandbox": _duration_sla_item(
            timing.get("sandbox_time_seconds"),
            target_seconds=_CAPABILITY_SLA_THRESHOLDS_SECONDS["sandbox"],
            source="capability_summary.timing.sandbox_time_seconds",
        ),
        "dependency_setup": _duration_sla_item(
            timing.get("dependency_setup_time_seconds"),
            target_seconds=_CAPABILITY_SLA_THRESHOLDS_SECONDS["dependency_setup"],
            source="capability_summary.timing.dependency_setup_time_seconds",
        ),
        "verification": _duration_sla_item(
            timing.get("verification_time_seconds"),
            target_seconds=_CAPABILITY_SLA_OPTIONAL_THRESHOLDS_SECONDS["verification"],
            source="capability_summary.timing.verification_time_seconds",
        ),
        "unattributed_time": _duration_sla_item(
            capability_summary.get("unattributed_time_seconds"),
            target_seconds=_CAPABILITY_SLA_OPTIONAL_THRESHOLDS_SECONDS["unattributed_time"],
            source="capability_summary.unattributed_time_seconds",
        ),
        "local_fallback": _count_sla_item(
            capability_summary.get("local_process_fallback_count"),
            target_count=0,
            source="capability_summary.local_process_fallback_count",
        ),
        "visibility_audit": _boolean_sla_item(
            ((capability_summary.get("evaluator_visibility_audit") or {}).get("passed"))
            if isinstance(capability_summary.get("evaluator_visibility_audit"), dict)
            else None,
            source="capability_summary.evaluator_visibility_audit.passed",
        ),
    }
    violations = [name for name, item in items.items() if item.get("status") == "over_sla"]
    unavailable = [name for name, item in items.items() if item.get("status") == "unavailable"]
    return {
        "schema_version": "evaluation.capability_sla/v1",
        "status": "over_sla" if violations else "unavailable" if unavailable else "within_sla",
        "blocking": False,
        "violations": violations,
        "items": items,
    }


def _duration_sla_item(value: Any, *, target_seconds: float, source: str) -> dict[str, Any]:
    if not isinstance(value, int | float):
        return {
            "actual_seconds": None,
            "target_seconds": target_seconds,
            "status": "unavailable",
            "delta_seconds": None,
            "blocking": False,
            "source": source,
        }
    actual = round(float(value), 3)
    delta = round(actual - target_seconds, 3)
    return {
        "actual_seconds": actual,
        "target_seconds": target_seconds,
        "status": "over_sla" if delta > 0 else "within_sla",
        "delta_seconds": delta,
        "blocking": False,
        "source": source,
    }


def _count_sla_item(value: Any, *, target_count: int, source: str) -> dict[str, Any]:
    if not isinstance(value, int):
        return {
            "actual_count": None,
            "target_count": target_count,
            "status": "unavailable",
            "delta_count": None,
            "blocking": False,
            "source": source,
        }
    delta = value - target_count
    return {
        "actual_count": value,
        "target_count": target_count,
        "status": "over_sla" if delta > 0 else "within_sla",
        "delta_count": delta,
        "blocking": False,
        "source": source,
    }


def _boolean_sla_item(value: Any, *, source: str) -> dict[str, Any]:
    if not isinstance(value, bool):
        return {
            "passed": None,
            "status": "unavailable",
            "blocking": False,
            "source": source,
        }
    return {
        "passed": value,
        "status": "passed" if value else "over_sla",
        "blocking": False,
        "source": source,
    }


def _summarize_capability_sla(results: list[EvaluationTaskResult]) -> dict[str, Any]:
    items: dict[str, dict[str, Any]] = {}
    violations: dict[str, int] = {}
    unavailable: dict[str, int] = {}
    for result in results:
        sla = result.capability_sla or {}
        if not isinstance(sla, dict):
            continue
        for name, item in (sla.get("items") or {}).items():
            if not isinstance(item, dict):
                continue
            items[str(name)] = _merge_sla_item(items.get(str(name)), item)
            status = str(item.get("status") or "")
            if status == "over_sla":
                violations[str(name)] = violations.get(str(name), 0) + 1
            elif status == "unavailable":
                unavailable[str(name)] = unavailable.get(str(name), 0) + 1
    return {
        "schema_version": "evaluation.capability_sla_summary/v1",
        "status": "over_sla" if violations else "unavailable" if unavailable else "within_sla",
        "blocking": False,
        "task_count": len(results),
        "violations": dict(sorted(violations.items())),
        "unavailable": dict(sorted(unavailable.items())),
        "items": items,
    }


def _merge_sla_item(previous: dict[str, Any] | None, current: dict[str, Any]) -> dict[str, Any]:
    if previous is None:
        return dict(current)
    if previous.get("status") != "over_sla" and current.get("status") == "over_sla":
        return dict(current)
    if previous.get("status") == "unavailable" and current.get("status") != "unavailable":
        return dict(current)
    previous_delta = _safe_float(previous.get("delta_seconds"))
    current_delta = _safe_float(current.get("delta_seconds"))
    if current_delta > previous_delta:
        return dict(current)
    previous_count_delta = _safe_int(previous.get("delta_count"))
    current_count_delta = _safe_int(current.get("delta_count"))
    if current_count_delta > previous_count_delta:
        return dict(current)
    return previous


def _build_evaluation_metrics(
    *,
    task: EvaluationTask,
    evaluation_passed: bool,
    tests_passed: bool,
    public_verification_passed: bool,
    hidden_verification_passed: bool,
    patch_applicable: bool,
    allowed_scope_passed: bool,
    patch_applied: bool,
    files_changed: list[str],
    patch: dict[str, Any],
    checks: dict[str, Any],
    verification: CommandEvalResult | None,
    capability_summary: dict[str, Any],
    trace: Path | None,
    token_usage: dict[str, Any],
    cache_usage: dict[str, Any],
    turn_count: int,
    tool_calls: int,
    agent_completed: bool,
    miscompletion_count: int,
    repair_attempt_count: int,
    repair_execution_count: int,
    blocked_reason: str,
    failure_category: str,
    final_report_status: str,
    agent_status: str,
    policy_blocks: int,
    trace_artifact_refs: list[str],
    reproducible_environment: dict[str, Any],
    baseline_failed: bool,
    baseline_checks: dict[str, Any],
    fail_to_pass_satisfied: bool,
    verification_misconfiguration_reason: str,
    error_summary: str,
) -> dict[str, Any]:
    trace_events = _read_trace_events(trace)
    model_profile = {}
    if isinstance(reproducible_environment.get("model_profile"), dict):
        model_profile = dict(reproducible_environment["model_profile"])
    reason = _resolved_reason(
        evaluation_passed=evaluation_passed,
        failure_category=failure_category,
        blocked_reason=blocked_reason,
        verification_misconfiguration_reason=verification_misconfiguration_reason,
        error_summary=error_summary,
    )
    return {
        "schema_version": EVALUATION_METRICS_SCHEMA_VERSION,
        "resolved": _resolved_metrics(evaluation_passed=evaluation_passed, reason=reason),
        "swe_bench": _swe_bench_metrics(
            task=task,
            baseline_failed=baseline_failed,
            baseline_checks=baseline_checks,
            fail_to_pass_satisfied=fail_to_pass_satisfied,
        ),
        "verification": _verification_metrics(
            tests_passed=tests_passed,
            public_verification_passed=public_verification_passed,
            hidden_verification_passed=hidden_verification_passed,
            checks=checks,
            verification=verification,
            capability_summary=capability_summary,
        ),
        "patch": _patch_metrics(
            patch=patch,
            files_changed=files_changed,
            expected_file_changes=task.expected_file_changes,
            allowed_paths=task.allowed_paths,
            patch_applicable=patch_applicable,
            allowed_scope_passed=allowed_scope_passed,
            patch_applied=patch_applied,
        ),
        "trajectory": _trajectory_metrics(
            trace_events=trace_events,
            agent_completed=agent_completed,
            turn_count=turn_count,
            miscompletion_count=miscompletion_count,
            repair_attempt_count=repair_attempt_count,
            repair_execution_count=repair_execution_count,
            blocked_reason=blocked_reason,
            failure_category=failure_category,
            final_report_status=final_report_status,
            agent_status=agent_status,
        ),
        "tools": _tool_metrics_from_trace_events(trace_events, fallback_tool_calls=tool_calls),
        "context": _context_metrics(
            trace_events=trace_events,
            capability_summary=capability_summary,
            expected_file_changes=task.expected_file_changes,
            allowed_paths=task.allowed_paths,
            cache_usage=cache_usage,
        ),
        "efficiency": _efficiency_metrics(
            capability_summary=capability_summary,
            token_usage=token_usage,
            cache_usage=cache_usage,
        ),
        "cost": _cost_metrics(
            trace_events=trace_events,
            token_usage=token_usage,
            model_profile=model_profile,
        ),
        "safety": _safety_metrics(
            policy_blocks=policy_blocks,
            capability_summary=capability_summary,
            trace_events=trace_events,
        ),
        "reproducibility": _reproducibility_metrics(
            reproducible_environment=reproducible_environment,
            trace_artifact_refs=trace_artifact_refs,
        ),
    }


def _resolved_reason(
    *,
    evaluation_passed: bool,
    failure_category: str,
    blocked_reason: str,
    verification_misconfiguration_reason: str,
    error_summary: str,
) -> str:
    if evaluation_passed:
        return ""
    return (
        verification_misconfiguration_reason
        or blocked_reason
        or (failure_category if failure_category and failure_category != "none" else "")
        or error_summary
        or "evaluation did not pass"
    )


def _resolved_metrics(*, evaluation_passed: bool, reason: str) -> dict[str, Any]:
    return {
        "value": evaluation_passed,
        "resolved_rate_contribution": 1.0 if evaluation_passed else 0.0,
        "reason": "" if evaluation_passed else reason,
    }


def _swe_bench_metrics(
    *,
    task: EvaluationTask,
    baseline_failed: bool,
    baseline_checks: dict[str, Any],
    fail_to_pass_satisfied: bool,
) -> dict[str, Any]:
    pass_to_pass = task.fixture_metadata.get("pass_to_pass") or task.fixture_metadata.get("PASS_TO_PASS")
    if pass_to_pass:
        pass_to_pass_checks = [str(item) for item in pass_to_pass] if isinstance(pass_to_pass, list) else [str(pass_to_pass)]
        pass_to_pass_payload: dict[str, Any] = {
            "satisfied": None,
            "status": "not_implemented",
            "reason": "PASS_TO_PASS checks are configured but this runner does not yet record PASS_TO_PASS evaluator results",
            "checks": pass_to_pass_checks,
        }
    else:
        pass_to_pass_payload = {
            "satisfied": None,
            "status": "not_configured",
            "reason": "manifest has no PASS_TO_PASS checks",
        }
    fail_to_pass_checks = (
        task.fixture_metadata.get("fail_to_pass")
        or task.fixture_metadata.get("FAIL_TO_PASS")
        or []
    )
    return {
        "fail_to_pass": {
            "satisfied": fail_to_pass_satisfied,
            "baseline_failed": baseline_failed,
            "checks": list(fail_to_pass_checks) if isinstance(fail_to_pass_checks, list) else [str(fail_to_pass_checks)],
            "baseline_checks": _scorecard_baseline_checks(baseline_checks),
        },
        "pass_to_pass": pass_to_pass_payload,
    }


def _scorecard_baseline_checks(baseline_checks: dict[str, Any]) -> dict[str, Any]:
    safe: dict[str, Any] = {}
    for name in ("public", "hidden"):
        check = baseline_checks.get(name)
        if not isinstance(check, dict):
            continue
        safe[name] = {
            "passed": check.get("passed"),
            "status": check.get("status"),
            "failure_category": check.get("failure_category"),
        }
    return safe


def _verification_metrics(
    *,
    tests_passed: bool,
    public_verification_passed: bool,
    hidden_verification_passed: bool,
    checks: dict[str, Any],
    verification: CommandEvalResult | None,
    capability_summary: dict[str, Any],
) -> dict[str, Any]:
    configured = [name for name in ("public", "hidden") if isinstance(checks.get(name), dict)]
    passed = sum(1 for name in configured if _check_passed(checks, name))
    timing = capability_summary.get("timing") if isinstance(capability_summary, dict) else {}
    return {
        "tests_passed": tests_passed,
        "public_verification_passed": public_verification_passed,
        "hidden_verification_passed": hidden_verification_passed,
        "verification_pass_rate": _rate(passed, len(configured)) if configured else None,
        "verification_time_seconds": _safe_float(timing.get("verification_time_seconds")) if isinstance(timing, dict) else 0.0,
        "pytest_time_seconds": _safe_float(timing.get("pytest_time_seconds")) if isinstance(timing, dict) else 0.0,
        "reason": "" if tests_passed else _verification_reason(checks, verification),
    }


def _verification_reason(checks: dict[str, Any], verification: CommandEvalResult | None) -> str:
    for name in ("public", "hidden"):
        check = checks.get(name)
        if isinstance(check, dict) and check.get("status") not in {"passed", "not_run"}:
            return str(check.get("error_summary") or check.get("failure_category") or f"{name} verification failed")
    if verification is not None and not verification.passed:
        return verification.error_summary or verification.failure_category or "verification failed"
    return ""


def _patch_metrics(
    *,
    patch: dict[str, Any],
    files_changed: list[str],
    expected_file_changes: list[str],
    allowed_paths: list[str],
    patch_applicable: bool,
    allowed_scope_passed: bool,
    patch_applied: bool,
) -> dict[str, Any]:
    displayed_files = [_display_path(path) for path in files_changed]
    out_of_scope = _out_of_scope_files(files_changed, allowed_paths)
    added, deleted, diff_reason = _diff_line_counts(str(patch.get("diff") or ""))
    return {
        "patch_applied": patch_applied,
        "patch_applicable": patch_applicable,
        "allowed_scope_passed": allowed_scope_passed,
        "files_changed_count": len(files_changed),
        "expected_files_changed": _expected_file_changes_satisfied(expected_file_changes, files_changed=files_changed)
        if expected_file_changes
        else None,
        "test_files_modified": any(_looks_like_test_path(path) for path in displayed_files),
        "out_of_scope_files": [_display_path(path) for path in out_of_scope],
        "diff_added_lines": added,
        "diff_deleted_lines": deleted,
        "reason": diff_reason,
    }


def _trajectory_metrics(
    *,
    trace_events: list[dict[str, Any]],
    agent_completed: bool,
    turn_count: int,
    miscompletion_count: int,
    repair_attempt_count: int,
    repair_execution_count: int,
    blocked_reason: str,
    failure_category: str,
    final_report_status: str,
    agent_status: str,
) -> dict[str, Any]:
    entered_loop = bool(
        turn_count
        or _count_events(trace_events, "model.request.created")
        or final_report_status
        or agent_status
    )
    return {
        "entered_agent_loop": entered_loop,
        "agent_completed": agent_completed,
        "turn_count": turn_count,
        "miscompletion_count": miscompletion_count,
        "repair_attempt_count": repair_attempt_count,
        "repair_execution_count": repair_execution_count,
        "blocked_reason": blocked_reason,
        "failure_category": failure_category,
        "final_report_status": final_report_status,
        "agent_loop_result_status": agent_status,
    }


def _tool_metrics_from_trace_events(
    events: list[dict[str, Any]],
    *,
    fallback_tool_calls: int = 0,
) -> dict[str, Any]:
    call_events = [event for event in events if _event_type(event) in {"tool_protocol.call_started", "model.tool_call.proposed"}]
    result_events = [
        event
        for event in events
        if _event_type(event) in {"tool_protocol.call_completed", "tool_protocol.result", "tool.result"}
    ]
    call_ids = {
        call_id
        for event in call_events
        for call_id in [_tool_call_id_from_event(event)]
        if call_id
    }
    call_count = len(call_ids) if call_ids else len(call_events)
    tool_names = sorted(
        dict.fromkeys(
            name
            for event in [*call_events, *result_events]
            for name in [_tool_name_from_event(event)]
            if name
        )
    )
    success = failure = unknown_results = 0
    for event in result_events:
        status = _tool_result_status(event)
        if status is True:
            success += 1
        elif status is False:
            failure += 1
        else:
            unknown_results += 1
    tool_call_count = call_count or fallback_tool_calls
    unknown_calls = max(tool_call_count - len(result_events), 0)
    total_unknown = unknown_results + unknown_calls
    if tool_call_count <= 0:
        success_rate: float | None = None
    elif total_unknown:
        success_rate = None
    else:
        success_rate = _rate(success, tool_call_count)
    return {
        "tool_call_count": tool_call_count,
        "tool_result_count": len(result_events),
        "tool_success_count": success,
        "tool_failure_count": failure,
        "tool_unknown_count": total_unknown,
        "tool_success_rate": success_rate,
        "distinct_tool_names": tool_names,
    }


def _context_metrics(
    *,
    trace_events: list[dict[str, Any]],
    capability_summary: dict[str, Any],
    expected_file_changes: list[str],
    allowed_paths: list[str],
    cache_usage: dict[str, Any],
) -> dict[str, Any]:
    compaction = capability_summary.get("context_compaction") if isinstance(capability_summary, dict) else {}
    retrieval_calls = _safe_int(capability_summary.get("retrieval_calls")) if isinstance(capability_summary, dict) else 0
    context_rebuilds = _safe_int(capability_summary.get("context_package_rebuild_count")) if isinstance(capability_summary, dict) else 0
    target_hit = _target_file_retrieval_hit(
        trace_events,
        target_files=expected_file_changes or allowed_paths,
    )
    return {
        "retrieval_calls": retrieval_calls,
        "target_file_retrieval_hit": target_hit["value"],
        "target_file_retrieval_reason": target_hit["reason"],
        "context_package_rebuild_count": context_rebuilds,
        "compaction": dict(compaction)
        if isinstance(compaction, dict)
        else {
            "requested": 0,
            "completed": 0,
            "failed": 0,
            "skipped": True,
            "reason": _compaction_reason(trace_events, compaction_requested=0),
        },
        "request_cache_hit_rate": _safe_float(cache_usage.get("request_cache_hit_rate")),
        "run_cache_hit_rate": _safe_float(cache_usage.get("run_cache_hit_rate")),
    }


def _efficiency_metrics(
    *,
    capability_summary: dict[str, Any],
    token_usage: dict[str, Any],
    cache_usage: dict[str, Any],
) -> dict[str, Any]:
    timing = capability_summary.get("timing") if isinstance(capability_summary, dict) else {}
    result = {
        "wall_time_seconds": _safe_float(timing.get("wall_time_seconds")) if isinstance(timing, dict) else 0.0,
        "provider_time_seconds": _safe_float(timing.get("provider_time_seconds")) if isinstance(timing, dict) else 0.0,
        "sandbox_time_seconds": _safe_float(timing.get("sandbox_time_seconds")) if isinstance(timing, dict) else 0.0,
        "context_retrieval_compaction_time_seconds": _safe_float(timing.get("context_retrieval_compaction_time_seconds"))
        if isinstance(timing, dict)
        else 0.0,
        "verification_time_seconds": _safe_float(timing.get("verification_time_seconds")) if isinstance(timing, dict) else 0.0,
        "prompt_tokens": _safe_int(token_usage.get("input_tokens")),
        "cached_tokens": _safe_int(token_usage.get("cached_input_tokens")),
        "output_tokens": _safe_int(token_usage.get("output_tokens")),
        "total_tokens": _safe_int(token_usage.get("total_tokens")),
        "request_cache_hit_rate": _safe_float(cache_usage.get("request_cache_hit_rate")),
        "run_cache_hit_rate": _safe_float(cache_usage.get("run_cache_hit_rate")),
    }
    if not result["total_tokens"]:
        result["total_tokens"] = result["prompt_tokens"] + result["output_tokens"]
    return result


def _cost_metrics(
    *,
    trace_events: list[dict[str, Any]],
    token_usage: dict[str, Any],
    model_profile: dict[str, Any],
) -> dict[str, Any]:
    provider_cost = _provider_cost_estimate(trace_events)
    model_name = _safe_str(model_profile.get("model")).strip()
    base_url = _safe_str(model_profile.get("base_url")).strip()
    matched_model = _pricing_model_key(model_name)
    pricing = _TOKEN_PRICING_PER_1M.get(matched_model or "")
    if provider_cost is not None:
        return _cost_payload(
            cost_estimate=provider_cost,
            cost_source="provider_usage",
            pricing_status="provider_supplied",
            pricing=pricing,
            matched_model=matched_model or model_name,
        )
    if pricing is None or not _pricing_base_url_allowed(base_url):
        return _cost_payload(
            cost_estimate=None,
            cost_source="unknown",
            pricing_status="unknown_model_or_unpriced",
            pricing=pricing,
            matched_model=matched_model or "",
        )
    input_tokens = _safe_int(token_usage.get("input_tokens"))
    cached_tokens = min(_safe_int(token_usage.get("cached_input_tokens")), input_tokens)
    uncached_tokens = max(input_tokens - cached_tokens, 0)
    output_tokens = _safe_int(token_usage.get("output_tokens"))
    cost = (
        uncached_tokens / 1_000_000 * _safe_float(pricing.get("input"))
        + cached_tokens / 1_000_000 * _safe_float(pricing.get("cached_input"))
        + output_tokens / 1_000_000 * _safe_float(pricing.get("output"))
    )
    return _cost_payload(
        cost_estimate=round(cost, 6),
        cost_source="pricing_table",
        pricing_status="priced",
        pricing=pricing,
        matched_model=matched_model or model_name,
    )


def _safety_metrics(
    *,
    policy_blocks: int,
    capability_summary: dict[str, Any],
    trace_events: list[dict[str, Any]],
) -> dict[str, Any]:
    return {
        "policy_blocks": policy_blocks,
        "sandbox_backend": str(capability_summary.get("sandbox_backend") or ""),
        "local_process_fallback_count": _safe_int(capability_summary.get("local_process_fallback_count")),
        "secret_leak_detected": _secret_leak_detected(trace_events),
    }


def _reproducibility_metrics(
    *,
    reproducible_environment: dict[str, Any],
    trace_artifact_refs: list[str],
) -> dict[str, Any]:
    workspace = reproducible_environment.get("workspace")
    runtime = reproducible_environment.get("runtime")
    runtime_summary: dict[str, Any] = {}
    if isinstance(runtime, dict):
        runtime_summary = {
            "python": runtime.get("python"),
            "platform": runtime.get("platform"),
            "interpreter_strategy": runtime.get("interpreter_strategy"),
        }
    return {
        "repo": _safe_str(workspace.get("source")) if isinstance(workspace, dict) else "",
        "base_commit": _safe_str(workspace.get("start_commit")) if isinstance(workspace, dict) else "",
        "trace_artifact_refs": list(trace_artifact_refs),
        "reproducible_environment": {
            "task_id": reproducible_environment.get("task_id"),
            "task_type": reproducible_environment.get("task_type"),
            "workspace": workspace if isinstance(workspace, dict) else {},
            "runtime": runtime_summary,
        },
    }


def _out_of_scope_files(files_changed: list[str], allowed_paths: list[str]) -> list[str]:
    if not allowed_paths:
        return []
    return [path for path in files_changed if not _allowed_path(path, allowed_paths)]


def _allowed_path(path: str, allowed_paths: list[str]) -> bool:
    normalized = _normalize_allowed(path)
    for allowed in allowed_paths:
        normalized_allowed = _normalize_allowed(allowed)
        if normalized_allowed == ".":
            return True
        if normalized == normalized_allowed or normalized.startswith(normalized_allowed.rstrip("/") + "/"):
            return True
    return False


def _looks_like_test_path(path: str) -> bool:
    normalized = _normalize_allowed(path).lower()
    name = Path(normalized).name
    return (
        normalized.startswith("tests/")
        or "/tests/" in normalized
        or name.startswith("test_")
        or name.endswith("_test.py")
        or name.endswith(".test.py")
    )


def _diff_line_counts(diff: str) -> tuple[int | None, int | None, str]:
    if not diff.strip():
        return None, None, "diff not recorded"
    added = 0
    deleted = 0
    saw_hunk = False
    for line in diff.splitlines():
        if line.startswith("@@"):
            saw_hunk = True
            continue
        if line.startswith(("+++", "---")):
            continue
        if line.startswith("+"):
            added += 1
        elif line.startswith("-"):
            deleted += 1
    if not saw_hunk and added == 0 and deleted == 0:
        return None, None, "diff format did not include parseable hunks"
    return added, deleted, ""


def _tool_name_from_event(event: dict[str, Any]) -> str:
    payload = _event_payload(event)
    for key in ("tool_name", "name", "tool", "function"):
        value = payload.get(key) or event.get(key)
        if value:
            return str(value)
    tool_call = payload.get("tool_call") or event.get("tool_call")
    if isinstance(tool_call, dict):
        return str(tool_call.get("name") or tool_call.get("tool_name") or "")
    return ""


def _tool_call_id_from_event(event: dict[str, Any]) -> str:
    payload = _event_payload(event)
    value = payload.get("tool_call_id") or event.get("tool_call_id")
    if value:
        return str(value)
    tool_call = payload.get("tool_call") or event.get("tool_call")
    if isinstance(tool_call, dict):
        return str(tool_call.get("tool_call_id") or tool_call.get("id") or "")
    return ""


def _tool_result_status(event: dict[str, Any]) -> bool | None:
    payload = _event_payload(event)
    for key in ("ok", "success", "passed"):
        if isinstance(payload.get(key), bool):
            return bool(payload[key])
    status = str(payload.get("status") or payload.get("result_status") or event.get("status") or "").lower()
    if status in {"ok", "success", "succeeded", "passed", "completed"}:
        return True
    if status in {"error", "failed", "failure", "denied", "blocked", "timeout", "timed_out"}:
        return False
    if payload.get("error") or payload.get("exception"):
        return False
    return None


def _target_file_retrieval_hit(events: list[dict[str, Any]], *, target_files: list[str]) -> dict[str, Any]:
    targets = [_normalize_allowed(path).lower() for path in target_files if path]
    if not targets:
        return {"value": None, "reason": "no target files configured"}
    retrieval_events = [
        event
        for event in events
        if _event_type(event).startswith("retrieval") or "retrieval" in _event_type(event)
    ]
    if not retrieval_events:
        return {"value": None, "reason": "no retrieval evidence recorded"}
    for event in retrieval_events:
        text = json.dumps(_event_payload(event), ensure_ascii=False).lower().replace("\\", "/")
        if any(target in text for target in targets):
            return {"value": True, "reason": ""}
    return {"value": False, "reason": "retrieval events did not reference target files"}


def _provider_cost_estimate(events: list[dict[str, Any]]) -> float | None:
    total = 0.0
    found = False
    for event in events:
        payload = _event_payload(event)
        usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else payload
        if not isinstance(usage, dict) or usage.get("cost_estimate") is None:
            continue
        total += _safe_float(usage.get("cost_estimate"))
        found = True
    return round(total, 6) if found else None


def _pricing_model_key(model_name: str) -> str:
    normalized = model_name.strip().lower()
    return "mimo-v2.5" if normalized == "mimo-v2.5" else ""


def _pricing_base_url_allowed(base_url: str) -> bool:
    lowered = base_url.lower()
    return "xiaomimimo.com" in lowered or "mimo.mi.com" in lowered


def _cost_payload(
    *,
    cost_estimate: float | None,
    cost_source: str,
    pricing_status: str,
    pricing: dict[str, Any] | None,
    matched_model: str,
) -> dict[str, Any]:
    return {
        "cost_estimate": cost_estimate,
        "currency": str((pricing or {}).get("currency") or "USD") if pricing or cost_estimate is not None else "",
        "cost_source": cost_source,
        "pricing_status": pricing_status,
        "pricing_source_url": str((pricing or {}).get("source_url") or _MIMO_PRICING_SOURCE_URL) if pricing else "",
        "retrieved_at": str((pricing or {}).get("retrieved_at") or _MIMO_PRICING_RETRIEVED_AT) if pricing else "",
        "pricing_unit": "1M tokens" if pricing else "",
        "matched_model": matched_model,
    }


def _secret_leak_detected(events: list[dict[str, Any]]) -> bool:
    for event in events:
        rendered = json.dumps(_event_payload(event), ensure_ascii=False).lower()
        if "api_key" in rendered or "authorization" in rendered or "bearer " in rendered:
            return True
    return False


def _read_trace_events(trace: Path | None) -> list[dict[str, Any]]:
    if trace is None:
        return []
    events_path = trace / "events.jsonl" if trace.is_dir() else trace
    if not events_path.exists():
        return []
    events: list[dict[str, Any]] = []
    try:
        for line in events_path.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            payload = json.loads(line)
            if isinstance(payload, dict):
                events.append(payload)
    except (OSError, json.JSONDecodeError):
        return events
    return events


def _event_type(event: dict[str, Any]) -> str:
    return str(event.get("event_type") or event.get("type") or "")


def _count_events(events: list[dict[str, Any]], event_type: str) -> int:
    return sum(1 for event in events if _event_type(event) == event_type)


def _count_events_with_prefix(events: list[dict[str, Any]], prefixes: tuple[str, ...]) -> int:
    return sum(1 for event in events if _event_type(event).startswith(prefixes))


def _count_events_with_contains(events: list[dict[str, Any]], needles: tuple[str, ...]) -> int:
    return sum(1 for event in events if any(needle in _event_type(event) for needle in needles))


def _event_payload(event: dict[str, Any]) -> dict[str, Any]:
    payload = event.get("payload")
    return payload if isinstance(payload, dict) else {}


def _compaction_reason(events: list[dict[str, Any]], *, compaction_requested: int) -> str:
    for event in events:
        if not _event_type(event).startswith("context.compaction"):
            continue
        payload = _event_payload(event)
        reason = payload.get("reason") or payload.get("skipped_reason")
        if reason:
            return str(reason)
    if compaction_requested:
        return "compaction requested; no detailed reason recorded"
    return "context usage below compaction threshold, retrieval content insufficient, or task completed before compaction was needed"


def _sandbox_backend(events: list[dict[str, Any]], trace_summary: dict[str, Any]) -> str:
    for event in events:
        if not _event_type(event).startswith("sandbox."):
            continue
        payload = _event_payload(event)
        backend = payload.get("backend") or payload.get("sandbox_backend")
        if backend:
            return str(backend)
    planner = trace_summary.get("sandbox_isolation_summary") if isinstance(trace_summary, dict) else {}
    if isinstance(planner, dict):
        backends = planner.get("selected_backends") or planner.get("available_backends") or []
        if backends:
            return str(backends[0])
    for event in events:
        payload = _event_payload(event)
        backend = payload.get("backend") or payload.get("sandbox_backend")
        if backend:
            return str(backend)
    return ""


def _local_process_fallback_count(events: list[dict[str, Any]], trace_summary: dict[str, Any]) -> int:
    count = 0
    for event in events:
        payload = _event_payload(event)
        backend = str(payload.get("backend") or payload.get("sandbox_backend") or "")
        fallback = payload.get("local_process_fallback") or payload.get("used_local_process_fallback")
        if backend == "local_process" or fallback is True:
            count += 1
    planner = trace_summary.get("sandbox_isolation_summary") if isinstance(trace_summary, dict) else {}
    if isinstance(planner, dict):
        count = max(count, _safe_int(planner.get("local_process_backend_count")))
    return count


def _sandbox_time_seconds(events: list[dict[str, Any]]) -> float:
    command_durations: dict[str, float] = {}
    unidentified_command_durations: list[float] = []
    command_sandbox_ids: set[str] = set()
    orphan_sandbox_durations: dict[str, float] = {}
    for event in events:
        event_type = _event_type(event)
        payload = _event_payload(event)
        if event_type in {"command.completed", "command.failed", "command.timeout", "command.killed"}:
            duration = _duration_seconds_from_payload(payload)
            command_id = str(payload.get("command_id") or event.get("command_id") or "")
            sandbox_id = str(event.get("sandbox_id") or payload.get("sandbox_id") or "")
            if sandbox_id:
                command_sandbox_ids.add(sandbox_id)
            if command_id:
                command_durations[command_id] = max(command_durations.get(command_id, 0.0), duration)
            else:
                unidentified_command_durations.append(duration)
            continue
        if event_type not in {"sandbox.completed", "sandbox.cleaned"}:
            continue
        command_id = str(event.get("command_id") or payload.get("command_id") or "")
        sandbox_id = str(event.get("sandbox_id") or payload.get("sandbox_id") or "")
        if command_id and command_id in command_durations:
            continue
        if sandbox_id and sandbox_id in command_sandbox_ids:
            continue
        key = sandbox_id or str(event.get("event_id") or len(orphan_sandbox_durations))
        orphan_sandbox_durations[key] = max(
            orphan_sandbox_durations.get(key, 0.0),
            _duration_seconds_from_payload(payload),
        )
    total = (
        sum(command_durations.values())
        + sum(unidentified_command_durations)
        + sum(orphan_sandbox_durations.values())
    )
    return round(total, 3)


def _provider_time_by_turn(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    starts: dict[str, dict[str, Any]] = {}
    turns: list[dict[str, Any]] = []
    for event in events:
        event_type = _event_type(event)
        payload = _event_payload(event)
        request_id = str(payload.get("request_id") or "")
        if not request_id:
            continue
        if event_type == "model.request.created":
            starts[request_id] = {
                "monotonic_ms": _safe_int(event.get("monotonic_ms")),
                "purpose": str(payload.get("purpose") or ""),
                "action_id": str(event.get("action_id") or ""),
            }
            continue
        if event_type not in {"model.response.received", "model.request.failed"}:
            continue
        started = starts.get(request_id)
        ended_ms = _safe_int(event.get("monotonic_ms"))
        if started is None or not started["monotonic_ms"] or ended_ms < started["monotonic_ms"]:
            continue
        turns.append(
            {
                "turn": len(turns) + 1,
                "request_id": request_id,
                "purpose": started["purpose"],
                "action_id": started["action_id"],
                "status": "completed" if event_type == "model.response.received" else "failed",
                "duration_seconds": round((ended_ms - started["monotonic_ms"]) / 1000.0, 3),
            }
        )
    return turns


def _turn_diagnostics(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    starts: dict[str, dict[str, Any]] = {}
    rows: list[dict[str, Any]] = []
    request_to_row: dict[str, dict[str, Any]] = {}
    tool_to_request: dict[str, str] = {}
    tool_starts: dict[str, int] = {}
    latest_context: dict[str, Any] | None = None
    latest_model_row: dict[str, Any] | None = None

    for event in events:
        event_type = _event_type(event)
        payload = _event_payload(event)
        monotonic_ms = _safe_int(event.get("monotonic_ms"))
        if event_type == "context.rendered_for_model":
            latest_context = {
                "bundle_id": str(payload.get("bundle_id") or ""),
                "message_count": _safe_int(payload.get("message_count")),
                "included": _safe_int(payload.get("included")),
                "excluded": _safe_int(payload.get("excluded")),
                "cache_miss_reasons": [str(item) for item in payload.get("cache_miss_reasons") or []],
            }
            continue

        request_id = str(payload.get("request_id") or "")
        if event_type == "model.request.created" and request_id:
            action_id = str(event.get("action_id") or "")
            if not action_id.startswith("turn_"):
                continue
            estimated_usage = payload.get("estimated_usage")
            starts[request_id] = {
                "monotonic_ms": monotonic_ms,
                "phase_id": str(event.get("phase_id") or ""),
                "action_id": action_id,
                "purpose": str(payload.get("purpose") or ""),
                "message_count": _safe_int(payload.get("message_count")),
                "tool_count": _safe_int(payload.get("tool_count")),
                "estimated_input_tokens": _safe_int(
                    estimated_usage.get("input_tokens") if isinstance(estimated_usage, dict) else 0
                ),
                "context": dict(latest_context or {}),
            }
            continue

        if event_type in {"model.response.received", "model.request.failed"} and request_id:
            started = starts.get(request_id)
            if started is None or not started["monotonic_ms"] or monotonic_ms < started["monotonic_ms"]:
                continue
            usage = payload.get("usage") if isinstance(payload.get("usage"), dict) else {}
            cache = payload.get("cache") if isinstance(payload.get("cache"), dict) else {}
            row = {
                "turn": len(rows) + 1,
                "request_id": request_id,
                "action_id": started["action_id"],
                "phase_id": started["phase_id"],
                "purpose": started["purpose"],
                "provider_duration_seconds": round((monotonic_ms - started["monotonic_ms"]) / 1000.0, 3),
                "status": "completed" if event_type == "model.response.received" else "failed",
                "message_count": started["message_count"],
                "tool_count": started["tool_count"],
                "tool_call_count": _safe_int(payload.get("tool_call_count")),
                "finish_reason": str(payload.get("finish_reason") or ""),
                "input_tokens": _safe_int(usage.get("input_tokens")) or started["estimated_input_tokens"],
                "output_tokens": _safe_int(usage.get("output_tokens")),
                "cached_input_tokens": _safe_int(usage.get("cached_input_tokens")),
                "cache_hit_rate": round(_safe_float(cache.get("cache_hit_ratio")), 4),
                "context": started["context"],
                "tool_calls": [],
                "review_events": [],
                "verification_events": [],
                "finalization_events": [],
            }
            rows.append(row)
            request_to_row[request_id] = row
            latest_model_row = row
            continue

        if event_type == "model.tool_call.proposed":
            tool_call_id = str(payload.get("tool_call_id") or event.get("action_id") or "")
            if tool_call_id and request_id:
                tool_to_request[tool_call_id] = request_id
                tool_starts.setdefault(tool_call_id, monotonic_ms)
            continue

        if event_type == "tool_protocol.call_started":
            tool_call_id = str(payload.get("tool_call_id") or event.get("action_id") or "")
            if tool_call_id:
                tool_starts[tool_call_id] = monotonic_ms
            continue

        if event_type == "tool_protocol.call_completed":
            tool_call_id = str(payload.get("tool_call_id") or event.get("action_id") or "")
            row = request_to_row.get(tool_to_request.get(tool_call_id, ""))
            if row is None:
                continue
            started_ms = tool_starts.get(tool_call_id)
            duration = (
                round((monotonic_ms - started_ms) / 1000.0, 3)
                if started_ms is not None and monotonic_ms >= started_ms
                else 0.0
            )
            row["tool_calls"].append(
                {
                    "tool_call_id": tool_call_id,
                    "tool_name": str(payload.get("tool_name") or ""),
                    "status": str(payload.get("status") or ""),
                    "ok": bool(payload.get("ok")),
                    "error_code": payload.get("error_code"),
                    "duration_seconds": duration,
                }
            )
            continue

        if event_type == "review.completed":
            row = latest_model_row
            if row is None:
                continue
            critic_reused = bool(payload.get("critic_reused"))
            critic_source_status = str(payload.get("critic_source_status") or "")
            if not critic_source_status and not critic_reused:
                critic_source_status = str(payload.get("model_critic_status") or "")
            row["review_events"].append(
                {
                    "stage": str(payload.get("review_stage") or ""),
                    "action_id": str(event.get("action_id") or ""),
                    "decision": str(payload.get("decision") or ""),
                    "duration_seconds": round(_safe_float(payload.get("duration_ms")) / 1000.0, 3),
                    "critic_duration_seconds": round(_safe_float(payload.get("critic_duration_ms")) / 1000.0, 3),
                    "model_critic_status": str(payload.get("model_critic_status") or ""),
                    "critic_reused": critic_reused,
                    "critic_skipped_reason": str(payload.get("critic_skipped_reason") or ""),
                    "critic_reuse_skip_reason": str(payload.get("critic_reuse_skip_reason") or ""),
                    "critic_source_status": critic_source_status,
                }
            )
            continue

        if event_type == "verification.check_completed":
            row = latest_model_row
            if row is None:
                continue
            row["verification_events"].append(
                {
                    "check_id": str(payload.get("check_id") or ""),
                    "status": str(payload.get("status") or ""),
                    "duration_seconds": round(_duration_seconds_from_payload(payload), 3),
                }
            )
            continue

        if event_type in {
            "finalization.completed",
            "final_report.completed",
            "final_reviewer.assess.done",
            "final_reviewer.assess.model_ok",
            "final_reviewer.assess.model_skipped",
            "final_reviewer.assess.fallback",
        }:
            row = latest_model_row
            if row is None:
                continue
            row["finalization_events"].append(
                {
                    "event_type": event_type,
                    "status": str(payload.get("status") or payload.get("overall_status") or ""),
                }
            )

    return rows


def _sandbox_command_timings(events: list[dict[str, Any]]) -> list[dict[str, Any]]:
    rows: dict[str, dict[str, Any]] = {}
    command_requested: dict[str, int] = {}
    for event in events:
        event_type = _event_type(event)
        payload = _event_payload(event)
        command_id = str(payload.get("command_id") or event.get("command_id") or "")
        if not command_id:
            continue
        if event_type == "command.requested":
            command_requested[command_id] = _safe_int(event.get("monotonic_ms"))
            continue
        row = rows.setdefault(
            command_id,
            {
                "command_id": command_id,
                "sandbox_id": str(event.get("sandbox_id") or ""),
                "duration_seconds": 0.0,
            },
        )
        if event_type == "sandbox.requested":
            requested_ms = command_requested.get(command_id, 0)
            sandbox_ms = _safe_int(event.get("monotonic_ms"))
            if requested_ms and sandbox_ms >= requested_ms:
                row["command_dispatch_time_seconds"] = round(
                    (sandbox_ms - requested_ms) / 1000.0,
                    3,
                )
        if event_type in {"command.completed", "command.failed", "command.timeout", "command.killed"}:
            row["duration_seconds"] = max(
                float(row["duration_seconds"]),
                _duration_seconds_from_payload(payload),
            )
        phase_timing = payload.get("timing")
        if not isinstance(phase_timing, dict):
            metadata = payload.get("metadata") if isinstance(payload.get("metadata"), dict) else {}
            phase_timing = metadata.get("sandbox_timing") if isinstance(metadata, dict) else {}
        if isinstance(phase_timing, dict):
            for name, value in phase_timing.items():
                if isinstance(value, int | float):
                    row[str(name)] = max(float(row.get(str(name), 0.0)), float(value))
    return list(rows.values())


_SANDBOX_BREAKDOWN_FIELDS: dict[str, tuple[str, str]] = {
    "doctor_readiness": ("sandbox_doctor_readiness_time_seconds", "diagnostic_observation"),
    "acl_grant": ("acl_grant_time_seconds", "diagnostic_observation"),
    "workspace_materialization": ("workspace_materialization_time_seconds", "diagnostic_observation"),
    "process_spawn": ("process_spawn_time_seconds", "actual_execution"),
    "command_runtime": ("command_runtime_time_seconds", "actual_execution"),
    "output_collection": ("output_collection_time_seconds", "actual_execution"),
    "change_detection": ("change_detection_time_seconds", "diagnostic_observation"),
    "artifact_collection": ("artifact_collection_time_seconds", "diagnostic_observation"),
    "cleanup": ("run_root_cleanup_time_seconds", "diagnostic_observation"),
}


def _sandbox_breakdown(events: list[dict[str, Any]]) -> dict[str, Any]:
    commands = _sandbox_command_timings(events)
    totals = {name: 0.0 for name in _SANDBOX_BREAKDOWN_FIELDS}
    total_seconds = 0.0
    for command in commands:
        duration = command.get("duration_seconds")
        if isinstance(duration, int | float):
            total_seconds += float(duration)
        for name, (field_name, _kind) in _SANDBOX_BREAKDOWN_FIELDS.items():
            value = command.get(field_name)
            if isinstance(value, int | float):
                totals[name] += float(value)
    explained = sum(totals.values())
    diagnostics_overhead = max(0.0, total_seconds - explained)
    items = {
        name: {
            "actual_seconds": round(value, 3),
            "source": "sandbox_trace",
            "kind": _SANDBOX_BREAKDOWN_FIELDS[name][1],
        }
        for name, value in totals.items()
    }
    items["diagnostics_overhead"] = {
        "actual_seconds": round(diagnostics_overhead, 3),
        "source": "capability_summary.sandbox_commands",
        "kind": "diagnostic_observation",
    }
    return {
        "schema_version": "evaluation.sandbox_breakdown/v1",
        "command_count": len(commands),
        "total_seconds": round(total_seconds, 3),
        "items": items,
    }


def _trace_timing_details(events: list[dict[str, Any]]) -> dict[str, float]:
    values: dict[str, float] = {}
    mutation_durations: dict[str, float] = {}
    sandbox_fields = {
        "command_dispatch_time_seconds",
        "sandbox_doctor_readiness_time_seconds",
        "sandbox_account_selection_time_seconds",
        "acl_grant_time_seconds",
        "process_spawn_time_seconds",
        "command_runtime_time_seconds",
        "output_collection_time_seconds",
        "run_root_cleanup_time_seconds",
    }
    for row in _sandbox_command_timings(events):
        for name in sandbox_fields:
            value = row.get(name)
            if isinstance(value, int | float):
                values[name] = values.get(name, 0.0) + float(value)
    for event in events:
        event_type = _event_type(event)
        payload = _event_payload(event)
        duration = _duration_seconds_from_payload(payload)
        if event_type == "tool.dispatch.completed" and payload.get("tool_name") == "edit_apply":
            values["edit_apply_total_time_seconds"] = values.get("edit_apply_total_time_seconds", 0.0) + duration
        elif event_type == "mutation.applied":
            transaction_id = str(event.get("transaction_id") or event.get("event_id") or len(mutation_durations))
            mutation_durations[transaction_id] = max(mutation_durations.get(transaction_id, 0.0), duration)
        elif event_type == "review.completed" and str(payload.get("review_stage") or "") in {"pre_edit", "post_patch"}:
            values["edit_apply_review_time_seconds"] = values.get("edit_apply_review_time_seconds", 0.0) + duration
            values["edit_apply_critic_time_seconds"] = values.get("edit_apply_critic_time_seconds", 0.0) + (
                _safe_float(payload.get("critic_duration_ms")) / 1000.0
            )
        elif event_type == "context.bundle_built":
            values["context_assembly_time_seconds"] = values.get("context_assembly_time_seconds", 0.0) + duration
            values["compaction_decision_time_seconds"] = values.get("compaction_decision_time_seconds", 0.0) + (
                _safe_float(payload.get("compaction_decision_duration_ms")) / 1000.0
            )
        elif event_type.startswith("retrieval.") and duration:
            values["retrieval_time_seconds"] = values.get("retrieval_time_seconds", 0.0) + duration
    if mutation_durations:
        values["edit_apply_mutation_time_seconds"] = sum(mutation_durations.values())
    return {name: round(value, 3) for name, value in values.items()}


def _wall_phase_timings(values: dict[str, Any]) -> dict[str, float]:
    names = (
        "run_root_reset_time_seconds",
        "workspace_materialization_time_seconds",
        "dependency_setup_time_seconds",
        "baseline_workspace_copy_time_seconds",
        "baseline_verification_time_seconds",
        "agent_loop_time_seconds",
        "verification_workspace_copy_time_seconds",
        "public_verification_time_seconds",
        "verification_prepare_time_seconds",
        "hidden_verification_time_seconds",
        "resource_cleanup_time_seconds",
    )
    return {
        name: round(float(values[name]), 3)
        for name in names
        if isinstance(values.get(name), int | float)
    }


_DETAILED_TIMING_FIELDS = (
    "workspace_materialization_time_seconds",
    "repo_clone_time_seconds",
    "repo_fetch_time_seconds",
    "repo_checkout_time_seconds",
    "dependency_setup_time_seconds",
    "sandbox_doctor_readiness_time_seconds",
    "command_dispatch_time_seconds",
    "sandbox_account_selection_time_seconds",
    "acl_grant_time_seconds",
    "process_spawn_time_seconds",
    "command_runtime_time_seconds",
    "output_collection_time_seconds",
    "artifact_import_time_seconds",
    "run_root_cleanup_time_seconds",
    "verification_workspace_copy_time_seconds",
    "public_verification_time_seconds",
    "hidden_verification_time_seconds",
    "edit_apply_total_time_seconds",
    "edit_apply_mutation_time_seconds",
    "edit_apply_review_time_seconds",
    "edit_apply_critic_time_seconds",
    "context_assembly_time_seconds",
    "retrieval_time_seconds",
    "compaction_decision_time_seconds",
)


def _capability_timing_details(
    values: dict[str, float | None],
) -> tuple[dict[str, float | None], dict[str, dict[str, str]]]:
    timings: dict[str, float | None] = {}
    diagnostics: dict[str, dict[str, str]] = {}
    for name in _DETAILED_TIMING_FIELDS:
        value = values.get(name)
        timings[name] = round(float(value), 3) if value is not None else None
        if value is not None:
            diagnostics[name] = {
                "status": "measured",
                "source": _timing_source(name),
                "reason": "",
            }
        elif name == "repo_fetch_time_seconds":
            diagnostics[name] = {
                "status": "not_applicable",
                "source": "evaluation_runner",
                "reason": "remote materialization used clone without a standalone fetch",
            }
        elif name == "artifact_import_time_seconds":
            diagnostics[name] = {
                "status": "not_applicable",
                "source": "sandbox_result",
                "reason": "sandbox artifacts are collected but workspace changes are not imported",
            }
        else:
            diagnostics[name] = {
                "status": "unavailable",
                "source": "trace",
                "reason": "no reliable timing span was recorded",
            }
    return timings, diagnostics


def _timing_source(name: str) -> str:
    if name in {
        "sandbox_doctor_readiness_time_seconds",
        "command_dispatch_time_seconds",
        "sandbox_account_selection_time_seconds",
        "acl_grant_time_seconds",
        "process_spawn_time_seconds",
        "command_runtime_time_seconds",
        "output_collection_time_seconds",
        "run_root_cleanup_time_seconds",
    }:
        return "sandbox_trace"
    if name.startswith("edit_apply_"):
        return "review_trace" if name.endswith(("review_time_seconds", "critic_time_seconds")) else "tool_trace"
    if name in {
        "context_assembly_time_seconds",
        "retrieval_time_seconds",
        "compaction_decision_time_seconds",
    }:
        return "context_trace"
    return "evaluation_runner"


def _provider_time_seconds(events: list[dict[str, Any]], trace_summary: dict[str, Any]) -> float:
    total = 0.0
    for event in events:
        if _event_type(event) not in {"model.response.received", "model.request.failed"}:
            continue
        total += _duration_seconds_from_payload(_event_payload(event))
    if total:
        return round(total, 3)
    starts: dict[str, int] = {}
    for event in events:
        event_type = _event_type(event)
        payload = _event_payload(event)
        request_id = str(payload.get("request_id") or "")
        monotonic_ms = _safe_int(event.get("monotonic_ms"))
        if not request_id or not monotonic_ms:
            continue
        if event_type == "model.request.created":
            starts[request_id] = monotonic_ms
        elif event_type in {"model.response.received", "model.request.failed"}:
            started = starts.get(request_id)
            if started is not None and monotonic_ms >= started:
                total += (monotonic_ms - started) / 1000.0
    if total:
        return round(total, 3)
    usage = trace_summary.get("model_usage_summary") if isinstance(trace_summary, dict) else {}
    if isinstance(usage, dict):
        return round(_safe_float(usage.get("latency_ms")) / 1000.0, 3)
    return 0.0


def _context_retrieval_compaction_time_seconds(events: list[dict[str, Any]]) -> float:
    total = 0.0
    for event in events:
        event_type = _event_type(event)
        if not (
            event_type.startswith("context.")
            or event_type.startswith("retrieval.")
            or event_type.startswith("prompt.")
            or "compaction" in event_type
        ):
            continue
        total += _duration_seconds_from_payload(_event_payload(event))
    return round(total, 3)


def _duration_seconds_from_payload(payload: dict[str, Any]) -> float:
    if payload.get("duration_seconds") is not None:
        return _safe_float(payload.get("duration_seconds"))
    if payload.get("duration_ms") is not None:
        return _safe_float(payload.get("duration_ms")) / 1000.0
    if payload.get("latency_ms") is not None:
        return _safe_float(payload.get("latency_ms")) / 1000.0
    return 0.0


def _pytest_time_seconds(*results: CommandEvalResult | None) -> float:
    seen: set[tuple[str, float]] = set()
    total = 0.0
    for result in results:
        if result is None:
            continue
        command = result.raw_command or result.command
        if "pytest" in command:
            key = (command, result.duration_seconds)
            if key in seen:
                continue
            seen.add(key)
            total += result.duration_seconds
    return round(total, 3)


def _verification_time_seconds(*results: CommandEvalResult | None) -> float:
    seen: set[tuple[str, float]] = set()
    total = 0.0
    for result in results:
        if result is None:
            continue
        key = (result.raw_command or result.command, result.duration_seconds)
        if key in seen:
            continue
        seen.add(key)
        total += result.duration_seconds
    return round(total, 3)


def _ordered_verification_checks(checks: dict[str, Any]) -> list[str]:
    ordered = [name for name in ("public", "hidden") if isinstance(checks.get(name), dict)]
    ordered.extend(
        name
        for name, payload in sorted(checks.items())
        if name not in {"public", "hidden"} and isinstance(payload, dict)
    )
    return ordered


def _repair_phase_contract_satisfaction(payload: dict[str, Any]) -> dict[str, Any]:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    satisfaction = planner.get("contract_satisfaction") if isinstance(planner, dict) else {}
    if not isinstance(satisfaction, dict) or not satisfaction:
        return {
            "status": "not_recorded",
            "source": "kernel.final_report.planner_summary.contract_satisfaction",
        }
    satisfied = satisfaction.get("satisfied")
    return {
        "status": "satisfied" if satisfied is True else "unsatisfied" if satisfied is False else "recorded",
        "source": "kernel.final_report.planner_summary.contract_satisfaction",
        "contract_id": satisfaction.get("contract_id"),
        "completed_steps": list(satisfaction.get("completed_steps") or []),
        "failed_steps": list(satisfaction.get("failed_steps") or []),
        "skipped_steps": list(satisfaction.get("skipped_steps") or []),
        "reason": satisfaction.get("reason"),
    }


def _result_status(
    *,
    success: bool,
    tests_passed: bool,
    infrastructure_blocked: bool,
    agent_status: str,
    verification: CommandEvalResult | None,
    policy_blocks: int,
    errors: list[str],
) -> str:
    if infrastructure_blocked:
        return "environment_blocker"
    if success:
        return "success"
    if policy_blocks and agent_status in {"blocked", "failed"}:
        return "policy_blocked"
    if agent_status in {"blocked", "failed", "max_turns_exceeded"}:
        return agent_status
    if verification is not None and not tests_passed:
        return "verification_failed"
    if errors:
        return "failure"
    return "unknown"


def _contract_satisfaction(
    task: EvaluationTask,
    *,
    files_changed: list[str],
    allowed_scope: bool,
    verification: CommandEvalResult | None,
    public_verification: CommandEvalResult | None,
    agent_status: str,
    final_report_status: str,
    policy_blocks: int,
    patch: dict[str, Any],
    final_report_payload: dict[str, Any] | None = None,
) -> dict[str, Any]:
    expected_changes = list(task.expected_file_changes or [])
    patch_required = _patch_required(task, files_changed=files_changed)
    checks = [
        {"name": "allowed_scope", "passed": allowed_scope, "required": True},
        {
            "name": "verification_result",
            "passed": bool(verification and verification.passed),
            "required": True,
            "command": task.verification_command,
        },
        {
            "name": "public_verification_result",
            "passed": bool(public_verification.passed if public_verification else verification and verification.passed),
            "required": True,
        },
        {
            "name": "final_report_status",
            "passed": bool(final_report_status),
            "required": True,
            "status": final_report_status,
        },
        {
            "name": "patch_applicable",
            "passed": bool(patch.get("applicable")) if patch_required else True,
            "required": patch_required,
        },
    ]
    if expected_changes:
        checks.append(
            {
                "name": "expected_file_changes",
                "passed": _expected_file_changes_satisfied(
                    expected_changes,
                    files_changed=files_changed,
                ),
                "required": True,
                "expected": expected_changes,
                "actual": files_changed,
            }
        )
    if task.completion_standard:
        checks.append(
            {
                "name": "completion_standard_recorded",
                "passed": True,
                "required": False,
                "standard": task.completion_standard,
            }
        )
    if task.risk_tags:
        checks.append(
            {
                "name": "risk_tags_recorded",
                "passed": True,
                "required": False,
                "risk_tags": list(task.risk_tags),
            }
        )
    if _expected_blocked_success(task.success):
        checks.append(
            {
                "name": "policy_or_agent_block_observed",
                "passed": policy_blocks > 0 or agent_status in {"blocked", "failed"},
                "required": True,
                "policy_blocks": policy_blocks,
                "agent_status": agent_status,
            }
        )
    required = [item for item in checks if item.get("required")]
    passed = [item for item in required if item.get("passed")]
    repair_contract = _repair_phase_contract_satisfaction(final_report_payload or {})
    return {
        "status": "satisfied" if len(passed) == len(required) else "unsatisfied",
        "score": round(len(passed) / len(required), 4) if required else 1.0,
        "scope": "evaluation_task_contract",
        "task_level_verdict_source": "post_agent_independent_verification",
        "repair_phase_contract_satisfaction": repair_contract,
        "checks": checks,
    }


def _expected_blocked_success(success: dict[str, Any]) -> bool:
    kind = str(success.get("type") or "")
    if kind == "agent_status" and str(success.get("status") or "") in {"blocked", "failed"}:
        return True
    if kind == "policy_blocks_min":
        return True
    criteria = success.get("criteria") or []
    return any(
        isinstance(item, dict)
        and (
            (item.get("type") == "agent_status" and item.get("status") in {"blocked", "failed"})
            or item.get("type") == "policy_blocks_min"
            or _expected_blocked_success(item)
        )
        for item in criteria
    )


def _patch_required(task: EvaluationTask, *, files_changed: list[str]) -> bool:
    if _expected_blocked_success(task.success):
        return False
    return bool(task.expected_file_changes or files_changed)


def _patch_applicable_for_task(
    task: EvaluationTask,
    *,
    patch: dict[str, Any],
    files_changed: list[str],
) -> bool:
    if not _patch_required(task, files_changed=files_changed):
        return True
    if not patch.get("applicable"):
        return False
    if task.expected_file_changes:
        return _expected_file_changes_satisfied(
            task.expected_file_changes,
            files_changed=files_changed,
        )
    return True


def _expected_file_changes_satisfied(expected_changes: list[str], *, files_changed: list[str]) -> bool:
    changed = [_normalize_allowed(path) for path in files_changed]
    for expected in expected_changes:
        normalized = _normalize_allowed(expected)
        if not any(path == normalized or path.startswith(normalized.rstrip("/") + "/") for path in changed):
            return False
    return True


def _public_verification_command(task: EvaluationTask) -> str:
    if task.public_verification_command:
        return task.public_verification_command
    if task.verification_prepare_commands:
        return ""
    return task.verification_command


def _hidden_verification_command(task: EvaluationTask) -> str:
    return task.hidden_verification_command or task.verification_command


def _model_visible_verification_command(task: EvaluationTask) -> str:
    if task.model_visible_verification_command:
        return task.model_visible_verification_command
    if task.verification_prepare_commands:
        return task.public_verification_command
    return task.verification_command


def _success_criterion_ok(
    success: dict[str, Any],
    *,
    verification: CommandEvalResult,
    workspace: Path,
    agent_status: str = "",
    policy_blocks: int = 0,
) -> bool:
    kind = str(success.get("type") or "verification_exit_code")
    if kind == "verification_exit_code":
        return verification.exit_code == int(success.get("exit_code", 0)) and not verification.timed_out
    if kind == "agent_status":
        return agent_status == str(success.get("status") or "")
    if kind == "policy_blocks_min":
        return policy_blocks >= int(success.get("count") or 1)
    if kind == "file_exists":
        return _workspace_path(workspace, str(success.get("path") or "")).exists()
    if kind == "file_absent":
        return not _workspace_path(workspace, str(success.get("path") or "")).exists()
    if kind == "file_contains":
        path = _workspace_path(workspace, str(success.get("path") or ""))
        try:
            return path.exists() and str(success.get("text") or "") in path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return False
    if kind == "all":
        criteria = success.get("criteria") or []
        return bool(criteria) and all(
            _success_criterion_ok(
                _dict(item, "success.criteria"),
                verification=verification,
                workspace=workspace,
                agent_status=agent_status,
                policy_blocks=policy_blocks,
            )
            for item in criteria
        )
    if kind == "any":
        criteria = success.get("criteria") or []
        return bool(criteria) and any(
            _success_criterion_ok(
                _dict(item, "success.criteria"),
                verification=verification,
                workspace=workspace,
                agent_status=agent_status,
                policy_blocks=policy_blocks,
            )
            for item in criteria
        )
    raise ValueError(f"Unsupported evaluation success criterion: {kind}")


def _run_shell(
    command: str,
    *,
    cwd: Path,
    timeout_seconds: int,
    redactor: TraceRedactor,
    env_overrides: dict[str, str] | None = None,
) -> CommandEvalResult:
    started = time.perf_counter()
    try:
        argv, strategy = _resolve_command_argv(command)
    except ValueError as exc:
        return CommandEvalResult(
            command=command,
            raw_command=command,
            resolved_argv=[],
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=redactor.redact_text(str(exc))[:500],
            interpreter_strategy={
                "schema_version": "evaluation.command_interpreter/v1",
                "mode": "parse_error",
                "shell": False,
                "harness_executable": sys.executable,
            },
            failure_category="command_parse_error",
        )
    try:
        env = None
        if env_overrides:
            env = os.environ.copy()
            env.update({key: str(value) for key, value in env_overrides.items()})
        completed = subprocess.run(
            argv,
            cwd=cwd,
            shell=False,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
            check=False,
            env=env,
        )
        output = (completed.stderr or completed.stdout or "").strip().splitlines()
        error_summary = redactor.redact_text(output[0] if output else "")
        return CommandEvalResult(
            command=command,
            raw_command=command,
            resolved_argv=argv,
            exit_code=completed.returncode,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=error_summary[:500],
            interpreter_strategy=strategy,
            failure_category=_command_failure_category(
                argv,
                exit_code=completed.returncode,
                error_summary=error_summary,
            ),
        )
    except subprocess.TimeoutExpired:
        return CommandEvalResult(
            command=command,
            raw_command=command,
            resolved_argv=argv,
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            timed_out=True,
            error_summary=f"timed out after {timeout_seconds}s",
            interpreter_strategy=strategy,
            failure_category="command_timeout",
        )
    except FileNotFoundError as exc:
        return CommandEvalResult(
            command=command,
            raw_command=command,
            resolved_argv=argv,
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=redactor.redact_text(str(exc))[:500],
            interpreter_strategy=strategy,
            failure_category="command_not_found",
        )
    except OSError as exc:
        return CommandEvalResult(
            command=command,
            raw_command=command,
            resolved_argv=argv,
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=redactor.redact_text(str(exc))[:500],
            interpreter_strategy=strategy,
            failure_category="command_execution_error",
        )


def _resolve_command_argv(command: str) -> tuple[list[str], dict[str, Any]]:
    try:
        argv = shlex.split(command, posix=True)
    except ValueError as exc:
        raise ValueError(f"command parse failed: {exc}") from exc
    if not argv:
        raise ValueError("command parse failed: empty command")
    original_executable = argv[0]
    mapped_bare_python = _is_bare_python_executable(original_executable)
    if mapped_bare_python:
        argv = [sys.executable, *argv[1:]]
    strategy = _interpreter_strategy_summary()
    strategy.update(
        {
            "mode": "argv",
            "raw_executable": original_executable,
            "resolved_executable": argv[0],
            "mapped_bare_python": mapped_bare_python,
        }
    )
    return argv, strategy


def _interpreter_strategy_summary() -> dict[str, Any]:
    return {
        "schema_version": "evaluation.command_interpreter/v1",
        "parser": "shlex.split(posix=True)",
        "shell": False,
        "bare_python_policy": "map_to_harness_sys_executable",
        "harness_executable": sys.executable,
    }


def _is_bare_python_executable(value: str) -> bool:
    if not value or any(sep in value for sep in ("/", "\\")):
        return False
    return value.lower() in {"python", "python.exe", "python3", "python3.exe", "py", "py.exe"}


def _command_failure_category(argv: list[str], *, exit_code: int, error_summary: str) -> str:
    if exit_code == 0:
        return "none"
    lowered = error_summary.lower()
    if _looks_like_python_ssl_runtime_failure(lowered):
        return "environment_error"
    if "no module named pytest" in lowered or "module named" in lowered:
        return "environment_dependency_missing"
    if len(argv) >= 3 and Path(argv[0]).resolve(strict=False) == Path(sys.executable).resolve(strict=False) and argv[1:3] == ["-m", "pytest"]:
        return "verification_failed"
    return "command_failed"


def _looks_like_python_ssl_runtime_failure(lowered_output: str) -> bool:
    runtime_markers = (
        "while importing _ssl",
        "while importing _ssl.pyd",
        "ssl_low_integrity_runtime_initialization_failed",
        "libssl",
        "libcrypto",
        "openssl provider",
        "openssl config",
        "ossl-modules",
        "certificate path unreadable",
        "ssl.get_default_verify_paths",
        "dll search path",
    )
    failure_markers = (
        "importerror:",
        "dll load failed",
        "dll initialization",
        "initialization routine failed",
        "was not found",
        "is not readable",
        "unreadable",
        "missing",
        "failed",
    )
    return any(marker in lowered_output for marker in runtime_markers) and any(
        marker in lowered_output for marker in failure_markers
    )


def _patch_payload(before_snapshot: dict[str, str], workspace: Path) -> dict[str, Any]:
    after = _read_text_files(workspace)
    all_changed = sorted(
        path
        for path in set(before_snapshot) | set(after)
        if before_snapshot.get(path, "") != after.get(path, "")
    )
    diff_lines: list[str] = []
    for path in all_changed:
        if _is_sensitive_path(path):
            continue
        old = before_snapshot.get(path, "").splitlines(keepends=True)
        new = after.get(path, "").splitlines(keepends=True)
        diff_lines.extend(
            difflib.unified_diff(
                old,
                new,
                fromfile=f"a/{path}",
                tofile=f"b/{path}",
                lineterm="",
            )
        )
    diff_text = _PATCH_REDACTOR.redact_text("\n".join(diff_lines))
    return {
        "schema_version": "evaluation.patch/v1",
        "changed_files": [_display_path(path) for path in all_changed],
        "diff": diff_text,
        "applicable": False,
    }


def _requires_baseline_verification(task: EvaluationTask) -> bool:
    if task.task_type == "public_representative":
        return True
    metadata = task.fixture_metadata
    return bool(metadata.get("fail_to_pass") or metadata.get("FAIL_TO_PASS") or task.test_patch)


def _run_baseline_verification(
    task: EvaluationTask,
    *,
    baseline_workspace: Path,
    baseline_verification_workspace: Path,
    root: Path,
    redactor: TraceRedactor,
) -> dict[str, Any]:
    _reset_dir(baseline_verification_workspace, root=root)
    if baseline_workspace.exists():
        shutil.copytree(baseline_workspace, baseline_verification_workspace, dirs_exist_ok=True, ignore=_copy_ignore)
    patch_result = _apply_test_patch(
        task,
        workspace=baseline_verification_workspace,
        redactor=redactor,
    )
    public_command = _public_verification_command(task)
    hidden_command = _hidden_verification_command(task)
    public = _run_shell(
        public_command,
        cwd=baseline_verification_workspace,
        timeout_seconds=task.verification_timeout_seconds,
        redactor=redactor,
    )
    hidden = _run_shell(
        hidden_command,
        cwd=baseline_verification_workspace,
        timeout_seconds=task.verification_timeout_seconds,
        redactor=redactor,
    )
    checks = _checks_payload(public, hidden)
    if patch_result is not None:
        checks["test_patch"] = patch_result.to_dict()
        if not patch_result.passed:
            return {
                "status": "verification_misconfigured",
                "baseline_failed": False,
                "checks": checks,
                "verification_misconfiguration_reason": patch_result.error_summary
                or patch_result.failure_category
                or "test patch failed to apply",
            }
    if public.passed and hidden.passed:
        return {
            "status": "baseline_already_passing",
            "baseline_failed": False,
            "checks": checks,
            "verification_misconfiguration_reason": "",
        }
    misconfigured = _baseline_misconfiguration_reason(public, hidden)
    if misconfigured:
        return {
            "status": "verification_misconfigured",
            "baseline_failed": False,
            "checks": checks,
            "verification_misconfiguration_reason": misconfigured,
        }
    return {
        "status": "baseline_failed",
        "baseline_failed": True,
        "checks": checks,
        "verification_misconfiguration_reason": "",
    }


def _apply_test_patch(
    task: EvaluationTask,
    *,
    workspace: Path,
    redactor: TraceRedactor,
) -> CommandEvalResult | None:
    patch = task.test_patch
    if not patch.strip():
        return None
    started = time.perf_counter()
    try:
        env = os.environ.copy()
        env["GIT_CEILING_DIRECTORIES"] = str(workspace.parent.resolve(strict=False))
        completed = subprocess.run(
            ["git", "apply", "--whitespace=nowarn"],
            cwd=workspace,
            input=patch,
            text=True,
            capture_output=True,
            timeout=60,
            env=env,
            check=False,
        )
        output = (completed.stderr or completed.stdout or "").strip().splitlines()
        error_summary = redactor.redact_text(output[0] if output else "")
        return CommandEvalResult(
            command="git apply <evaluator test_patch>",
            raw_command="git apply <evaluator test_patch>",
            resolved_argv=["git", "apply", "--whitespace=nowarn"],
            exit_code=completed.returncode,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=error_summary[:500],
            interpreter_strategy={
                "schema_version": "evaluation.command_interpreter/v1",
                "mode": "argv",
                "shell": False,
                "harness_executable": sys.executable,
            },
            failure_category="none" if completed.returncode == 0 else "test_patch_apply_failed",
        )
    except subprocess.TimeoutExpired:
        return CommandEvalResult(
            command="git apply <evaluator test_patch>",
            raw_command="git apply <evaluator test_patch>",
            resolved_argv=["git", "apply", "--whitespace=nowarn"],
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            timed_out=True,
            error_summary="timed out after 60s",
            interpreter_strategy={
                "schema_version": "evaluation.command_interpreter/v1",
                "mode": "argv",
                "shell": False,
                "harness_executable": sys.executable,
            },
            failure_category="command_timeout",
        )
    except OSError as exc:
        return CommandEvalResult(
            command="git apply <evaluator test_patch>",
            raw_command="git apply <evaluator test_patch>",
            resolved_argv=["git", "apply", "--whitespace=nowarn"],
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=redactor.redact_text(str(exc))[:500],
            interpreter_strategy={
                "schema_version": "evaluation.command_interpreter/v1",
                "mode": "argv",
                "shell": False,
                "harness_executable": sys.executable,
            },
            failure_category="command_execution_error",
        )


def _baseline_misconfiguration_reason(*results: CommandEvalResult) -> str:
    for result in results:
        if result.passed:
            continue
        if result.timed_out:
            return result.error_summary or "baseline verification timed out"
        if result.failure_category in {
            "command_parse_error",
            "command_timeout",
            "command_not_found",
            "command_execution_error",
            "environment_dependency_missing",
            "environment_error",
        }:
            return result.error_summary or result.failure_category
        if result.exit_code is None:
            return result.error_summary or "baseline verification did not run"
        if result.exit_code not in {1} and result.failure_category != "verification_failed":
            return result.error_summary or result.failure_category or "baseline verification failed unexpectedly"
    return ""


def _prepare_verification_workspace(
    *,
    source_workspace: Path,
    verification_workspace: Path,
    baseline_workspace: Path,
    before_snapshot: dict[str, str],
    root: Path,
    test_patch: str = "",
) -> bool:
    _reset_dir(verification_workspace, root=root)
    if baseline_workspace.exists():
        for source_file in baseline_workspace.rglob("*"):
            if not source_file.is_file():
                continue
            relative = source_file.relative_to(baseline_workspace).as_posix()
            if _skip_path(relative):
                continue
            target = _workspace_path(verification_workspace, relative)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source_file, target)
    if test_patch.strip():
        patch_task = EvaluationTask(
            task_id="patch",
            workspace=EvaluationWorkspace(kind="fixture", files={"placeholder": ""}),
            user_task="patch",
            allowed_paths=["."],
            verification_command="python -c pass",
            success={"type": "verification_exit_code", "exit_code": 0},
            test_patch=test_patch,
        )
        applied = _apply_test_patch(patch_task, workspace=verification_workspace, redactor=TraceRedactor())
        if applied is not None and not applied.passed:
            return False
    elif _read_text_files(verification_workspace) != before_snapshot:
        return False
    after = _read_text_files(source_workspace)
    changed_paths = sorted(path for path in set(before_snapshot) | set(after) if before_snapshot.get(path) != after.get(path))
    for path in changed_paths:
        target = _workspace_path(verification_workspace, path)
        if path not in after:
            if target.exists():
                target.unlink()
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(after[path], encoding="utf-8")
    if not test_patch.strip():
        return _read_text_files(verification_workspace) == after
    return True


def _checks_payload(
    public: CommandEvalResult | None,
    hidden: CommandEvalResult | None,
) -> dict[str, Any]:
    return {
        "public": _check_payload(public),
        "hidden": _check_payload(hidden),
    }


def _check_passed(checks: dict[str, Any], name: str) -> bool:
    check = checks.get(name) if isinstance(checks, dict) else None
    return bool(isinstance(check, dict) and check.get("passed") is True)


def _check_payload(result: CommandEvalResult | None) -> dict[str, Any]:
    if result is None:
        return {"passed": False, "status": "not_run"}
    payload = result.to_dict()
    payload["status"] = "passed" if result.passed else "failed"
    return payload


def _first_test_command(task: BenchmarkTask) -> str:
    for outcome in task.expected_outcomes:
        if outcome.kind == ExpectedOutcomeKind.TEST and outcome.command:
            return outcome.command
    return ""


def _allowed_paths_for_task(task: BenchmarkTask, metadata: dict[str, Any]) -> list[str]:
    explicit = metadata.get("allowed_paths")
    if explicit:
        return [str(path) for path in explicit]
    paths: list[str] = []
    if task.golden_contract is not None:
        paths.extend(task.golden_contract.expected_files)
    for outcome in task.expected_outcomes:
        paths.extend(str(path) for path in outcome.expected_diff.get("paths", []) or [])
    return sorted(dict.fromkeys(paths)) or ["."]


def _run_git(args: list[str], *, cwd: Path) -> None:
    completed = subprocess.run(["git", *args], cwd=cwd, text=True, capture_output=True, check=False)
    if completed.returncode:
        raise RuntimeError((completed.stderr or completed.stdout or "git command failed").strip())


def _changed_files(workspace: Path, *, before_snapshot: dict[str, str]) -> list[str]:
    if _is_git_repo(workspace):
        completed = subprocess.run(
            ["git", "status", "--short", "--untracked-files=all"],
            cwd=workspace,
            text=True,
            capture_output=True,
            check=False,
        )
        if completed.returncode == 0:
            return sorted(
                path
                for path in (_status_path(line) for line in completed.stdout.splitlines())
                if path and not _skip_path(path)
            )
    after = _snapshot_files(workspace)
    return sorted(path for path in set(before_snapshot) | set(after) if before_snapshot.get(path) != after.get(path))


def _snapshot_files(root: Path) -> dict[str, str]:
    snapshot: dict[str, str] = {}
    if not root.exists():
        return snapshot
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        if _skip_path(relative):
            continue
        try:
            stat = path.stat()
            snapshot[relative] = f"{stat.st_size}:{stat.st_mtime_ns}"
        except OSError:
            continue
    return snapshot


def _read_text_files(root: Path) -> dict[str, str]:
    files: dict[str, str] = {}
    if not root.exists():
        return files
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        if _skip_path(relative):
            continue
        try:
            files[relative] = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return files


def _allowed_scope_ok(files_changed: list[str], allowed_paths: list[str]) -> bool:
    allowed = [_normalize_allowed(path) for path in allowed_paths]
    if "." in allowed:
        return True
    for path in files_changed:
        normalized = _normalize_allowed(path)
        if not any(normalized == item or normalized.startswith(item.rstrip("/") + "/") for item in allowed):
            return False
    return True


def _write_workspace_file(root: Path, relative: str, content: str) -> None:
    target = _workspace_path(root, relative)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


def _workspace_path(root: Path, relative: str) -> Path:
    if not relative.strip():
        raise ValueError("workspace-relative path is required.")
    target = (root / relative).resolve(strict=False)
    root_resolved = root.resolve(strict=False)
    if os.path.commonpath([str(root_resolved), str(target)]) != str(root_resolved):
        raise ValueError("path escapes evaluation workspace.")
    return target


def _reset_dir(path: Path, *, root: Path) -> None:
    resolved = path.resolve(strict=False)
    root_resolved = root.resolve(strict=False)
    if os.path.commonpath([str(root_resolved), str(resolved)]) != str(root_resolved):
        raise ValueError("refusing to delete outside evaluation run directory.")
    if resolved.exists():
        _make_tree_writable(resolved)
        shutil.rmtree(resolved)
    resolved.mkdir(parents=True, exist_ok=True)


def _make_tree_writable(root: Path) -> None:
    for path in [root, *root.rglob("*")]:
        try:
            os.chmod(path, 0o700)
        except OSError:
            continue


def _copy_ignore(_dir: str, names: list[str]) -> set[str]:
    return {name for name in names if _skip_path(name) or _is_sensitive_name(name)}


def _skip_path(path: str) -> bool:
    parts = Path(path).parts
    return any(part in {".git", ".singularity", ".venv", ".pytest_cache", ".ruff_cache", "__pycache__", "work", "outputs"} for part in parts)


def _is_sensitive_name(name: str) -> bool:
    lowered = name.lower()
    return (
        lowered.startswith(".env")
        or "api_key" in lowered
        or "token" in lowered
        or "secret" in lowered
        or lowered.endswith((".pem", ".key"))
    )


def _is_sensitive_path(path: str) -> bool:
    return any(_is_sensitive_name(part) for part in Path(path).parts)


def _display_path(path: str) -> str:
    return "<sensitive_path>" if _is_sensitive_path(path) else path


def _is_git_repo(path: Path) -> bool:
    return (path / ".git").exists()


def _is_remote_git_url(value: str) -> bool:
    lowered = value.lower().strip()
    return lowered.startswith(("https://", "http://", "ssh://", "git@"))


def _status_path(line: str) -> str:
    if len(line) < 4:
        return ""
    path = line[3:].strip()
    if " -> " in path:
        path = path.split(" -> ", 1)[1]
    return path.replace("\\", "/")


def _safe_name(value: str) -> str:
    safe = "".join(ch if ch.isalnum() or ch in {"-", "_", "."} else "_" for ch in value.strip())
    return safe or f"task_{uuid4().hex[:8]}"


def _normalize_allowed(path: str) -> str:
    normalized = path.replace("\\", "/").strip().strip("/")
    return normalized or "."


def _dict(value: Any, field_name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"evaluation {field_name} must be an object.")
    return dict(value)


def _safe_int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _safe_str(value: Any) -> str:
    return "" if value is None else str(value)


def _safe_float(value: Any) -> float:
    try:
        return float(value or 0.0)
    except (TypeError, ValueError):
        return 0.0


def _format_optional_float(value: Any) -> str:
    if value is None:
        return "unknown"
    return f"{_safe_float(value):.6f}"


def _format_optional_rate(value: Any) -> str:
    if value is None:
        return "unknown"
    return f"{_safe_float(value):.4f}"


def _float_map(value: dict[str, Any]) -> dict[str, float]:
    return {str(key): _safe_float(val) for key, val in value.items()}


def _average_rate(rates: dict[str, float]) -> float:
    if not rates:
        return 0.0
    return round(sum(rates.values()) / len(rates), 4)


def _rate(numerator: int, denominator: int) -> float:
    if denominator <= 0:
        return 0.0
    return round(numerator / denominator, 4)


def _float_delta(current: dict[str, Any], previous: dict[str, Any], key: str) -> float:
    return round(float(current.get(key, 0.0) or 0.0) - float(previous.get(key, 0.0) or 0.0), 6)


