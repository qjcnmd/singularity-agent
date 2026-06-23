from __future__ import annotations

import hashlib
import json
import os
import shlex
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from singularity.command import (
    CommandPurpose,
    CommandRequest,
    CommandRuntime,
    FilesystemMode,
)
from singularity.evaluation.models import (
    BenchmarkTask,
    EvaluationHook,
    ExpectedOutcome,
    ExpectedOutcomeKind,
    WorkspaceSnapshotKind,
)
from singularity.observability.models import TraceArtifactKind, TraceEventType
from singularity.verification.models import CheckKind, VerificationCheck
from singularity.verification.runtime import VerificationRuntime
from singularity.workspace import MutationRuntime
from singularity.workspace.errors import MutationError
from singularity.workspace.operations import CreateFile
from singularity.workspace.pathing import WorkspacePathResolver


@dataclass(frozen=True)
class TaskExecutionEvidence:
    verification: dict[str, Any]
    assertions: dict[str, Any]
    diff: dict[str, Any]
    heuristics: dict[str, float]
    trace_metrics: dict[str, Any]
    diff_summary: list[dict[str, Any]]
    hook_results: list[dict[str, Any]] = field(default_factory=list)
    snapshot: dict[str, Any] = field(default_factory=dict)
    runtime_overrides: dict[str, Any] = field(default_factory=dict)
    golden_contract: dict[str, Any] = field(default_factory=dict)
    failure_reasons: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "verification": self.verification,
            "assertions": self.assertions,
            "diff": self.diff,
            "heuristics": self.heuristics,
            "trace_metrics": self.trace_metrics,
            "diff_summary": self.diff_summary,
            "hook_results": self.hook_results,
            "snapshot": self.snapshot,
            "runtime_overrides": self.runtime_overrides,
            "golden_contract": self.golden_contract,
            "failure_reasons": self.failure_reasons,
        }


class EvaluationTaskExecutor:
    def __init__(
        self,
        *,
        project_root: Path | str,
        command_runtime: CommandRuntime | None = None,
        verification_runtime: VerificationRuntime | None = None,
        mutation_runtime: MutationRuntime | None = None,
        trace_runtime: Any | None = None,
    ) -> None:
        self.project_root = Path(project_root).resolve(strict=False)
        self.command_runtime = command_runtime
        self.verification_runtime = verification_runtime
        self.mutation_runtime = mutation_runtime
        self.trace_runtime = trace_runtime
        self.path_resolver = WorkspacePathResolver(self.project_root)

    def evaluate(
        self,
        task: BenchmarkTask,
        *,
        runtime_overrides: dict[str, Any],
        execute: bool,
    ) -> TaskExecutionEvidence:
        snapshot = self.prepare_snapshot(task, execute=execute)
        before = _capture_text_snapshot(self.project_root) if execute else {}
        hook_results: list[dict[str, Any]] = []
        hook_results.extend(self.run_hooks(task.evaluation_hooks, stage="before_run", execute=execute))
        verification = self.evaluate_tests(task.expected_outcomes, execute=execute)
        assertions = self.evaluate_assertions(task.expected_outcomes)
        diff = self.evaluate_diff(task.expected_outcomes, before_snapshot=before, execute=execute)
        hook_results.extend(self.run_hooks(task.evaluation_hooks, stage="after_run", execute=execute))
        after = _capture_text_snapshot(self.project_root) if execute else {}
        diff_summary = _diff_summary(before, after) if execute else []
        hook_results.extend(
            self.run_hooks(task.evaluation_hooks, stage="score_adjustment", execute=execute)
        )
        trace_metrics = {
            "policy_denials": int(verification.get("policy_denials", 0) or 0),
            "interventions": 0,
            "tool_calls": len(hook_results) + int(verification.get("checks", 0) or 0),
            "latency_ms": int(verification.get("duration_ms", 0) or 0)
            + sum(int(item.get("duration_ms", 0) or 0) for item in hook_results),
            "cost": 0.0,
        }
        heuristics = self.evaluate_heuristics(
            task.expected_outcomes,
            verification=verification,
            diff_summary=diff_summary,
        )
        golden_contract = self.evaluate_golden_contract(
            task,
            verification=verification,
            assertions=assertions,
            diff=diff,
            diff_summary=diff_summary,
        )
        failure_reasons = []
        failure_reasons.extend(snapshot.get("failure_reasons") or [])
        if diff.get("failure_reason"):
            failure_reasons.append(str(diff["failure_reason"]))
        failure_reasons.extend(item["error_code"] for item in hook_results if item.get("error_code"))
        return TaskExecutionEvidence(
            verification=verification,
            assertions=assertions,
            diff=diff,
            heuristics=heuristics,
            trace_metrics=trace_metrics,
            diff_summary=diff_summary,
            hook_results=hook_results,
            snapshot=snapshot,
            runtime_overrides=runtime_overrides,
            golden_contract=golden_contract,
            failure_reasons=failure_reasons,
        )

    def prepare_snapshot(self, task: BenchmarkTask, *, execute: bool) -> dict[str, Any]:
        snapshot = task.workspace_snapshot
        payload = {"kind": snapshot.kind.value, "prepared": False, "execute": execute}
        if snapshot.kind == WorkspaceSnapshotKind.GIT_REF:
            payload["git_ref"] = snapshot.git_ref
            payload["prepared"] = True
            return payload
        if snapshot.kind == WorkspaceSnapshotKind.BASELINE_TRACE_RUN_ID:
            payload["baseline_trace_run_id"] = snapshot.baseline_trace_run_id
            payload["prepared"] = True
            return payload
        if not execute:
            payload["failure_reasons"] = ["snapshot_requires_execution"]
            return payload
        if snapshot.kind == WorkspaceSnapshotKind.INLINE_FILES:
            operations = [
                CreateFile(path=path, content=content)
                for path, content in sorted(snapshot.inline_files.items())
            ]
            if self.mutation_runtime is None:
                payload["failure_reasons"] = ["mutation_runtime_unavailable"]
                return payload
            result = self.mutation_runtime.apply_operations(
                operations,
                intent=f"materialize benchmark snapshot {task.task_id}",
                created_by="EvaluationRuntime",
            )
            payload.update(
                {
                    "prepared": bool(result.ok),
                    "transaction_id": result.transaction_id,
                    "changed_files": result.affected_files,
                    "error_code": result.error_code,
                }
            )
            if not result.ok:
                payload["failure_reasons"] = [result.error_code or "snapshot_failed"]
            return payload
        if snapshot.kind == WorkspaceSnapshotKind.ARCHIVE_PATH:
            # Archive materialization must not bypass MutationRuntime or workspace policy.
            # Until a safe staging-to-mutation adapter exists, classify it as supported
            # schema input but block execution instead of unpacking directly.
            archive_path = Path(str(snapshot.archive_path or ""))
            payload["archive_path"] = str(archive_path)
            payload["failure_reasons"] = ["archive_snapshot_requires_controlled_restore"]
            return payload
        return payload

    def run_hooks(
        self,
        hooks: list[EvaluationHook],
        *,
        stage: str,
        execute: bool,
    ) -> list[dict[str, Any]]:
        return [
            self._run_hook(hook, execute=execute)
            for hook in hooks
            if hook.stage == stage
        ]

    def evaluate_tests(
        self,
        outcomes: list[ExpectedOutcome],
        *,
        execute: bool,
    ) -> dict[str, Any]:
        test_outcomes = [item for item in outcomes if item.kind == ExpectedOutcomeKind.TEST]
        if not test_outcomes:
            return {"status": "not_required", "passed": 0, "failed": 0, "checks": 0}
        if not execute:
            return {
                "status": "blocked",
                "passed": 0,
                "failed": len(test_outcomes),
                "checks": len(test_outcomes),
                "failure_reason": "execution_disabled",
            }
        passed = 0
        failed = 0
        duration_ms = 0
        policy_denials = 0
        results: list[dict[str, Any]] = []
        for index, outcome in enumerate(test_outcomes, start=1):
            if not outcome.command:
                results.append({"status": "blocked", "reason": "missing_command"})
                failed += 1
                continue
            command_result = self._run_verification_command(
                command=outcome.command,
                check_id=f"eval_test_{index}",
            )
            results.append(command_result)
            duration_ms += int(command_result.get("duration_ms", 0) or 0)
            if command_result.get("passed"):
                passed += 1
            else:
                failed += 1
            if command_result.get("policy_denied"):
                policy_denials += 1
        return {
            "status": "ready" if failed == 0 else "failed",
            "passed": passed,
            "failed": failed,
            "checks": len(test_outcomes),
            "duration_ms": duration_ms,
            "policy_denials": policy_denials,
            "results": results,
        }

    def evaluate_assertions(self, outcomes: list[ExpectedOutcome]) -> dict[str, Any]:
        assertion_outcomes = [
            item for item in outcomes if item.kind == ExpectedOutcomeKind.ASSERTION
        ]
        if not assertion_outcomes:
            return {"passed": 0, "failed": 0}
        passed = 0
        failed = 0
        results: list[dict[str, Any]] = []
        for outcome in assertion_outcomes:
            ok = self._evaluate_assertion(str(outcome.assertion or ""))
            passed += 1 if ok else 0
            failed += 0 if ok else 1
            results.append({"assertion": outcome.assertion, "passed": ok})
        return {"passed": passed, "failed": failed, "results": results}

    def evaluate_diff(
        self,
        outcomes: list[ExpectedOutcome],
        *,
        before_snapshot: dict[str, str],
        execute: bool,
    ) -> dict[str, Any]:
        diff_outcomes = [item for item in outcomes if item.kind == ExpectedOutcomeKind.DIFF]
        if not diff_outcomes:
            return {"matched": True, "passed": 0, "failed": 0}
        if not execute:
            return {
                "status": "blocked",
                "matched": False,
                "passed": 0,
                "failed": len(diff_outcomes),
                "failure_reason": "diff_requires_execution",
            }
        after = _capture_text_snapshot(self.project_root)
        summary = _diff_summary(before_snapshot, after)
        changed_paths = {item["path"] for item in summary}
        passed = 0
        failed = 0
        for outcome in diff_outcomes:
            expected = outcome.expected_diff or {}
            paths = set(str(path) for path in expected.get("paths", []) or [])
            max_changed_lines = expected.get("max_changed_lines")
            changed_lines = sum(int(item["added_lines"]) + int(item["removed_lines"]) for item in summary)
            ok = (not paths or paths.issubset(changed_paths)) and (
                max_changed_lines is None or changed_lines <= int(max_changed_lines)
            )
            passed += 1 if ok else 0
            failed += 0 if ok else 1
        return {
            "matched": failed == 0,
            "passed": passed,
            "failed": failed,
            "changed_paths": sorted(changed_paths),
            "summary": summary,
        }

    def evaluate_heuristics(
        self,
        outcomes: list[ExpectedOutcome],
        *,
        verification: dict[str, Any],
        diff_summary: list[dict[str, Any]],
    ) -> dict[str, float]:
        heuristics: dict[str, float] = {}
        for outcome in outcomes:
            if outcome.kind != ExpectedOutcomeKind.HEURISTIC:
                continue
            name = outcome.heuristic or "patch_quality"
            if name == "patch_quality":
                heuristics[name] = _patch_quality_proxy(diff_summary, verification)
            elif name == "planner_completion":
                heuristics[name] = 1.0 if verification.get("status") in {"ready", "passed"} else 0.0
            else:
                heuristics[name] = float(outcome.metadata.get("score", 0.0) or 0.0)
        return heuristics

    def evaluate_golden_contract(
        self,
        task: BenchmarkTask,
        *,
        verification: dict[str, Any],
        assertions: dict[str, Any],
        diff: dict[str, Any],
        diff_summary: list[dict[str, Any]],
    ) -> dict[str, Any]:
        contract = task.golden_contract
        if contract is None:
            return {}
        changed_paths = {str(item.get("path")) for item in diff_summary}
        diff_paths = set(str(path) for path in diff.get("changed_paths") or [])
        assertion_results = assertions.get("results") or []
        assertion_sources = {
            str(item.get("assertion"))
            for item in assertion_results
            if item.get("passed")
        }
        verification_commands = {
            str(result.get("command"))
            for result in verification.get("results", [])
            if result.get("command")
        }
        return {
            "scenario": contract.scenario,
            "expected_files": [
                {
                    "path": path,
                    "declared": True,
                    "changed": path in changed_paths or path in diff_paths,
                    "exists": (self.project_root / path).exists(),
                }
                for path in contract.expected_files
            ],
            "expected_commands": [
                {
                    "command": command,
                    "declared": True,
                    "observed": command in verification_commands,
                }
                for command in contract.expected_commands
            ],
            "expected_evidence": [
                {
                    "name": name,
                    "declared": True,
                    "observed": _evidence_observed(
                        name,
                        verification=verification,
                        diff_summary=diff_summary,
                        assertions=assertion_sources,
                    ),
                }
                for name in contract.expected_evidence
            ],
            "expected_report_sections": [
                {"section": section, "declared": True}
                for section in contract.expected_report_sections
            ],
            "required_trace_artifacts": [
                {
                    "kind": kind,
                    "required": True,
                    "artifact_ref": f"required:{task.task_id}:{kind}",
                }
                for kind in contract.required_trace_artifacts
            ],
        }

    def _run_hook(self, hook: EvaluationHook, *, execute: bool) -> dict[str, Any]:
        if not execute:
            return {
                "name": hook.name,
                "stage": hook.stage,
                "status": "blocked",
                "args": hook.args,
                "error_code": "execution_disabled",
            }
        if hook.command:
            command = _append_shell_args(hook.command, hook.args)
            result = self._run_command(
                command,
                purpose=CommandPurpose.PROJECT_VERIFICATION,
                timeout_seconds=hook.timeout_seconds,
            )
            result.update({"name": hook.name, "stage": hook.stage, "args": hook.args})
            return result
        if hook.module:
            result = self._run_command(
                argv=["python", "-m", hook.module, *_hook_args_to_argv(hook.args)],
                purpose=CommandPurpose.PROJECT_VERIFICATION,
                timeout_seconds=hook.timeout_seconds,
            )
            result.update({"name": hook.name, "stage": hook.stage, "args": hook.args})
            return result
        return {
            "name": hook.name,
            "stage": hook.stage,
            "status": "blocked",
            "args": hook.args,
            "error_code": "missing_hook_target",
        }

    def _run_verification_command(self, *, command: str, check_id: str) -> dict[str, Any]:
        if self.verification_runtime is None:
            return self._run_command(command, purpose=CommandPurpose.PROJECT_VERIFICATION)
        check = VerificationCheck(
            id=check_id,
            kind=CheckKind.CUSTOM,
            command=CommandRequest(
                shell=command,
                cwd=".",
                purpose=CommandPurpose.PROJECT_VERIFICATION,
                filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
                timeout_seconds=120,
            ),
            scope="benchmark_expected_outcome",
            required=True,
            timeout=120,
            risk_tags=["benchmark"],
            failure_policy="fail",
            source="benchmark_task",
        )
        # VerificationRuntime owns policy checks and delegates process execution to CommandRuntime.
        plan = self.verification_runtime.plan_verification(
            changed_files=[],
            task_intent="benchmark expected outcome",
        )
        result = self.verification_runtime._run_check(plan, check)
        return {
            "passed": result.status.value == "passed",
            "status": result.status.value,
            "duration_ms": result.duration_ms,
            "exit_code": result.evidence.exit_code,
            "command": command,
            "output_excerpt": result.evidence.output_excerpt,
            "policy_denied": bool(
                result.policy_decision and result.policy_decision.error_code
            ),
        }

    def _run_command(
        self,
        command: str | None = None,
        *,
        purpose: CommandPurpose,
        timeout_seconds: int | None = None,
        argv: list[str] | None = None,
    ) -> dict[str, Any]:
        if self.command_runtime is None:
            return {"status": "blocked", "error_code": "command_runtime_unavailable"}
        result = self.command_runtime.run(
            CommandRequest(
                argv=argv,
                shell=command if argv is None else None,
                cwd=".",
                purpose=purpose,
                filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
                timeout_seconds=timeout_seconds or 120,
            )
        )
        return {
            "status": result.semantic_status.value,
            "passed": result.exit_code == 0 and result.error_code is None,
            "duration_ms": result.duration_ms,
            "exit_code": result.exit_code,
            "error_code": result.error_code,
            "output_excerpt": result.combined_output_preview,
        }

    def _evaluate_assertion(self, expression: str) -> bool:
        try:
            if not expression:
                return False
            if expression.startswith("file_exists:"):
                target = self._resolve_assertion_path(expression.removeprefix("file_exists:"))
                return bool(target and target.exists())
            if expression.startswith("file_contains:"):
                _, path, needle = expression.split(":", 2)
                target = self._resolve_assertion_path(path)
                if target is None:
                    return False
                return target.exists() and needle in target.read_text(encoding="utf-8")
            if expression.startswith("json:"):
                _, path, key, expected = expression.split(":", 3)
                target = self._resolve_assertion_path(path)
                if target is None or not target.exists():
                    return False
                payload = json.loads(target.read_text(encoding="utf-8"))
                value: Any = payload
                for part in key.split("."):
                    value = value[part]
                return str(value) == expected
        except (OSError, UnicodeDecodeError, ValueError, KeyError, TypeError, json.JSONDecodeError):
            return False
        return False

    def _resolve_assertion_path(self, path: str) -> Path | None:
        try:
            return self.path_resolver.resolve(path.strip()).path
        except MutationError:
            return None


class EvaluationArtifactWriter:
    def __init__(
        self,
        *,
        project_root: Path | str,
        output_root: Path | str,
        trace_runtime: Any | None = None,
    ) -> None:
        self.project_root = Path(project_root).resolve(strict=False)
        self.output_root = Path(output_root)
        self.trace_runtime = trace_runtime

    def write_report(self, *, run_id: str, json_text: str, markdown_text: str) -> Path:
        output_dir = self._run_output_dir(run_id)
        output_dir.mkdir(parents=True, exist_ok=True)
        json_path = output_dir / "report.json"
        md_path = output_dir / "report.md"
        json_path.write_text(json_text, encoding="utf-8")
        md_path.write_text(markdown_text, encoding="utf-8")
        if self.trace_runtime is not None and hasattr(self.trace_runtime, "write_artifact"):
            artifact = self.trace_runtime.write_artifact(
                kind=TraceArtifactKind.REPORT,
                path=json_path,
                summary="Evaluation report JSON.",
                metadata={"run_id": run_id, "report_path": str(json_path)},
            )
            if hasattr(self.trace_runtime, "emit"):
                self.trace_runtime.emit(
                    TraceEventType.FINAL_REPORT_CREATED,
                    runtime="evaluation",
                    summary="Evaluation report written.",
                    payload={
                        "run_id": run_id,
                        "output_dir": str(output_dir),
                        "artifact_id": artifact.artifact_id,
                    },
                    artifact_refs=[artifact.artifact_id],
                )
        return output_dir

    def previous_report_payload(self, *, run_id: str) -> dict[str, Any] | None:
        candidates = []
        if not self.output_root.exists():
            return None
        for report_path in self.output_root.glob("*/report.json"):
            if report_path.parent.name == run_id:
                continue
            try:
                candidates.append((report_path.stat().st_mtime_ns, report_path))
            except OSError:
                continue
        for _mtime, report_path in sorted(candidates, reverse=True):
            try:
                payload = json.loads(report_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                continue
            if isinstance(payload, dict) and isinstance(payload.get("metrics"), dict):
                return payload
        return None

    def write_regression_report(
        self,
        *,
        run_id: str,
        json_text: str,
        markdown_text: str,
    ) -> Path:
        output_dir = self._run_output_dir(run_id)
        output_dir.mkdir(parents=True, exist_ok=True)
        json_path = output_dir / "regression.json"
        md_path = output_dir / "regression.md"
        json_path.write_text(json_text, encoding="utf-8")
        md_path.write_text(markdown_text, encoding="utf-8")
        regression_artifact_refs: list[str] = []
        if self.trace_runtime is not None and hasattr(self.trace_runtime, "write_artifact"):
            for regression in _regressions_from_report_json(json_text):
                item_artifact = self.trace_runtime.write_artifact(
                    kind=TraceArtifactKind.REPORT,
                    text=json.dumps(regression, ensure_ascii=False, sort_keys=True),
                    summary="Evaluation regression artifact.",
                    metadata={
                        "artifact_type": "evaluation_regression",
                        "run_id": run_id,
                        "task_id": regression.get("task_id"),
                        "metric": regression.get("metric"),
                        "trace_artifact_ref": regression.get("trace_artifact_ref"),
                    },
                )
                regression_artifact_refs.append(item_artifact.artifact_id)
            artifact = self.trace_runtime.write_artifact(
                kind=TraceArtifactKind.REPORT,
                path=json_path,
                summary="Evaluation regression report JSON.",
                metadata={
                    "run_id": run_id,
                    "report_path": str(json_path),
                    "regression_artifacts": regression_artifact_refs,
                },
            )
            if hasattr(self.trace_runtime, "emit"):
                self.trace_runtime.emit(
                    TraceEventType.FINAL_REPORT_CREATED,
                    runtime="evaluation",
                    summary="Evaluation regression report written.",
                    payload={
                        "run_id": run_id,
                        "output_dir": str(output_dir),
                        "artifact_id": artifact.artifact_id,
                    },
                    artifact_refs=[artifact.artifact_id],
                )
        return output_dir

    def _run_output_dir(self, run_id: str) -> Path:
        output_dir = (self.output_root / run_id).resolve(strict=False)
        allowed_root = self.output_root.resolve(strict=False)
        if os.path.commonpath([str(allowed_root), str(output_dir)]) != str(allowed_root):
            raise ValueError("Evaluation report output must stay under the evaluation output root.")
        return output_dir


def _capture_text_snapshot(root: Path) -> dict[str, str]:
    files: dict[str, str] = {}
    skip = {
        ".git",
        ".singularity",
        ".venv",
        ".pytest_cache",
        ".ruff_cache",
        "work",
        "outputs",
        "__pycache__",
    }
    sensitive_names = {".env", ".env.local", ".env.production", ".env.development"}
    if not root.exists():
        return files
    for path in root.rglob("*"):
        if not path.is_file():
            continue
        try:
            relative = path.relative_to(root)
        except ValueError:
            continue
        if any(part in skip for part in relative.parts):
            continue
        if any(part in sensitive_names for part in relative.parts):
            continue
        try:
            files[relative.as_posix()] = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
    return files


def _diff_summary(before: dict[str, str], after: dict[str, str]) -> list[dict[str, Any]]:
    paths = sorted(set(before) | set(after))
    summary: list[dict[str, Any]] = []
    for path in paths:
        old = before.get(path)
        new = after.get(path)
        if old == new:
            continue
        old_lines = old.splitlines() if old is not None else []
        new_lines = new.splitlines() if new is not None else []
        added = max(0, len(new_lines) - len(old_lines))
        removed = max(0, len(old_lines) - len(new_lines))
        if added == 0 and removed == 0:
            digest_old = hashlib.sha256((old or "").encode("utf-8")).hexdigest()
            digest_new = hashlib.sha256((new or "").encode("utf-8")).hexdigest()
            changed = 1 if digest_old != digest_new else 0
            added = changed
            removed = changed
        summary.append(
            {
                "path": path,
                "added_lines": added,
                "removed_lines": removed,
                "complexity": 0,
                "redundant_code": False,
            }
        )
    return summary


def _evidence_observed(
    name: str,
    *,
    verification: dict[str, Any],
    diff_summary: list[dict[str, Any]],
    assertions: set[str],
) -> bool:
    normalized = name.strip().lower()
    if normalized in {"verification_passed", "test_passed", "smoke_verified"}:
        return str(verification.get("status", "")).lower() in {
            "ready",
            "passed",
            "success",
            "ready_with_warnings",
        } or int(verification.get("passed", 0) or 0) > 0
    if normalized in {"verification_failed", "test_failed"}:
        return str(verification.get("status", "")).lower() in {"failed", "failure"}
    if normalized in {"file_created", "file_modified", "diff_observed"}:
        return bool(diff_summary)
    if normalized in {"assertion_passed", "report_artifact_written"}:
        return bool(assertions)
    if normalized in {
        "completion_rejected",
        "continued_after_rejection",
        "review_rejected",
        "repair_applied",
        "approval_required",
        "resume_recorded",
        "sandbox_fail_closed",
        "dynamic_retrieval_recorded",
        "memory_write_gated",
        "final_report_written",
    }:
        return False
    return False


def _regressions_from_report_json(json_text: str) -> list[dict[str, Any]]:
    try:
        payload = json.loads(json_text)
    except json.JSONDecodeError:
        return []
    regressions = payload.get("regressions", [])
    if not isinstance(regressions, list):
        return []
    return [item for item in regressions if isinstance(item, dict)]


def _append_shell_args(command: str, args: dict[str, Any]) -> str:
    argv = _hook_args_to_argv(args)
    if not argv:
        return command
    if os.name == "nt":
        return command + " " + subprocess.list2cmdline(argv)
    return command + " " + " ".join(shlex.quote(item) for item in argv)


def _hook_args_to_argv(args: dict[str, Any]) -> list[str]:
    argv: list[str] = []
    for key, value in args.items():
        flag = "--" + str(key).replace("_", "-")
        if value is None or value is False:
            continue
        if value is True:
            argv.append(flag)
            continue
        if isinstance(value, (list, tuple)):
            for item in value:
                argv.extend([flag, str(item)])
            continue
        argv.extend([flag, str(value)])
    return argv


def _patch_quality_proxy(diff_summary: list[dict[str, Any]], verification: dict[str, Any]) -> float:
    changed = sum(int(item.get("added_lines", 0)) + int(item.get("removed_lines", 0)) for item in diff_summary)
    score = 1.0
    if changed > 80:
        score -= 0.25
    if verification.get("status") not in {"ready", "passed", "not_required"}:
        score -= 0.25
    return max(0.0, min(1.0, score))
