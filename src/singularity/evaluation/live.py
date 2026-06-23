from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from uuid import uuid4

from rich.console import Console

from singularity.config import ProductionRuntimeConfig, adaptive_default_max_turns
from singularity.interaction import InteractionMode
from singularity.observability.redaction import TraceRedactor
from singularity.policy import ApprovalMode, SecurityMode


LIVE_TASK_SET_SCHEMA_VERSION = "evaluation.live_agent_task_set/v1"
LIVE_RESULT_SCHEMA_VERSION = "evaluation.live_agent_eval_result/v1"


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
    verification: CommandEvalResult | None = None
    request_cache_hit_rates: dict[str, float] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "success": self.success,
            "tests_passed": self.tests_passed,
            "infrastructure_blocked": self.infrastructure_blocked,
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
            "verification": self.verification.to_dict() if self.verification else None,
            "request_cache_hit_rates": dict(sorted(self.request_cache_hit_rates.items())),
        }


class LiveAgentEvalRunner:
    def __init__(
        self,
        *,
        output_root: Path | str | None = None,
        run_id: str | None = None,
        max_turns: int | None = None,
        model: str | None = None,
        base_url: str | None = None,
        bootstrap_cls: Any | None = None,
        console: Console | None = None,
    ) -> None:
        self.output_root = Path(output_root or Path.cwd() / "work" / "evaluations-live").resolve(strict=False)
        self.run_id = run_id or f"live_eval_{uuid4().hex[:8]}"
        self.max_turns = max_turns
        self.model = model
        self.base_url = base_url
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
        payload = {
            "schema_version": LIVE_RESULT_SCHEMA_VERSION,
            "run_id": self.run_id,
            "output_dir": str(self.run_dir),
            "summary": summarize_live_results(results),
            "tasks": [result.to_dict() for result in results],
            "duration_seconds": round(time.perf_counter() - started, 3),
        }
        result_path = self.run_dir / "result.json"
        result_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        payload["result_path"] = str(result_path)
        return payload

    def run_task(self, task: LiveEvalTask, *, manifest_base: Path) -> LiveEvalTaskResult:
        started = time.perf_counter()
        task_dir = self.run_dir / _safe_name(task.task_id)
        workspace = task_dir / "workspace"
        trace_path = ""
        verification: CommandEvalResult | None = None
        files_changed: list[str] = []
        errors: list[str] = []
        usage: dict[str, Any] = {}
        tool_calls = 0
        success_ok = False
        tests_passed = False
        kernel = None
        before_snapshot: dict[str, str] = {}
        try:
            _reset_dir(task_dir, root=self.run_dir)
            self._materialize_workspace(task, workspace=workspace, manifest_base=manifest_base)
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
                        files_changed=[],
                        usage={},
                        tool_calls=0,
                        errors=errors,
                        success=False,
                        tests_passed=False,
                        infrastructure_blocked=False,
                    )
            before_snapshot = _snapshot_files(workspace)
            goal = _task_goal(task)
            config = ProductionRuntimeConfig.from_cli(
                project_root=workspace,
                max_turns=self.max_turns or adaptive_default_max_turns(task.user_task),
                model=self.model,
                base_url=self.base_url,
                approval_mode=ApprovalMode.AUTO_SAFE,
                security_mode=SecurityMode.COMPAT,
                interaction_mode=InteractionMode.NON_INTERACTIVE,
                raw_artifacts=False,
                profile=f"live-eval:{task.task_id}",
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
            kernel = self.bootstrap_cls(project_root=workspace, config=config, console=self.console).boot(goal)
            agent_result = kernel.run_task(goal)
            trace_path = str(kernel.graph.trace.store.run_dir)
            trace_summary = _trace_summary(kernel, agent_result)
            usage = dict(trace_summary.get("model_usage_summary") or {})
            tool_calls = _safe_int(trace_summary.get("tool_calls")) or _safe_int(usage.get("tool_calls_proposed"))
            if _infrastructure_blocked(agent_result, usage=usage, tool_calls=tool_calls):
                errors.append("infrastructure blocked: model provider unavailable")
                files_changed = _changed_files(workspace, before_snapshot=before_snapshot)
                return self._task_result(
                    task=task,
                    workspace=workspace,
                    trace=trace_path,
                    started=started,
                    verification=None,
                    files_changed=files_changed,
                    usage=usage,
                    tool_calls=tool_calls,
                    errors=errors,
                    success=False,
                    tests_passed=False,
                    infrastructure_blocked=True,
                )
            files_changed = _changed_files(workspace, before_snapshot=before_snapshot)
            for command in task.verification_prepare_commands:
                prepared = _run_shell(command, cwd=workspace, timeout_seconds=120, redactor=self.redactor)
                if not prepared.passed:
                    errors.append(f"verification prepare failed: {prepared.error_summary or command}")
                    return self._task_result(
                        task=task,
                        workspace=workspace,
                        trace=trace_path,
                        started=started,
                        verification=prepared,
                        files_changed=files_changed,
                        usage=usage,
                        tool_calls=tool_calls,
                        errors=errors,
                        success=False,
                        tests_passed=False,
                        infrastructure_blocked=False,
                    )
            verification = _run_shell(
                task.verification_command,
                cwd=workspace,
                timeout_seconds=task.verification_timeout_seconds,
                redactor=self.redactor,
            )
            tests_passed = verification.passed
            allowed_ok = _allowed_scope_ok(files_changed, task.allowed_paths)
            criterion_ok = _success_criterion_ok(task.success, verification=verification, workspace=workspace)
            agent_status = getattr(agent_result.status, "value", agent_result.status)
            agent_completed = agent_status == "completed"
            if not agent_completed:
                errors.append(f"agent status: {getattr(agent_result.status, 'value', agent_result.status)}")
            if not tests_passed:
                errors.append(f"verification failed: {verification.error_summary or verification.command}")
            if not allowed_ok:
                errors.append("changed files outside allowed_paths")
            if not criterion_ok:
                errors.append("success criterion failed")
            success_ok = bool(agent_completed and tests_passed and allowed_ok and criterion_ok)
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
            files_changed=files_changed,
            usage=usage,
            tool_calls=tool_calls,
            errors=errors,
            success=success_ok,
            tests_passed=tests_passed,
            infrastructure_blocked=False,
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
        files_changed: list[str],
        usage: dict[str, Any],
        tool_calls: int,
        errors: list[str],
        success: bool,
        tests_passed: bool,
        infrastructure_blocked: bool,
    ) -> LiveEvalTaskResult:
        request_rates = _float_map(usage.get("request_cache_hit_rates") or {})
        return LiveEvalTaskResult(
            task_id=task.task_id,
            success=success,
            tests_passed=tests_passed,
            infrastructure_blocked=infrastructure_blocked,
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
            verification=verification,
            request_cache_hit_rates=request_rates,
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
    }


def _score_status(*, task_count: int, scored_task_count: int, infrastructure_blocked_count: int) -> str:
    if scored_task_count > 0:
        return "scored"
    if task_count > 0 and infrastructure_blocked_count == task_count:
        return "infrastructure_blocked"
    return "empty"


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
    verification_instruction = (
        "Before finishing, run the relevant visible checks you can infer. "
        "Hidden evaluator setup and independent verification will run after you finish."
        if task.verification_prepare_commands
        else f"Before finishing, run this verification command: {task.verification_command}"
    )
    return (
        f"{task.user_task}\n\n"
        f"Allowed modification scope: {allowed}.\n"
        f"{verification_instruction}\n"
        "Do not read, print, or modify .env files or API keys."
    )


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
        return trace.final_report_summary(task_id=task_id)
    return {}


def _infrastructure_blocked(agent_result: Any, *, usage: dict[str, Any], tool_calls: int) -> bool:
    status = str(getattr(getattr(agent_result, "status", None), "value", getattr(agent_result, "status", "")))
    if status != "failed" or _safe_int(usage.get("input_tokens")) or tool_calls:
        return False
    answer = str(getattr(agent_result, "final_answer", "") or "").lower()
    return any(marker in answer for marker in ("winerror 10013", "network", "socket", "访问权限不允许"))


def _success_criterion_ok(success: dict[str, Any], *, verification: CommandEvalResult, workspace: Path) -> bool:
    kind = str(success.get("type") or "verification_exit_code")
    if kind == "verification_exit_code":
        return verification.exit_code == int(success.get("exit_code", 0)) and not verification.timed_out
    if kind == "file_exists":
        return _workspace_path(workspace, str(success.get("path") or "")).exists()
    if kind == "file_contains":
        path = _workspace_path(workspace, str(success.get("path") or ""))
        try:
            return path.exists() and str(success.get("text") or "") in path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            return False
    if kind == "all":
        criteria = success.get("criteria") or []
        return bool(criteria) and all(
            _success_criterion_ok(_dict(item, "success.criteria"), verification=verification, workspace=workspace)
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
    return lowered.startswith(".env") or "api_key" in lowered or "token" in lowered or "secret" in lowered


def _display_path(path: str) -> str:
    return "<sensitive_path>" if any(_is_sensitive_name(part) for part in Path(path).parts) else path


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
