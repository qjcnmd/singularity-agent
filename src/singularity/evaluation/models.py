from __future__ import annotations

import copy
import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import Enum, StrEnum
from pathlib import Path
from typing import Any, TypeVar, cast

from singularity.utils.serialization import coerce_dict, coerce_enum

SCHEMA_VERSION = "evaluation.benchmark_task/v1"
FAILURE_CASE_RECORD_SCHEMA_VERSION = "evaluation.failure_case_record/v1"
EnumT = TypeVar("EnumT", bound=Enum)


class TaskDifficulty(StrEnum):
    EASY = "easy"
    MEDIUM = "medium"
    HARD = "hard"


class BenchmarkTaskKind(StrEnum):
    REPO_ISSUE_REPAIR = "repo_issue_repair"
    TERMINAL_TASK = "terminal_task"
    SINGULARITY_INTERNAL = "singularity_internal"


class BenchmarkVisibility(StrEnum):
    PUBLIC = "public"
    PRIVATE = "private"


class BenchmarkAdapterKind(StrEnum):
    SINGULARITY_PRIVATE = "singularity_private"
    SWE_BENCH = "swe_bench"
    TERMINAL_BENCH = "terminal_bench"


class WorkspaceSnapshotKind(StrEnum):
    GIT_REF = "git_ref"
    ARCHIVE_PATH = "archive_path"
    INLINE_FILES = "inline_files"
    BASELINE_TRACE_RUN_ID = "baseline_trace_run_id"


class ExpectedOutcomeKind(StrEnum):
    TEST = "test"
    ASSERTION = "assertion"
    DIFF = "diff"
    HEURISTIC = "heuristic"


VALID_DIFFICULTY_TAGS = {item.value for item in TaskDifficulty}
VALID_AGENT_HARNESS_TAGS = {"memory-heavy", "tool-heavy", "golden-contract"}
VALID_TAGS = VALID_DIFFICULTY_TAGS | VALID_AGENT_HARNESS_TAGS
VALID_TASK_VERSIONS = {"v1", "v2"}
VALID_HOOK_STAGES = {"before_run", "after_run", "score_adjustment"}
GOLDEN_CONTRACT_FIELDS = {
    "scenario",
    "expected_files",
    "expected_commands",
    "expected_evidence",
    "expected_report_sections",
    "required_trace_artifacts",
}


@dataclass(frozen=True)
class TaskInput:
    prompt: str
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.prompt.strip():
            raise ValueError("BenchmarkTask requires input.prompt.")

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {"prompt": self.prompt}
        if self.metadata:
            payload["metadata"] = _copy_jsonish(self.metadata)
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any] | str) -> TaskInput:
        if isinstance(payload, str):
            return cls(prompt=payload)
        return cls(
            prompt=str(payload.get("prompt", "")),
            metadata=_dict(payload.get("metadata")),
        )


@dataclass(frozen=True)
class WorkspaceSnapshot:
    kind: WorkspaceSnapshotKind
    git_ref: str | None = None
    archive_path: str | Path | None = None
    inline_files: dict[str, str] = field(default_factory=dict)
    baseline_trace_run_id: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "kind", _enum(WorkspaceSnapshotKind, self.kind))
        if self.kind == WorkspaceSnapshotKind.GIT_REF and not self.git_ref:
            raise ValueError("workspace_snapshot.git_ref is required for git_ref snapshots.")
        if self.kind == WorkspaceSnapshotKind.ARCHIVE_PATH and not self.archive_path:
            raise ValueError("workspace_snapshot.archive_path is required for archive snapshots.")
        if self.kind == WorkspaceSnapshotKind.INLINE_FILES and not self.inline_files:
            raise ValueError("workspace_snapshot.inline_files is required for inline file snapshots.")
        if self.kind == WorkspaceSnapshotKind.BASELINE_TRACE_RUN_ID and not self.baseline_trace_run_id:
            raise ValueError(
                "workspace_snapshot.baseline_trace_run_id is required for trace snapshots."
            )

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {"kind": self.kind.value}
        if self.git_ref:
            payload["git_ref"] = self.git_ref
        if self.archive_path:
            payload["archive_path"] = str(self.archive_path)
        if self.inline_files:
            payload["inline_files"] = dict(sorted(self.inline_files.items()))
        if self.baseline_trace_run_id:
            payload["baseline_trace_run_id"] = self.baseline_trace_run_id
        if self.metadata:
            payload["metadata"] = _copy_jsonish(self.metadata)
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> WorkspaceSnapshot:
        return cls(
            kind=_enum(WorkspaceSnapshotKind, str(payload.get("kind", ""))),
            git_ref=payload.get("git_ref"),
            archive_path=payload.get("archive_path"),
            inline_files={str(key): str(value) for key, value in _dict(payload.get("inline_files")).items()},
            baseline_trace_run_id=payload.get("baseline_trace_run_id"),
            metadata=_dict(payload.get("metadata")),
        )


@dataclass(frozen=True)
class ExpectedOutcome:
    kind: ExpectedOutcomeKind
    weight: float = 1.0
    command: str | None = None
    assertion: str | None = None
    expected_diff: dict[str, Any] = field(default_factory=dict)
    heuristic: str | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "kind", _enum(ExpectedOutcomeKind, self.kind))
        if self.weight < 0:
            raise ValueError("expected_outcomes.weight must be non-negative.")
        if self.kind == ExpectedOutcomeKind.HEURISTIC and not self.heuristic:
            object.__setattr__(self, "heuristic", "patch_quality")

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {"kind": self.kind.value, "weight": self.weight}
        if self.command:
            payload["command"] = self.command
        if self.assertion:
            payload["assertion"] = self.assertion
        if self.expected_diff:
            payload["expected_diff"] = _copy_jsonish(self.expected_diff)
        if self.heuristic:
            payload["heuristic"] = self.heuristic
        if self.metadata:
            payload["metadata"] = _copy_jsonish(self.metadata)
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> ExpectedOutcome:
        return cls(
            kind=_enum(ExpectedOutcomeKind, str(payload.get("kind", ""))),
            weight=float(payload.get("weight", 1.0)),
            command=payload.get("command"),
            assertion=payload.get("assertion"),
            expected_diff=_dict(payload.get("expected_diff")),
            heuristic=payload.get("heuristic"),
            metadata=_dict(payload.get("metadata")),
        )


@dataclass(frozen=True)
class EvaluationHook:
    name: str
    stage: str
    command: str | None = None
    module: str | None = None
    args: dict[str, Any] = field(default_factory=dict)
    timeout_seconds: int | None = None

    def __post_init__(self) -> None:
        if not self.name.strip():
            raise ValueError("evaluation_hooks.name is required.")
        if self.stage not in VALID_HOOK_STAGES:
            raise ValueError(f"Unsupported evaluation hook stage: {self.stage}")
        if bool(self.command) == bool(self.module):
            raise ValueError("Evaluation hooks must declare exactly one of command or module.")

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {"name": self.name, "stage": self.stage}
        if self.command:
            payload["command"] = self.command
        if self.module:
            payload["module"] = self.module
        if self.args:
            payload["args"] = _copy_jsonish(self.args)
        if self.timeout_seconds is not None:
            payload["timeout_seconds"] = self.timeout_seconds
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> EvaluationHook:
        return cls(
            name=str(payload.get("name", "")),
            stage=str(payload.get("stage", "")),
            command=payload.get("command"),
            module=payload.get("module"),
            args=_dict(payload.get("args")),
            timeout_seconds=payload.get("timeout_seconds"),
        )


@dataclass(frozen=True)
class GoldenTaskContract:
    scenario: str
    expected_files: list[str] = field(default_factory=list)
    expected_commands: list[str] = field(default_factory=list)
    expected_evidence: list[str] = field(default_factory=list)
    expected_report_sections: list[str] = field(default_factory=list)
    required_trace_artifacts: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.scenario.strip():
            raise ValueError("golden_contract.scenario is required.")
        for field_name in sorted(GOLDEN_CONTRACT_FIELDS - {"scenario"}):
            values = getattr(self, field_name)
            if not values:
                raise ValueError(f"golden_contract.{field_name} requires at least one item.")
            object.__setattr__(self, field_name, [str(item) for item in values])

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "scenario": self.scenario,
            "expected_files": list(self.expected_files),
            "expected_commands": list(self.expected_commands),
            "expected_evidence": list(self.expected_evidence),
            "expected_report_sections": list(self.expected_report_sections),
            "required_trace_artifacts": list(self.required_trace_artifacts),
        }
        if self.metadata:
            payload["metadata"] = _copy_jsonish(self.metadata)
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> GoldenTaskContract:
        missing = sorted(field for field in GOLDEN_CONTRACT_FIELDS if field not in payload)
        if missing:
            raise ValueError(f"golden_contract missing fields: {', '.join(missing)}")
        return cls(
            scenario=str(payload.get("scenario", "")),
            expected_files=[str(item) for item in payload.get("expected_files") or []],
            expected_commands=[str(item) for item in payload.get("expected_commands") or []],
            expected_evidence=[str(item) for item in payload.get("expected_evidence") or []],
            expected_report_sections=[
                str(item) for item in payload.get("expected_report_sections") or []
            ],
            required_trace_artifacts=[
                str(item) for item in payload.get("required_trace_artifacts") or []
            ],
            metadata=_dict(payload.get("metadata")),
        )


@dataclass(frozen=True)
class BenchmarkTask:
    task_id: str
    version: str
    title: str
    task_type: BenchmarkTaskKind = BenchmarkTaskKind.SINGULARITY_INTERNAL
    visibility: BenchmarkVisibility = BenchmarkVisibility.PRIVATE
    adapter: BenchmarkAdapterKind = BenchmarkAdapterKind.SINGULARITY_PRIVATE
    input: TaskInput = field(init=False)
    workspace_snapshot: WorkspaceSnapshot = field(init=False)
    expected_outcomes: list[ExpectedOutcome] = field(default_factory=list, init=False)
    evaluation_hooks: list[EvaluationHook] = field(default_factory=list, init=False)
    allowed_tools: list[str] = field(default_factory=list)
    strategy: dict[str, Any] = field(default_factory=dict)
    expected_file_changes: list[str] = field(default_factory=list)
    completion_standard: str = ""
    risk_tags: list[str] = field(default_factory=list)
    tags: list[str] = field(default_factory=list)
    profiles: dict[str, Any] = field(default_factory=dict)
    golden_contract: GoldenTaskContract | None = field(default=None, init=False)
    description: str = ""
    owner: str | None = None
    created_at: str | None = None
    updated_at: str | None = None
    schema_version: str = SCHEMA_VERSION

    def __init__(
        self,
        *,
        task_id: str,
        version: str,
        title: str = "",
        task_type: BenchmarkTaskKind | str = BenchmarkTaskKind.SINGULARITY_INTERNAL,
        visibility: BenchmarkVisibility | str = BenchmarkVisibility.PRIVATE,
        adapter: BenchmarkAdapterKind | str = BenchmarkAdapterKind.SINGULARITY_PRIVATE,
        input_prompt: str | None = None,
        input: TaskInput | dict[str, Any] | str | None = None,
        workspace_snapshot: WorkspaceSnapshot | dict[str, Any] | None = None,
        expected_outcomes: list[ExpectedOutcome | dict[str, Any]] | None = None,
        evaluation_hooks: list[EvaluationHook | dict[str, Any]] | None = None,
        allowed_tools: list[str] | None = None,
        strategy: dict[str, Any] | None = None,
        expected_file_changes: list[str] | None = None,
        completion_standard: str = "",
        risk_tags: list[str] | None = None,
        tags: list[str] | None = None,
        profiles: dict[str, Any] | None = None,
        golden_contract: GoldenTaskContract | dict[str, Any] | None = None,
        description: str = "",
        owner: str | None = None,
        created_at: str | None = None,
        updated_at: str | None = None,
        schema_version: str = SCHEMA_VERSION,
    ) -> None:
        resolved_input = input if input is not None else {"prompt": input_prompt or ""}
        object.__setattr__(self, "task_id", task_id)
        object.__setattr__(self, "version", version)
        object.__setattr__(self, "title", title)
        object.__setattr__(self, "task_type", _task_kind(task_type))
        object.__setattr__(self, "visibility", _visibility(visibility))
        object.__setattr__(self, "adapter", _adapter_kind(adapter))
        object.__setattr__(self, "input", _task_input(resolved_input))
        object.__setattr__(self, "workspace_snapshot", _snapshot(workspace_snapshot))
        object.__setattr__(
            self,
            "expected_outcomes",
            [_outcome(item) for item in (expected_outcomes or [])],
        )
        object.__setattr__(
            self,
            "evaluation_hooks",
            [_hook(item) for item in (evaluation_hooks or [])],
        )
        object.__setattr__(self, "allowed_tools", sorted(dict.fromkeys(allowed_tools or [])))
        object.__setattr__(self, "strategy", _dict(strategy))
        object.__setattr__(
            self,
            "expected_file_changes",
            sorted(dict.fromkeys(str(item) for item in (expected_file_changes or []))),
        )
        object.__setattr__(
            self,
            "completion_standard",
            str(completion_standard or ""),
        )
        object.__setattr__(
            self,
            "risk_tags",
            sorted(dict.fromkeys(str(item) for item in (risk_tags or []))),
        )
        object.__setattr__(self, "tags", list(tags or []))
        object.__setattr__(self, "profiles", _dict(profiles))
        object.__setattr__(self, "golden_contract", _golden_contract(golden_contract))
        object.__setattr__(self, "description", description)
        object.__setattr__(self, "owner", owner)
        object.__setattr__(self, "created_at", created_at)
        object.__setattr__(self, "updated_at", updated_at)
        object.__setattr__(self, "schema_version", schema_version)
        self._validate()

    def _validate(self) -> None:
        if self.schema_version != SCHEMA_VERSION:
            raise ValueError(
                f"Unsupported BenchmarkTask.schema_version: {self.schema_version}"
            )
        if not self.task_id.strip():
            raise ValueError("BenchmarkTask requires task_id.")
        if self.version not in VALID_TASK_VERSIONS:
            raise ValueError("BenchmarkTask.version must be v1 or v2.")
        if not self.expected_outcomes:
            raise ValueError("BenchmarkTask requires at least one expected_outcome.")
        difficulties = [tag for tag in self.tags if tag in VALID_DIFFICULTY_TAGS]
        if len(difficulties) != 1:
            raise ValueError("BenchmarkTask requires exactly one difficulty tag.")
        invalid = [tag for tag in self.tags if tag not in VALID_TAGS]
        if invalid:
            raise ValueError(f"Unsupported benchmark tag or difficulty tag: {', '.join(invalid)}")

    def with_updates(self, **updates: Any) -> BenchmarkTask:
        payload = self.to_dict()
        for key, value in updates.items():
            if key == "input_prompt":
                payload.setdefault("input", {})["prompt"] = value
            else:
                payload[key] = _to_jsonish(value)
        return BenchmarkTask.from_dict(payload)

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "schema_version": self.schema_version,
            "task_id": self.task_id,
            "version": self.version,
            "title": self.title,
            "task_type": self.task_type.value,
            "visibility": self.visibility.value,
            "adapter": self.adapter.value,
            "input": self.input.to_dict(),
            "workspace_snapshot": self.workspace_snapshot.to_dict(),
            "expected_outcomes": [item.to_dict() for item in self.expected_outcomes],
            "tags": list(self.tags),
        }
        if self.description:
            payload["description"] = self.description
        if self.evaluation_hooks:
            payload["evaluation_hooks"] = [item.to_dict() for item in self.evaluation_hooks]
        if self.allowed_tools:
            payload["allowed_tools"] = list(self.allowed_tools)
        if self.strategy:
            payload["strategy"] = _copy_jsonish(self.strategy)
        if self.expected_file_changes:
            payload["expected_file_changes"] = list(self.expected_file_changes)
        if self.completion_standard:
            payload["completion_standard"] = self.completion_standard
        if self.risk_tags:
            payload["risk_tags"] = list(self.risk_tags)
        if self.profiles:
            payload["profiles"] = _copy_jsonish(self.profiles)
        if self.golden_contract is not None:
            payload["golden_contract"] = self.golden_contract.to_dict()
        if self.owner:
            payload["owner"] = self.owner
        if self.created_at:
            payload["created_at"] = self.created_at
        if self.updated_at:
            payload["updated_at"] = self.updated_at
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> BenchmarkTask:
        return cls(
            task_id=str(payload.get("task_id", "")),
            version=str(payload.get("version", "")),
            title=str(payload.get("title", "")),
            task_type=str(payload.get("task_type", BenchmarkTaskKind.SINGULARITY_INTERNAL.value)),
            visibility=str(payload.get("visibility", BenchmarkVisibility.PRIVATE.value)),
            adapter=str(payload.get("adapter", BenchmarkAdapterKind.SINGULARITY_PRIVATE.value)),
            input=payload.get("input"),
            workspace_snapshot=payload.get("workspace_snapshot"),
            expected_outcomes=list(payload.get("expected_outcomes") or []),
            evaluation_hooks=list(payload.get("evaluation_hooks") or []),
            allowed_tools=[str(item) for item in payload.get("allowed_tools") or []],
            strategy=_dict(payload.get("strategy")),
            expected_file_changes=[
                str(item) for item in payload.get("expected_file_changes") or []
            ],
            completion_standard=str(payload.get("completion_standard", "")),
            risk_tags=[str(item) for item in payload.get("risk_tags") or []],
            tags=list(payload.get("tags") or []),
            profiles=_dict(payload.get("profiles")),
            golden_contract=payload.get("golden_contract"),
            description=str(payload.get("description", "")),
            owner=payload.get("owner"),
            created_at=payload.get("created_at"),
            updated_at=payload.get("updated_at"),
            schema_version=str(payload.get("schema_version", SCHEMA_VERSION)),
        )


@dataclass(frozen=True)
class EvaluationProfile:
    name: str
    model: str
    prompt_profile: str = "default"
    memory_enabled: bool = True
    allowed_tools: list[str] = field(default_factory=list)
    tool_policy: str = "read_write"
    temperature: float | None = None
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.name.strip():
            raise ValueError("EvaluationProfile.name is required.")
        if not self.model.strip():
            raise ValueError("EvaluationProfile.model is required.")
        object.__setattr__(self, "allowed_tools", sorted(dict.fromkeys(self.allowed_tools)))

    def to_dict(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "name": self.name,
            "model": self.model,
            "prompt_profile": self.prompt_profile,
            "memory_enabled": self.memory_enabled,
            "allowed_tools": list(self.allowed_tools),
            "tool_policy": self.tool_policy,
        }
        if self.temperature is not None:
            payload["temperature"] = self.temperature
        if self.metadata:
            payload["metadata"] = _copy_jsonish(self.metadata)
        return payload

    @classmethod
    def from_dict(cls, payload: dict[str, Any] | None) -> EvaluationProfile:
        payload = payload or {}
        return cls(
            name=str(payload.get("name", "baseline")),
            model=str(payload.get("model", "default")),
            prompt_profile=str(payload.get("prompt_profile", "default")),
            memory_enabled=bool(payload.get("memory_enabled", True)),
            allowed_tools=[str(item) for item in payload.get("allowed_tools") or []],
            tool_policy=str(payload.get("tool_policy", "read_write")),
            temperature=payload.get("temperature"),
            metadata=_dict(payload.get("metadata")),
        )

    def config_fingerprint_payload(self) -> dict[str, Any]:
        return self.to_dict()

    def to_agent_config_overrides(self) -> dict[str, Any]:
        return {
            "profile": self.name,
            "model": self.model,
            "prompt_profile": self.prompt_profile,
            "memory_enabled": self.memory_enabled,
            "allowed_tools": list(self.allowed_tools),
            "tool_policy": self.tool_policy,
        }


@dataclass(frozen=True)
class ScoringResult:
    task_id: str
    status: str
    score: float
    subscores: dict[str, float]
    evidence: list[dict[str, Any]] = field(default_factory=list)
    failure_reasons: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "task_id": self.task_id,
            "status": self.status,
            "score": self.score,
            "subscores": dict(sorted(self.subscores.items())),
            "evidence": _copy_jsonish(self.evidence),
            "failure_reasons": list(self.failure_reasons),
        }


@dataclass(frozen=True)
class PatchQualityResult:
    score: float
    metrics: dict[str, Any]
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "score": self.score,
            "metrics": _copy_jsonish(self.metrics),
            "warnings": list(self.warnings),
        }


@dataclass(frozen=True)
class FailureCaseRecord:
    task_id: str
    status: str
    failure_category: str
    miscompletion_count: int
    public_verification_passed: bool
    hidden_verification_passed: bool
    policy_blocks: int
    expected_file_changes: list[str]
    files_changed: list[str]
    final_report_status: str
    repair_attempt_count: int
    repair_execution_count: int
    blocked_reason: str = ""
    trace_path: str = ""
    trace_artifact_refs: list[str] = field(default_factory=list)
    contract_satisfaction: dict[str, Any] = field(default_factory=dict)
    repair_telemetry: dict[str, Any] = field(default_factory=dict)
    verification: dict[str, Any] = field(default_factory=dict)
    trace_summary: dict[str, Any] = field(default_factory=dict)
    source_report_path: str = ""
    source_regression_path: str = ""
    schema_version: str = FAILURE_CASE_RECORD_SCHEMA_VERSION

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "task_id": self.task_id,
            "status": self.status,
            "failure_category": self.failure_category,
            "miscompletion_count": self.miscompletion_count,
            "public_verification_passed": self.public_verification_passed,
            "hidden_verification_passed": self.hidden_verification_passed,
            "policy_blocks": self.policy_blocks,
            "expected_file_changes": list(self.expected_file_changes),
            "files_changed": list(self.files_changed),
            "final_report_status": self.final_report_status,
            "repair_attempt_count": self.repair_attempt_count,
            "repair_execution_count": self.repair_execution_count,
            "blocked_reason": self.blocked_reason,
            "trace_path": self.trace_path,
            "trace_artifact_refs": list(self.trace_artifact_refs),
            "contract_satisfaction": _copy_jsonish(self.contract_satisfaction),
            "repair_telemetry": _copy_jsonish(self.repair_telemetry),
            "verification": _copy_jsonish(self.verification),
            "trace_summary": _copy_jsonish(self.trace_summary),
            "source_report_path": self.source_report_path,
            "source_regression_path": self.source_regression_path,
        }

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> FailureCaseRecord:
        return cls(
            task_id=str(payload.get("task_id") or ""),
            status=str(payload.get("status") or ""),
            failure_category=str(payload.get("failure_category") or ""),
            miscompletion_count=int(payload.get("miscompletion_count") or 0),
            public_verification_passed=bool(payload.get("public_verification_passed")),
            hidden_verification_passed=bool(payload.get("hidden_verification_passed")),
            policy_blocks=int(payload.get("policy_blocks") or 0),
            expected_file_changes=[str(item) for item in payload.get("expected_file_changes") or []],
            files_changed=[str(item) for item in payload.get("files_changed") or []],
            final_report_status=str(payload.get("final_report_status") or ""),
            repair_attempt_count=int(payload.get("repair_attempt_count") or 0),
            repair_execution_count=int(payload.get("repair_execution_count") or 0),
            blocked_reason=str(payload.get("blocked_reason") or ""),
            trace_path=str(payload.get("trace_path") or ""),
            trace_artifact_refs=[str(item) for item in payload.get("trace_artifact_refs") or []],
            contract_satisfaction=dict(payload.get("contract_satisfaction") or {}),
            repair_telemetry=dict(payload.get("repair_telemetry") or {}),
            verification=dict(payload.get("verification") or {}),
            trace_summary=dict(payload.get("trace_summary") or {}),
            source_report_path=str(payload.get("source_report_path") or ""),
            source_regression_path=str(payload.get("source_regression_path") or ""),
            schema_version=str(payload.get("schema_version") or FAILURE_CASE_RECORD_SCHEMA_VERSION),
        )


@dataclass(frozen=True)
class TraceReplayResult:
    trace_run_dir: Path
    profile: EvaluationProfile
    deterministic: bool
    replay_classification: str
    metrics: dict[str, Any]
    verification: dict[str, Any]
    events_replayed: int
    side_effects_simulated: int
    config_fingerprint: str
    trace_input_digest: str
    result_hash: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "trace_run_dir": str(self.trace_run_dir),
            "profile": self.profile.to_dict(),
            "deterministic": self.deterministic,
            "replay_classification": self.replay_classification,
            "metrics": _copy_jsonish(self.metrics),
            "verification": _copy_jsonish(self.verification),
            "events_replayed": self.events_replayed,
            "side_effects_simulated": self.side_effects_simulated,
            "config_fingerprint": self.config_fingerprint,
            "trace_input_digest": self.trace_input_digest,
            "result_hash": self.result_hash,
        }


def now_iso() -> str:
    return datetime.now(UTC).isoformat()


def canonical_json(payload: Any) -> str:
    return json.dumps(_to_jsonish(payload), ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _task_input(value: TaskInput | dict[str, Any] | str | None) -> TaskInput:
    if isinstance(value, TaskInput):
        return value
    return TaskInput.from_dict(value or {"prompt": ""})


def _snapshot(value: WorkspaceSnapshot | dict[str, Any] | None) -> WorkspaceSnapshot:
    if isinstance(value, WorkspaceSnapshot):
        return value
    return WorkspaceSnapshot.from_dict(value or {})


def _task_kind(value: BenchmarkTaskKind | str) -> BenchmarkTaskKind:
    if isinstance(value, BenchmarkTaskKind):
        return value
    try:
        return BenchmarkTaskKind(str(value))
    except ValueError as exc:
        raise ValueError(f"Unsupported BenchmarkTask.task_type: {value}") from exc


def _visibility(value: BenchmarkVisibility | str) -> BenchmarkVisibility:
    if isinstance(value, BenchmarkVisibility):
        return value
    try:
        return BenchmarkVisibility(str(value))
    except ValueError as exc:
        raise ValueError(f"Unsupported BenchmarkTask.visibility: {value}") from exc


def _adapter_kind(value: BenchmarkAdapterKind | str) -> BenchmarkAdapterKind:
    if isinstance(value, BenchmarkAdapterKind):
        return value
    try:
        return BenchmarkAdapterKind(str(value))
    except ValueError as exc:
        raise ValueError(f"Unsupported BenchmarkTask.adapter: {value}") from exc


def _outcome(value: ExpectedOutcome | dict[str, Any]) -> ExpectedOutcome:
    if isinstance(value, ExpectedOutcome):
        return value
    return ExpectedOutcome.from_dict(value)


def _hook(value: EvaluationHook | dict[str, Any]) -> EvaluationHook:
    if isinstance(value, EvaluationHook):
        return value
    return EvaluationHook.from_dict(value)


def _golden_contract(
    value: GoldenTaskContract | dict[str, Any] | None,
) -> GoldenTaskContract | None:
    if value is None:
        return None
    if isinstance(value, GoldenTaskContract):
        return value
    return GoldenTaskContract.from_dict(value)


def _enum(enum_type: type[EnumT], value: EnumT | str) -> EnumT:
    return cast(EnumT, coerce_enum(enum_type, value))


def _dict(value: Any) -> dict[str, Any]:
    return coerce_dict(
        value,
        allow_none=True,
        error_message="Expected object mapping.",
    )


def _copy_jsonish(value: Any) -> Any:
    return copy.deepcopy(_to_jsonish(value))


def _to_jsonish(value: Any) -> Any:
    if hasattr(value, "to_dict"):
        return value.to_dict()
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, Path):
        return str(value)
    if isinstance(value, dict):
        return {str(key): _to_jsonish(val) for key, val in value.items()}
    if isinstance(value, list | tuple):
        return [_to_jsonish(item) for item in value]
    return value
