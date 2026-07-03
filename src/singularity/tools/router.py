from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from singularity.tools.models import PermissionLevel, ToolSpec

LOW_LEVEL_INTERNAL_TOOLS = {
    "workspace_replace_text",
    "workspace_create_file",
    "workspace_delete_file",
    "workspace_move_file",
}
COMMAND_EXECUTOR_TOOLS = {
    "run_command",
    "start_process",
    "read_process_output",
    "stop_process",
    "list_processes",
}
WRITE_TOOL_NAMES = {
    "write_file",
    "apply_patch",
    "edit_apply",
    *LOW_LEVEL_INTERNAL_TOOLS,
}


@dataclass(frozen=True)
class ToolExposureRecord:
    name: str
    reason_code: str
    risk_category: str
    phase: str
    stage_basis: str
    factors: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "reason_code": self.reason_code,
            "risk_category": self.risk_category,
            "phase": self.phase,
            "stage_basis": self.stage_basis,
            "factors": self.factors,
        }


@dataclass(frozen=True)
class ToolExposureDecision:
    phase: str
    selected_tool_names: list[str]
    suppressed_tools: list[ToolExposureRecord] = field(default_factory=list)
    deferred_tools: list[ToolExposureRecord] = field(default_factory=list)
    blocked_tools: list[ToolExposureRecord] = field(default_factory=list)
    factors: dict[str, Any] = field(default_factory=dict)

    def to_trace_data(self) -> dict[str, Any]:
        return {
            "phase": self.phase,
            "selected_tools": list(self.selected_tool_names),
            "suppressed_tools": [item.name for item in self.suppressed_tools],
            "deferred_tools": [item.name for item in self.deferred_tools],
            "blocked_tools": [item.name for item in self.blocked_tools],
            "suppressed": [item.to_dict() for item in self.suppressed_tools],
            "deferred": [item.to_dict() for item in self.deferred_tools],
            "blocked": [item.to_dict() for item in self.blocked_tools],
            "factors": self.factors,
        }


class ToolRouter:
    def decide(
        self,
        *,
        phase: str,
        phase_allowed_tool_names: set[str],
        available_tools: list[ToolSpec],
        task_state: Any | None = None,
        policy_profile: str | None = None,
        sandbox_mode: str | None = None,
        active_user_constraints: list[str] | None = None,
        workspace_state: dict[str, Any] | None = None,
    ) -> ToolExposureDecision:
        constraints = list(active_user_constraints or [])
        factors = {
            "policy_profile": policy_profile,
            "sandbox_mode": sandbox_mode,
            "active_user_constraints": constraints,
            "workspace_state_keys": sorted((workspace_state or {}).keys()),
        }
        selected: list[str] = []
        suppressed: list[ToolExposureRecord] = []
        deferred: list[ToolExposureRecord] = []
        blocked: list[ToolExposureRecord] = []

        for spec in sorted(available_tools, key=lambda item: item.name):
            record = self._classify(
                spec,
                phase=phase,
                phase_allowed_tool_names=phase_allowed_tool_names,
                task_state=task_state,
                constraints=constraints,
                workspace_state=workspace_state or {},
                policy_profile=policy_profile,
                sandbox_mode=sandbox_mode,
            )
            if record is None:
                selected.append(spec.name)
            elif (
                record.reason_code == "phase_not_allowed"
                or record.reason_code.startswith("blocked_")
                or record.reason_code.startswith("user_")
            ):
                blocked.append(record)
            elif record.reason_code.endswith("_indirect") or record.reason_code == "low_level_internal_capability":
                deferred.append(record)
            else:
                suppressed.append(record)

        return ToolExposureDecision(
            phase=phase,
            selected_tool_names=selected,
            suppressed_tools=suppressed,
            deferred_tools=deferred,
            blocked_tools=blocked,
            factors=factors,
        )

    def _classify(
        self,
        spec: ToolSpec,
        *,
        phase: str,
        phase_allowed_tool_names: set[str],
        task_state: Any | None,
        constraints: list[str],
        workspace_state: dict[str, Any],
        policy_profile: str | None,
        sandbox_mode: str | None,
    ) -> ToolExposureRecord | None:
        del task_state
        risk_category = _risk_category(spec)
        factors = {
            "policy_profile": policy_profile,
            "sandbox_mode": sandbox_mode,
        }
        if not spec.enabled:
            return _record(spec, "tool_disabled", risk_category, phase, "registry", factors)
        if _sandbox_blocks(spec, sandbox_mode):
            return _record(spec, "blocked_by_sandbox_mode", risk_category, phase, "sandbox", factors)
        if _constraint_blocks_write(spec, constraints, workspace_state):
            return _record(
                spec,
                "user_constraint_blocks_write_path",
                "user_constraint",
                phase,
                "active_user_constraints",
                {**factors, "constraints": constraints, "target_paths": _target_paths(workspace_state)},
            )
        if spec.name in LOW_LEVEL_INTERNAL_TOOLS:
            return _record(
                spec,
                "low_level_internal_capability",
                risk_category,
                phase,
                "internal_tool_layer",
                factors,
            )
        if spec.name in COMMAND_EXECUTOR_TOOLS:
            return _record(
                spec,
                "command_executor_indirect",
                risk_category,
                phase,
                "executor_indirection",
                factors,
            )
        if spec.name not in phase_allowed_tool_names:
            return _record(spec, "phase_not_allowed", risk_category, phase, "planner_phase", factors)
        return None


def _record(
    spec: ToolSpec,
    reason_code: str,
    risk_category: str,
    phase: str,
    stage_basis: str,
    factors: dict[str, Any],
) -> ToolExposureRecord:
    return ToolExposureRecord(
        name=spec.name,
        reason_code=reason_code,
        risk_category=risk_category,
        phase=phase,
        stage_basis=stage_basis,
        factors=factors,
    )


def _risk_category(spec: ToolSpec) -> str:
    if spec.permission_level == PermissionLevel.WRITE:
        return "mutation"
    if spec.permission_level == PermissionLevel.SHELL:
        return "verification" if "verification" in spec.name else "command"
    return "read_only"


def _sandbox_blocks(spec: ToolSpec, sandbox_mode: str | None) -> bool:
    return sandbox_mode in {"read_only", "no_shell"} and spec.permission_level == PermissionLevel.SHELL


def _constraint_blocks_write(
    spec: ToolSpec,
    constraints: list[str],
    workspace_state: dict[str, Any],
) -> bool:
    targets = _target_paths(workspace_state)
    if not targets:
        return False
    return write_blocked_by_user_constraint(spec, constraints, targets)


def write_blocked_by_user_constraint(
    spec: ToolSpec,
    constraints: list[str],
    target_paths: list[str],
) -> bool:
    if spec.name not in WRITE_TOOL_NAMES:
        return False
    blocked_prefixes = _blocked_write_prefixes(constraints)
    if not blocked_prefixes or not target_paths:
        return False
    return any(_is_under_prefix(path, prefix) for path in target_paths for prefix in blocked_prefixes)


def target_paths_from_tool_arguments(tool_name: str, arguments: dict[str, Any]) -> list[str]:
    paths: list[str] = []

    def add(value: Any) -> None:
        if value is None:
            return
        text = str(value).replace("\\", "/").strip()
        if not text or text == "/dev/null":
            return
        text = _strip_diff_prefix(text)
        if text not in paths:
            paths.append(text)

    for key in ("path", "new_path"):
        add(arguments.get(key))
    for key in ("paths", "expected_files", "changed_files"):
        value = arguments.get(key)
        if isinstance(value, list):
            for item in value:
                add(item)
    scope = arguments.get("scope")
    if isinstance(scope, dict):
        value = scope.get("paths")
        if isinstance(value, list):
            for item in value:
                add(item)
    operations = arguments.get("operations")
    if isinstance(operations, list):
        for operation in operations:
            if isinstance(operation, dict):
                add(operation.get("path"))
                add(operation.get("new_path"))
    if tool_name == "apply_patch":
        _add_patch_paths(arguments.get("patch") or arguments.get("unified_diff"), add)
    return paths


def _blocked_write_prefixes(constraints: list[str]) -> list[str]:
    prefixes: list[str] = []
    for constraint in constraints:
        lowered = constraint.lower()
        if not any(marker in lowered for marker in ("do not", "don't", "不要", "不得", "禁止")):
            continue
        if not any(marker in lowered for marker in ("modify", "change", "edit", "write", "修改", "改动", "写", "编辑")):
            continue
        if "tests/" in lowered or "tests\\" in lowered or "tests" in lowered:
            prefixes.append("tests")
    return prefixes


def _target_paths(workspace_state: dict[str, Any]) -> list[str]:
    for key in ("target_paths", "changed_files", "candidate_paths", "paths"):
        value = workspace_state.get(key)
        if isinstance(value, list):
            return [str(item) for item in value]
    return []


def _add_patch_paths(patch: Any, add: Any) -> None:
    if not isinstance(patch, str):
        return
    for line in patch.splitlines():
        if line.startswith(("--- ", "+++ ")):
            add(line[4:].split("\t", 1)[0].strip())
        elif line.startswith("diff --git "):
            parts = line.split()
            if len(parts) >= 4:
                add(parts[2])
                add(parts[3])


def _strip_diff_prefix(path: str) -> str:
    if path.startswith(("a/", "b/")):
        return path[2:]
    return path


def _is_under_prefix(path: str, prefix: str) -> bool:
    normalized = path.replace("\\", "/").strip("/")
    return normalized == prefix or normalized.startswith(prefix.rstrip("/") + "/")
