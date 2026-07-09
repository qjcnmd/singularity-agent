from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from singularity.evaluation.models import (
    BenchmarkAdapterKind,
    BenchmarkTask,
    BenchmarkVisibility,
    ExpectedOutcomeKind,
    WorkspaceSnapshotKind,
)
from singularity.evaluation.store import GoldenTaskStore
from singularity.policy.permissions import ApprovalPolicy, NetworkAccess, PermissionProfileName
from singularity.runtime.defaults import EVALUATION_TASK_VERIFICATION_TIMEOUT_SECONDS
from singularity.utils.attributes import nested_getattr
from singularity.utils.serialization import coerce_evaluation_dict

EVALUATION_TASK_SET_SCHEMA_VERSION = "evaluation.task_set/v1"


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
    verification_timeout_seconds: int = EVALUATION_TASK_VERIFICATION_TIMEOUT_SECONDS
    smoke_command: str = ""
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
            success=coerce_evaluation_dict(payload.get("success"), "success"),
            task_type=str(payload.get("task_type") or "").strip(),
            description=str(payload.get("description") or "").strip(),
            allowed_tools=[str(item) for item in payload.get("allowed_tools") or []],
            tool_policy=str(payload.get("tool_policy") or "read_write").strip(),
            strategy=coerce_evaluation_dict(payload.get("strategy") or {}, "strategy"),
            expected_file_changes=[
                str(item) for item in payload.get("expected_file_changes") or []
            ],
            completion_standard=str(payload.get("completion_standard") or "").strip(),
            risk_tags=[str(item) for item in payload.get("risk_tags") or []],
            prepare_commands=[str(item) for item in prepare_commands if str(item).strip()],
            public_verification_command=str(payload.get("public_verification_command") or "").strip(),
            hidden_verification_command=str(payload.get("hidden_verification_command") or "").strip(),
            verification_prepare_commands=[str(item) for item in verification_prepare_commands if str(item).strip()],
            verification_timeout_seconds=int(
                payload.get("verification_timeout_seconds")
                or EVALUATION_TASK_VERIFICATION_TIMEOUT_SECONDS
            ),
            smoke_command=str(payload.get("smoke_command") or "").strip(),
            model_visible_verification_command=str(payload.get("model_visible_verification_command") or "").strip(),
            fixture_metadata=coerce_evaluation_dict(
                payload.get("fixture_metadata") or {},
                "fixture_metadata",
            ),
            hidden_test_patch=coerce_evaluation_dict(
                payload.get("hidden_test_patch") or {},
                "hidden_test_patch",
            ),
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
        if self.smoke_command:
            payload["smoke_command"] = self.smoke_command
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


def load_evaluation_task_set(path: Path | str) -> EvaluationTaskSet:
    manifest_path = Path(path)
    payload = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("evaluation manifest must be a JSON object.")
    return EvaluationTaskSet.from_dict(payload, base_dir=manifest_path.parent.resolve(strict=False))


def _workspace_payload(payload: dict[str, Any]) -> dict[str, Any]:
    workspace = payload.get("workspace")
    if isinstance(workspace, dict):
        if payload.get("start_commit") and not workspace.get("start_commit"):
            workspace = {**workspace, "start_commit": payload["start_commit"]}
        return workspace
    if "fixture_workspace" in payload:
        fixture = coerce_evaluation_dict(
            payload.get("fixture_workspace"),
            "fixture_workspace",
        )
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
    planner = nested_getattr(kernel, "graph.planner")
    apply_constraints = getattr(planner, "apply_benchmark_constraints", None)
    if not callable(apply_constraints):
        return
    apply_constraints(_model_visible_benchmark_constraints(task))


def _model_visible_benchmark_constraints(task: EvaluationTask) -> dict[str, Any]:
    verification_command = _model_visible_verification_command(task)
    if _requires_baseline_verification(task) and not task.smoke_command and not task.model_visible_verification_command:
        verification_command = ""
    return {
        "task_id": task.task_id,
        "allowed_tools": task.allowed_tools,
        "expected_file_changes": task.expected_file_changes,
        "completion_standard": task.completion_standard,
        "risk_tags": task.risk_tags,
        "verification_command": verification_command,
    }


def _public_verification_command(task: EvaluationTask) -> str:
    if task.public_verification_command:
        return task.public_verification_command
    if task.verification_prepare_commands:
        return ""
    return task.verification_command


def _hidden_verification_command(task: EvaluationTask) -> str:
    return task.hidden_verification_command or task.verification_command


def _model_visible_verification_command(task: EvaluationTask) -> str:
    if task.smoke_command:
        return task.smoke_command
    if task.model_visible_verification_command:
        return task.model_visible_verification_command
    if task.verification_prepare_commands:
        return task.public_verification_command
    return task.verification_command


def _requires_baseline_verification(task: EvaluationTask) -> bool:
    if task.task_type == "public_representative":
        return True
    metadata = task.fixture_metadata
    return bool(metadata.get("fail_to_pass") or metadata.get("FAIL_TO_PASS") or task.test_patch)


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
