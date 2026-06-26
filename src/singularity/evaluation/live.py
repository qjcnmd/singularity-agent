from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
import difflib
import sys
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
from singularity.policy import ApprovalMode, SecurityMode


LIVE_TASK_SET_SCHEMA_VERSION = "evaluation.live_agent_task_set/v1"
LIVE_RESULT_SCHEMA_VERSION = "evaluation.live_agent_eval_result/v1"

_PATCH_REDACTOR = ContextRedactor()


@dataclass(frozen=True)
class LiveEvalWorkspace:
    kind: str
    path: str | None = None
    files: dict[str, str] = field(default_factory=dict)
    start_commit: str | None = None

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "LiveEvalWorkspace":
        kind = str(payload.get("type") or payload.get("kind") or "").strip()
        start_commit = payload.get("start_commit")
        if kind in {"fixture", "fixture_workspace", "inline_files"}:
            files = payload.get("files") or payload.get("inline_files") or {}
            if not isinstance(files, dict) or not files:
                raise ValueError("live eval fixture workspace requires files.")
            return cls(kind="fixture", files={str(key): str(value) for key, value in files.items()})
        if kind in {"repo", "path"}:
            path = str(payload.get("path") or "").strip()
            if not path:
                raise ValueError("live eval repo workspace requires path.")
            return cls(kind="repo", path=path, start_commit=str(start_commit) if start_commit else None)
        raise ValueError(f"Unsupported live eval workspace type: {kind}")

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
class LiveEvalTask:
    task_id: str
    workspace: LiveEvalWorkspace
    user_task: str
    allowed_paths: list[str]
    verification_command: str
    success: dict[str, Any]
    description: str = ""
    allowed_tools: list[str] = field(default_factory=list)
    tool_policy: str = "read_write"
    strategy: dict[str, Any] = field(default_factory=dict)
    expected_file_changes: list[str] = field(default_factory=list)
    completion_standard: str = ""
    risk_tags: list[str] = field(default_factory=list)
    prepare_commands: list[str] = field(default_factory=list)
    verification_prepare_commands: list[str] = field(default_factory=list)
    verification_timeout_seconds: int = 120

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "LiveEvalTask":
        workspace_payload = _workspace_payload(payload)
        prepare_commands = payload.get("prepare_commands")
        if prepare_commands is None:
            single = payload.get("prepare_command")
            prepare_commands = [single] if single else []
        if not isinstance(prepare_commands, list):
            raise ValueError("live eval prepare_commands must be a list.")
        verification_prepare_commands = payload.get("verification_prepare_commands") or []
        if not isinstance(verification_prepare_commands, list):
            raise ValueError("live eval verification_prepare_commands must be a list.")
        task = cls(
            task_id=str(payload.get("task_id") or "").strip(),
            workspace=LiveEvalWorkspace.from_dict(workspace_payload),
            user_task=str(payload.get("user_task") or payload.get("prompt") or "").strip(),
            allowed_paths=[str(item) for item in payload.get("allowed_paths") or []],
            verification_command=str(payload.get("verification_command") or "").strip(),
            success=_dict(payload.get("success"), "success"),
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
            verification_prepare_commands=[str(item) for item in verification_prepare_commands if str(item).strip()],
            verification_timeout_seconds=int(payload.get("verification_timeout_seconds") or 120),
        )
        task._validate()
        return task

    def _validate(self) -> None:
        if not self.task_id:
            raise ValueError("live eval task requires task_id.")
        if not self.user_task:
            raise ValueError(f"live eval task {self.task_id} requires user_task.")
        if not self.allowed_paths:
            raise ValueError(f"live eval task {self.task_id} requires allowed_paths.")
        if not self.verification_command:
            raise ValueError(f"live eval task {self.task_id} requires verification_command.")
        if not self.success:
            raise ValueError(f"live eval task {self.task_id} requires success.")
        if self.tool_policy not in {"read_write", "read_only", "review_all", "non_interactive"}:
            raise ValueError(f"live eval task {self.task_id} has unsupported tool_policy.")
        if self.strategy.get("approval_mode") and str(self.strategy["approval_mode"]).lower() not in {
            "auto_safe",
            "read_only",
            "review_all",
            "non_interactive",
            "interactive",
        }:
            raise ValueError(f"live eval task {self.task_id} has unsupported strategy.approval_mode.")
        if self.workspace.kind == "repo" and not self.workspace.start_commit and not self.prepare_commands:
            raise ValueError(f"live eval repo task {self.task_id} requires start_commit or prepare_command.")

    def to_dict(self) -> dict[str, Any]:
        payload = {
            "task_id": self.task_id,
            "workspace": self.workspace.to_dict(),
            "user_task": self.user_task,
            "allowed_paths": list(self.allowed_paths),
            "verification_command": self.verification_command,
            "success": dict(self.success),
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
        if self.verification_prepare_commands:
            payload["verification_prepare_commands"] = list(self.verification_prepare_commands)
        return payload


@dataclass(frozen=True)
class LiveEvalManifest:
    tasks: list[LiveEvalTask]
    base_dir: Path
    schema_version: str = LIVE_TASK_SET_SCHEMA_VERSION

    @classmethod
    def from_dict(cls, payload: dict[str, Any], *, base_dir: Path) -> "LiveEvalManifest":
        schema_version = str(payload.get("schema_version") or "")
        if schema_version != LIVE_TASK_SET_SCHEMA_VERSION:
            raise ValueError(f"Unsupported live eval schema_version: {schema_version}")
        tasks_payload = payload.get("tasks")
        if not isinstance(tasks_payload, list) or not tasks_payload:
            raise ValueError("live eval manifest requires tasks.")
        return cls(tasks=[LiveEvalTask.from_dict(item) for item in tasks_payload], base_dir=base_dir)

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
        }


@dataclass(frozen=True)
class LiveEvalTaskResult:
    task_id: str
    success: bool
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
    request_cache_hit_rates: dict[str, float] = field(default_factory=dict)
    status: str = "unknown"
    turn_count: int = 0
    verification_result: dict[str, Any] = field(default_factory=dict)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)
    final_report_status: str = ""
    failure_repair_count: int = 0
    policy_blocks: int = 0
    token_usage: dict[str, Any] = field(default_factory=dict)
    cache_usage: dict[str, Any] = field(default_factory=dict)
    trace_artifact_refs: list[str] = field(default_factory=list)
    reproducible_environment: dict[str, Any] = field(default_factory=dict)
    result_extraction: dict[str, Any] = field(default_factory=dict)
    task_verification_result: dict[str, Any] = field(default_factory=dict)
    repair_verification_contract: dict[str, Any] = field(default_factory=dict)
    agent_loop_ref: str = "KernelBootstrap.boot -> AgentKernel.run_task -> AgentLoop.run"

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "status": self.status,
            "success": self.success,
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
            "request_cache_hit_rates": dict(sorted(self.request_cache_hit_rates.items())),
            "verification_result": self.verification_result,
            "contract_satisfaction": self.contract_satisfaction,
            "final_report_status": self.final_report_status,
            "failure_repair_count": self.failure_repair_count,
            "policy_blocks": self.policy_blocks,
            "token_usage": self.token_usage,
            "cache_usage": self.cache_usage,
            "trace_artifact_refs": list(self.trace_artifact_refs),
            "reproducible_environment": self.reproducible_environment,
            "result_extraction": self.result_extraction,
            "task_verification_result": self.task_verification_result,
            "repair_verification_contract": self.repair_verification_contract,
            "agent_loop_ref": self.agent_loop_ref,
        }


class SingularityPrivateBenchmarkAdapter:
    def load(self, path: Path | str) -> LiveEvalManifest:
        task_path = Path(path)
        tasks = [
            self._convert(task)
            for task in GoldenTaskStore(task_path).load()
            if task.adapter == BenchmarkAdapterKind.SINGULARITY_PRIVATE
            and task.visibility == BenchmarkVisibility.PRIVATE
        ]
        if not tasks:
            raise ValueError("No private Singularity benchmark tasks found.")
        return LiveEvalManifest(tasks=tasks, base_dir=task_path.parent.resolve(strict=False))

    def _convert(self, task: BenchmarkTask) -> LiveEvalTask:
        command = _first_test_command(task)
        if not command:
            raise ValueError(f"Private benchmark task {task.task_id} requires a test expected_outcome.")
        metadata = dict(task.input.metadata)
        if task.workspace_snapshot.kind == WorkspaceSnapshotKind.INLINE_FILES:
            workspace = LiveEvalWorkspace(kind="fixture", files=dict(task.workspace_snapshot.inline_files))
        elif task.workspace_snapshot.kind == WorkspaceSnapshotKind.GIT_REF:
            repo_path = metadata.get("repo_path") or metadata.get("repo")
            if not repo_path:
                raise ValueError(
                    f"Private git_ref benchmark task {task.task_id} requires input.metadata.repo_path."
                )
            workspace = LiveEvalWorkspace(
                kind="repo",
                path=str(repo_path),
                start_commit=task.workspace_snapshot.git_ref,
            )
        else:
            raise ValueError(
                f"Private benchmark task {task.task_id} uses unsupported snapshot kind: "
                f"{task.workspace_snapshot.kind.value}"
            )
        return LiveEvalTask(
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


class LiveAgentEvalRunner:
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
        self.output_root = Path(output_root or Path.cwd() / "work" / "evaluations-live").resolve(strict=False)
        self.run_id = run_id or f"live_eval_{uuid4().hex[:8]}"
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

    def run(self, manifest: LiveEvalManifest) -> dict[str, Any]:
        started = time.perf_counter()
        self.run_dir.mkdir(parents=True, exist_ok=True)
        results = [self.run_task(task, manifest_base=manifest.base_dir) for task in manifest.tasks]
        previous = _previous_live_result(
            self.output_root,
            current_run_id=self.run_id,
            baseline_result_path=self.baseline_result_path,
        )
        payload = {
            "schema_version": LIVE_RESULT_SCHEMA_VERSION,
            "run_id": self.run_id,
            "output_dir": str(self.run_dir),
            "summary": summarize_live_results(results),
            "tasks": [result.to_dict() for result in results],
            "duration_seconds": round(time.perf_counter() - started, 3),
        }
        if previous:
            payload["regression"] = compare_live_eval_results(previous, payload)
        result_path = self.run_dir / "result.json"
        result_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        report_path = self.run_dir / "report.json"
        markdown_path = self.run_dir / "report.md"
        report_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        markdown_path.write_text(live_eval_report_markdown(payload), encoding="utf-8")
        regression = payload.get("regression")
        if isinstance(regression, dict):
            regression_path = self.run_dir / "regression.json"
            regression_md_path = self.run_dir / "regression.md"
            regression_path.write_text(
                json.dumps(regression, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            regression_md_path.write_text(
                live_eval_regression_markdown(regression),
                encoding="utf-8",
            )
            payload["regression_path"] = str(regression_path)
            payload["regression_markdown_path"] = str(regression_md_path)
        payload["result_path"] = str(result_path)
        payload["report_path"] = str(report_path)
        payload["markdown_path"] = str(markdown_path)
        return payload

    def run_task(self, task: LiveEvalTask, *, manifest_base: Path) -> LiveEvalTaskResult:
        started = time.perf_counter()
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
        failure_repair_count = 0
        policy_blocks = 0
        trace_artifact_refs: list[str] = []
        contract_satisfaction: dict[str, Any] = {}
        reproducible_environment: dict[str, Any] = {}
        try:
            _reset_dir(task_dir, root=self.run_dir)
            self._materialize_workspace(task, workspace=workspace, manifest_base=manifest_base)
            config = ProductionConfig.from_cli(
                project_root=workspace,
                max_turns=self.max_turns or adaptive_default_max_turns(task.user_task),
                model=self.model,
                base_url=self.base_url,
                env_root=self.env_root,
                approval_mode=_approval_mode_for_task(task),
                security_mode=_security_mode_for_task(task),
                interaction_mode=InteractionMode.NON_INTERACTIVE,
                raw_artifacts=False,
                profile=f"live-eval:{task.task_id}:{task.tool_policy}",
                cli_overrides={
                    "max_turns",
                    "model",
                    "base_url",
                    "approval_mode",
                    "security_mode",
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
            for command in task.prepare_commands:
                prepared = _run_shell(command, cwd=workspace, timeout_seconds=120, redactor=self.redactor)
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
                    )
            before_snapshot = _snapshot_files(workspace)
            before_text_snapshot = _read_text_files(workspace)
            shutil.copytree(workspace, baseline_workspace, ignore=_copy_ignore)
            goal = _task_goal(task)
            kernel = self.bootstrap_cls(project_root=workspace, config=config, console=self.console).boot(goal)
            _apply_benchmark_constraints(kernel, task)
            agent_result = kernel.run_task(goal)
            trace_path = _trace_path(kernel)
            trace_summary = _trace_summary(kernel, agent_result)
            final_report_payload = _final_report_payload(agent_result)
            usage = dict(trace_summary.get("model_usage_summary") or {})
            tool_calls = _safe_int(trace_summary.get("tool_calls")) or _safe_int(usage.get("tool_calls_proposed"))
            turn_count = _turn_count(agent_result, usage)
            failure_repair_count = _failure_repair_count(final_report_payload)
            policy_blocks = _policy_blocks(final_report_payload, trace_summary)
            trace_artifact_refs = _trace_artifact_refs(final_report_payload, trace_summary)
            agent_status = _agent_status(agent_result)
            if _infrastructure_blocked(agent_result, usage=usage, tool_calls=tool_calls):
                errors.append("infrastructure blocked: model provider unavailable")
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
                    failure_repair_count=failure_repair_count,
                    policy_blocks=policy_blocks,
                    trace_artifact_refs=trace_artifact_refs,
                    contract_satisfaction=contract_satisfaction,
                    reproducible_environment=reproducible_environment,
                )
            files_changed = _changed_files(workspace, before_snapshot=before_snapshot)
            patch_payload = _patch_payload(before_text_snapshot, workspace)
            applicable = _prepare_verification_workspace(
                source_workspace=workspace,
                verification_workspace=verification_workspace,
                baseline_workspace=baseline_workspace,
                before_snapshot=before_text_snapshot,
                root=task_dir,
            )
            patch_payload["applicable"] = applicable
            if task.verification_prepare_commands:
                public_verification = CommandEvalResult(
                    command=task.verification_command,
                    exit_code=0,
                    duration_seconds=0.0,
                    error_summary="hidden verifier only",
                )
            else:
                public_verification = _run_shell(
                    task.verification_command,
                    cwd=verification_workspace,
                    timeout_seconds=task.verification_timeout_seconds,
                    redactor=self.redactor,
                )
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
                        failure_repair_count=failure_repair_count,
                        policy_blocks=policy_blocks,
                        trace_artifact_refs=trace_artifact_refs,
                        contract_satisfaction=contract_satisfaction,
                        reproducible_environment=reproducible_environment,
                    )
            hidden_verification = _run_shell(
                task.verification_command,
                cwd=verification_workspace,
                timeout_seconds=task.verification_timeout_seconds,
                redactor=self.redactor,
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
            patch_ok = bool(
                patch_payload.get("applicable")
                or not _patch_required(task, files_changed=files_changed)
            )
            if not patch_ok:
                errors.append("patch could not be applied to clean verification workspace")
            if not allowed_ok:
                errors.append("changed files outside allowed_paths")
            if not criterion_ok:
                errors.append("success criterion failed")
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
        finally:
            close_resources = getattr(kernel, "close_resources", None) if kernel is not None else None
            if callable(close_resources):
                close_resources()
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
            failure_repair_count=failure_repair_count,
            policy_blocks=policy_blocks,
            trace_artifact_refs=trace_artifact_refs,
            contract_satisfaction=contract_satisfaction,
            reproducible_environment=reproducible_environment,
        )

    def _materialize_workspace(self, task: LiveEvalTask, *, workspace: Path, manifest_base: Path) -> None:
        if task.workspace.kind == "fixture":
            workspace.mkdir(parents=True, exist_ok=True)
            for relative, content in task.workspace.files.items():
                _write_workspace_file(workspace, relative, content)
            return
        source = Path(str(task.workspace.path or ""))
        if not source.is_absolute():
            source = (manifest_base / source).resolve(strict=False)
        if not source.exists():
            raise FileNotFoundError(source)
        if task.workspace.start_commit and _is_git_repo(source):
            _run_git(["clone", "--quiet", "--shared", str(source), str(workspace)], cwd=manifest_base)
            _run_git(["checkout", "--quiet", task.workspace.start_commit], cwd=workspace)
            return
        shutil.copytree(source, workspace, ignore=_copy_ignore)

    def _task_result(
        self,
        *,
        task: LiveEvalTask,
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
        failure_repair_count: int = 0,
        policy_blocks: int = 0,
        trace_artifact_refs: list[str] | None = None,
        contract_satisfaction: dict[str, Any] | None = None,
        reproducible_environment: dict[str, Any] | None = None,
    ) -> LiveEvalTaskResult:
        request_rates = _float_map(usage.get("request_cache_hit_rates") or {})
        final_report_payload = final_report_payload or {}
        trace_summary = trace_summary or {}
        status = _result_status(
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
        extraction = _result_extraction(final_report_payload, trace_summary)
        repair_contract = _repair_verification_contract(final_report_payload)
        return LiveEvalTaskResult(
            task_id=task.task_id,
            success=success,
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
            patch=patch or {"diff": "", "applicable": False, "changed_files": []},
            checks=checks or _checks_payload(None, verification),
            verification=verification,
            request_cache_hit_rates=request_rates,
            verification_result=verification_result,
            contract_satisfaction=contract_satisfaction or _contract_satisfaction(
                task,
                files_changed=files_changed,
                allowed_scope=_allowed_scope_ok(files_changed, task.allowed_paths),
                verification=verification,
                public_verification=None,
                agent_status=agent_status,
                final_report_status=final_report_status,
                policy_blocks=policy_blocks,
                patch=patch,
                final_report_payload=final_report_payload,
            ),
            final_report_status=final_report_status,
            failure_repair_count=failure_repair_count or _failure_repair_count(final_report_payload),
            policy_blocks=policy_blocks,
            token_usage=token_usage,
            cache_usage=cache_usage,
            trace_artifact_refs=list(trace_artifact_refs or _trace_artifact_refs(final_report_payload, trace_summary)),
            reproducible_environment=reproducible_environment or {},
            result_extraction=extraction,
            task_verification_result=verification_result,
            repair_verification_contract=repair_contract,
        )


def load_live_eval_manifest(path: Path | str) -> LiveEvalManifest:
    manifest_path = Path(path)
    payload = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("live eval manifest must be a JSON object.")
    return LiveEvalManifest.from_dict(payload, base_dir=manifest_path.parent.resolve(strict=False))


def summarize_live_results(results: list[LiveEvalTaskResult]) -> dict[str, Any]:
    task_count = len(results)
    scored_results = [result for result in results if not result.infrastructure_blocked]
    scored_task_count = len(scored_results)
    infrastructure_blocked_count = task_count - scored_task_count
    success_count = sum(1 for result in results if result.success)
    tests_passed_count = sum(1 for result in results if result.tests_passed)
    prompt_tokens = sum(result.prompt_tokens for result in results)
    cached_tokens = sum(result.cached_tokens for result in results)
    failures: dict[str, int] = {}
    miscompletion_count = 0
    for result in scored_results:
        if not result.success:
            failures[result.status or "failure"] = failures.get(result.status or "failure", 0) + 1
        if result.final_report_status in {"completed", "finalized"} and not result.success:
            miscompletion_count += 1
    return {
        "task_count": task_count,
        "scored_task_count": scored_task_count,
        "infrastructure_blocked_count": infrastructure_blocked_count,
        "score_status": _score_status(
            task_count=task_count,
            scored_task_count=scored_task_count,
            infrastructure_blocked_count=infrastructure_blocked_count,
        ),
        "success_count": success_count,
        "task_completion_rate": _rate(success_count, scored_task_count),
        "tests_passed_count": tests_passed_count,
        "test_pass_rate": _rate(tests_passed_count, scored_task_count),
        "prompt_tokens": prompt_tokens,
        "cached_tokens": cached_tokens,
        "request_cache_hit_rate": _average_rate({result.task_id: result.request_cache_hit_rate for result in scored_results}),
        "run_cache_hit_rate": _rate(cached_tokens, prompt_tokens),
        "tool_calls": sum(result.tool_calls for result in results),
        "success_rate": _rate(success_count, scored_task_count),
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
        "failure_repair_count": sum(result.failure_repair_count for result in results),
        "policy_blocks": sum(result.policy_blocks for result in results),
        "miscompletion_count": miscompletion_count,
        "failure_reasons": dict(sorted(failures.items())),
    }


def compare_live_eval_results(
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
                "baseline_status": previous.get("status") or _legacy_status(previous),
                "candidate_status": item.get("status") or _legacy_status(item),
                "baseline_success": bool(previous.get("success")),
                "candidate_success": bool(item.get("success")),
                "turn_delta": _safe_int(item.get("turn_count")) - _safe_int(previous.get("turn_count")),
                "tool_call_delta": _safe_int(item.get("tool_calls")) - _safe_int(previous.get("tool_calls")),
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
        "schema_version": "evaluation.live_agent_regression/v1",
        "baseline_run_id": str(baseline.get("run_id") or ""),
        "candidate_run_id": str(candidate.get("run_id") or ""),
        "summary": {
            "success_rate_delta": _float_delta(candidate_summary, baseline_summary, "success_rate"),
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


def live_eval_report_markdown(payload: dict[str, Any]) -> str:
    summary = _dict(payload.get("summary") or {}, "summary")
    lines = [
        f"# Live Agent Evaluation `{payload.get('run_id', '')}`",
        "",
        f"- status: `{summary.get('score_status', 'unknown')}`",
        f"- task count: {summary.get('task_count', 0)}",
        f"- success rate: {summary.get('success_rate', 0):.4f}",
        f"- verification pass rate: {summary.get('verification_pass_rate', 0):.4f}",
        f"- average turns: {summary.get('average_turns', 0):.4f}",
        f"- average tool calls: {summary.get('average_tool_calls', 0):.4f}",
        f"- policy blocks: {summary.get('policy_blocks', 0)}",
        f"- failure repairs: {summary.get('failure_repair_count', 0)}",
        f"- miscompletion count: {summary.get('miscompletion_count', 0)}",
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
            f"{task.get('status', _legacy_status(task))} | "
            f"{verification} | "
            f"{_safe_int(task.get('turn_count'))} | "
            f"{_safe_int(task.get('tool_calls'))} | "
            f"{files} | "
            f"{task.get('final_report_status') or '-'} | "
            f"{contract} | "
            f"{failures} |"
        )
    regression = payload.get("regression")
    if isinstance(regression, dict):
        lines.extend(["", "## Regression", ""])
        lines.extend(live_eval_regression_markdown(regression).splitlines()[2:])
    return "\n".join(lines) + "\n"


def live_eval_regression_markdown(payload: dict[str, Any]) -> str:
    summary = _dict(payload.get("summary") or {}, "regression.summary")
    lines = [
        f"# Live Agent Regression `{payload.get('baseline_run_id', '')}` -> `{payload.get('candidate_run_id', '')}`",
        "",
        f"- success rate delta: {summary.get('success_rate_delta', 0):.4f}",
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
    def load(self, _path: Path | str) -> LiveEvalManifest:
        raise NotImplementedError("SWE-bench adapter boundary is reserved; use SingularityPrivateBenchmarkAdapter today.")


class TerminalBenchAdapter:
    def load(self, _path: Path | str) -> LiveEvalManifest:
        raise NotImplementedError("Terminal-Bench adapter boundary is reserved; use SingularityPrivateBenchmarkAdapter today.")


def _score_status(*, task_count: int, scored_task_count: int, infrastructure_blocked_count: int) -> str:
    if scored_task_count > 0:
        return "scored"
    if task_count > 0 and infrastructure_blocked_count == task_count:
        return "infrastructure_blocked"
    return "empty"


def _previous_live_result(
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
        if isinstance(payload, dict) and payload.get("schema_version") == LIVE_RESULT_SCHEMA_VERSION:
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
        if isinstance(payload, dict) and payload.get("schema_version") == LIVE_RESULT_SCHEMA_VERSION:
            return payload
    return None


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
    raise ValueError("live eval task requires workspace, repo_path, or fixture_workspace.")


def _task_goal(task: LiveEvalTask) -> str:
    allowed = ", ".join(task.allowed_paths)
    tools = ", ".join(task.allowed_tools) if task.allowed_tools else "default Singularity coding tools"
    risks = ", ".join(task.risk_tags) if task.risk_tags else "none declared"
    expected_changes = ", ".join(task.expected_file_changes) if task.expected_file_changes else "no required file changes declared"
    verification_instruction = (
        "Before finishing, run the relevant visible checks you can infer. "
        "Hidden evaluator setup and independent verification will run after you finish."
        if task.verification_prepare_commands
        else f"Before finishing, run this verification command: {task.verification_command}"
    )
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
    task: LiveEvalTask,
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
        "schema_version": "evaluation.live_agent_environment/v1",
        "task_id": task.task_id,
        "workspace": _workspace_environment(task, manifest_base=manifest_base),
        "workspace_path": str(workspace),
        "prepare_commands": list(task.prepare_commands),
        "verification_command": task.verification_command,
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
            "approval_mode": config.approval_mode.value,
            "security_mode": config.security_mode.value,
            "interaction_mode": config.interaction_mode.value,
            "sandbox_strategy": task.strategy.get("sandbox_strategy") or config.security_mode.value,
        },
        "baseline_artifacts": {
            "baseline_result_path": str(baseline_result_path) if baseline_result_path else None,
            "output_root": str(output_root),
        },
        "runtime": {
            "python": sys.version.split()[0],
            "platform": sys.platform,
            "agent_loop_ref": "KernelBootstrap.boot -> AgentKernel.run_task -> AgentLoop.run",
        },
    }


def _workspace_environment(task: LiveEvalTask, *, manifest_base: Path) -> dict[str, Any]:
    if task.workspace.kind == "fixture":
        return {
            "type": "fixture",
            "file_count": len(task.workspace.files),
            "file_names": sorted(task.workspace.files),
            "source": "manifest.inline_files",
        }
    source = task.workspace.path or ""
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


def _infrastructure_blocked(agent_result: Any, *, usage: dict[str, Any], tool_calls: int) -> bool:
    status = _agent_status(agent_result)
    if status != "failed" or _safe_int(usage.get("input_tokens")) or tool_calls:
        return False
    answer = str(getattr(agent_result, "final_answer", "") or "").lower()
    return any(marker in answer for marker in ("winerror 10013", "network", "socket", "访问权限不允许"))


def _approval_mode_for_task(task: LiveEvalTask) -> ApprovalMode:
    value = str(task.strategy.get("approval_mode") or "").strip().lower()
    if not value:
        value = {
            "read_only": "read_only",
            "review_all": "review_all",
            "non_interactive": "non_interactive",
        }.get(task.tool_policy, "auto_safe")
    return _approval_mode(value)


def _apply_benchmark_constraints(kernel: Any, task: LiveEvalTask) -> None:
    planner = getattr(getattr(kernel, "graph", None), "planner", None)
    apply_constraints = getattr(planner, "apply_benchmark_constraints", None)
    if not callable(apply_constraints):
        return
    apply_constraints(
        {
            "task_id": task.task_id,
            "allowed_tools": task.allowed_tools,
            "expected_file_changes": task.expected_file_changes,
            "completion_standard": task.completion_standard,
            "risk_tags": task.risk_tags,
            "verification_command": task.verification_command,
        }
    )


def _security_mode_for_task(task: LiveEvalTask) -> SecurityMode:
    value = str(task.strategy.get("security_mode") or "").strip().lower() or "compat"
    try:
        return SecurityMode[value.upper()]
    except KeyError:
        return SecurityMode(value)


def _approval_mode(value: str) -> ApprovalMode:
    try:
        return ApprovalMode[value.upper()]
    except KeyError:
        return ApprovalMode(value)


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


def _failure_repair_count(payload: dict[str, Any]) -> int:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    if not isinstance(planner, dict):
        return 0
    failure_repair = planner.get("failure_repair_summary")
    if not isinstance(failure_repair, dict):
        return 0
    return _safe_int(
        failure_repair.get("repair_execution_count")
        or failure_repair.get("repair_attempt_count")
        or failure_repair.get("repair_plan_count")
        or failure_repair.get("failure_analysis_count")
    )


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


def _result_extraction(payload: dict[str, Any], trace_summary: dict[str, Any]) -> dict[str, Any]:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    policy = payload.get("policy_summary") if isinstance(payload, dict) else {}
    return {
        "schema_version": "evaluation.live_agent_result_extraction/v1",
        "status_source": "kernel.final_report.planner_summary.status"
        if isinstance(planner, dict) and planner.get("status")
        else "kernel.run_result.status",
        "verification_source": "post_agent_independent_verification",
        "planner_summary_present": isinstance(planner, dict) and bool(planner),
        "failure_repair_source": "kernel.final_report.planner_summary.failure_repair_summary",
        "policy_blocks_source": "kernel.final_report.policy_summary/execution_trace_summary/trace_summary",
        "trace_artifact_source": "kernel.final_report.artifacts/planner_summary.artifacts/trace_summary.key_artifacts",
        "contract_satisfaction_source": "live_eval.task_contract_plus_independent_verification",
        "repair_verification_contract_source": (
            "kernel.final_report.planner_summary.failure_repair_summary"
        ),
        "raw_policy_summary_keys": sorted(policy) if isinstance(policy, dict) else [],
        "raw_trace_summary_keys": sorted(trace_summary),
    }


def _repair_verification_contract(payload: dict[str, Any]) -> dict[str, Any]:
    planner = payload.get("planner_summary") if isinstance(payload, dict) else {}
    failure_repair = planner.get("failure_repair_summary") if isinstance(planner, dict) else {}
    if not isinstance(failure_repair, dict) or not failure_repair:
        return {"status": "not_recorded", "source": "kernel.final_report.planner_summary.failure_repair_summary"}
    return {
        "status": failure_repair.get("verification_contract_status") or "recorded",
        "source": "kernel.final_report.planner_summary.failure_repair_summary",
        "contract_id": failure_repair.get("verification_contract_id"),
        "step_count": _safe_int(failure_repair.get("verification_contract_step_count")),
        "validation_errors": list(failure_repair.get("verification_contract_validation_errors") or []),
        "latest_repair_contract_id": failure_repair.get("latest_repair_contract_id"),
        "latest_verification_plan": list(failure_repair.get("latest_verification_plan") or []),
        "latest_target_files": list(failure_repair.get("latest_target_files") or []),
        "latest_blocked_reason": failure_repair.get("latest_blocked_reason"),
        "needs_user_input": bool(failure_repair.get("needs_user_input")),
        "repair_plan_count": _safe_int(failure_repair.get("repair_plan_count")),
        "repair_attempt_count": _safe_int(failure_repair.get("repair_attempt_count")),
        "repair_execution_count": _safe_int(failure_repair.get("repair_execution_count")),
        "repair_blocked_count": _safe_int(failure_repair.get("repair_blocked_count")),
    }


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
        return "infrastructure_blocked"
    if success:
        return "success"
    if policy_blocks and agent_status in {"blocked", "failed"}:
        return "policy_blocked"
    if verification is not None and not tests_passed:
        return "verification_failed"
    if agent_status in {"blocked", "failed", "max_turns_exceeded"}:
        return agent_status
    if errors:
        return "failure"
    return "unknown"


def _contract_satisfaction(
    task: LiveEvalTask,
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
    changed = set(files_changed)
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
                "passed": all(path in changed for path in expected_changes),
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
        "scope": "live_task_contract",
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


def _patch_required(task: LiveEvalTask, *, files_changed: list[str]) -> bool:
    if _expected_blocked_success(task.success):
        return False
    return bool(task.expected_file_changes or files_changed)


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
    raise ValueError(f"Unsupported live eval success criterion: {kind}")


def _run_shell(command: str, *, cwd: Path, timeout_seconds: int, redactor: TraceRedactor) -> CommandEvalResult:
    started = time.perf_counter()
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            shell=True,
            text=True,
            capture_output=True,
            timeout=timeout_seconds,
            check=False,
        )
        output = (completed.stderr or completed.stdout or "").strip().splitlines()
        error_summary = redactor.redact_text(output[0] if output else "")
        return CommandEvalResult(
            command=command,
            exit_code=completed.returncode,
            duration_seconds=round(time.perf_counter() - started, 3),
            error_summary=error_summary[:500],
        )
    except subprocess.TimeoutExpired:
        return CommandEvalResult(
            command=command,
            exit_code=None,
            duration_seconds=round(time.perf_counter() - started, 3),
            timed_out=True,
            error_summary=f"timed out after {timeout_seconds}s",
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


def _prepare_verification_workspace(
    *,
    source_workspace: Path,
    verification_workspace: Path,
    baseline_workspace: Path,
    before_snapshot: dict[str, str],
    root: Path,
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
    if _read_text_files(verification_workspace) != before_snapshot:
        return False
    after = _read_text_files(source_workspace)
    for path in sorted(set(before_snapshot) | set(after)):
        target = _workspace_path(verification_workspace, path)
        if path not in after:
            if target.exists():
                target.unlink()
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(after[path], encoding="utf-8")
    return _read_text_files(verification_workspace) == after


def _checks_payload(
    public: CommandEvalResult | None,
    hidden: CommandEvalResult | None,
) -> dict[str, Any]:
    return {
        "public": _check_payload(public),
        "hidden": _check_payload(hidden),
    }


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
        raise ValueError("path escapes live eval workspace.")
    return target


def _reset_dir(path: Path, *, root: Path) -> None:
    resolved = path.resolve(strict=False)
    root_resolved = root.resolve(strict=False)
    if os.path.commonpath([str(root_resolved), str(resolved)]) != str(root_resolved):
        raise ValueError("refusing to delete outside live eval run directory.")
    if resolved.exists():
        shutil.rmtree(resolved)
    resolved.mkdir(parents=True, exist_ok=True)


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
        raise ValueError(f"live eval {field_name} must be an object.")
    return dict(value)


def _safe_int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _safe_float(value: Any) -> float:
    try:
        return float(value or 0.0)
    except (TypeError, ValueError):
        return 0.0


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


def _legacy_status(payload: dict[str, Any]) -> str:
    if payload.get("infrastructure_blocked"):
        return "infrastructure_blocked"
    if payload.get("success"):
        return "success"
    if payload.get("tests_passed") is False:
        return "verification_failed"
    return "failure"
