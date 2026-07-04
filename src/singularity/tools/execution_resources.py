from __future__ import annotations

from pathlib import Path
from typing import Any

from singularity.policy import ResourceRef
from singularity.policy.audit import redact_resource_identifier
from singularity.tools.models import PermissionLevel, ToolSideEffectKind, ToolSpec


def resources_for(spec: ToolSpec, args: dict[str, Any], workspace_root: Path) -> list[ResourceRef]:
    if spec.resource_resolver is not None:
        return spec.resource_resolver(args, workspace_root)
    return [default_resource(spec, args)]


def touched_paths(spec: ToolSpec, args: dict[str, Any], workspace_root: Path) -> tuple[str, ...]:
    paths: list[str] = []
    for resource in resources_for(spec, args, workspace_root):
        if resource.resource_type in {"file", "directory"} and resource.workspace_relative:
            paths.append(Path(resource.identifier).as_posix())
    return tuple(sorted(set(paths)))


def redacted_resource_details(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        return []
    resources: list[dict[str, Any]] = []
    for item in value:
        if not isinstance(item, dict):
            continue
        identifier = item.get("identifier")
        normalized_identifier = item.get("normalized_identifier")
        resources.append(
            {
                **item,
                "identifier": redact_resource_identifier(str(identifier))
                if identifier is not None
                else "",
                "normalized_identifier": (
                    redact_resource_identifier(str(normalized_identifier))
                    if normalized_identifier is not None
                    else None
                ),
            }
        )
    return resources


def default_resource(spec: ToolSpec, args: dict[str, Any]) -> ResourceRef:
    name = spec.name
    if name in {"edit_plan", "edit_preview", "edit_apply"}:
        operations = args.get("operations") or []
        if isinstance(operations, list):
            for operation in operations:
                if isinstance(operation, dict) and operation.get("path"):
                    return ResourceRef("file", str(operation.get("path")), workspace_relative=True)
        return ResourceRef("workspace", "edit", workspace_relative=True)
    if name in {"read_file", "workspace_create_file", "workspace_delete_file", "workspace_replace_text"}:
        return ResourceRef("file", str(args.get("path") or "."), workspace_relative=True)
    if name == "workspace_move_file":
        return ResourceRef("file", str(args.get("path") or "."), workspace_relative=True)
    if name in {"list_files", "search_text"}:
        return ResourceRef("directory", str(args.get("path") or "."), workspace_relative=True)
    if name == "start_process":
        return ResourceRef("command", command_identifier(args))
    if name == "stop_process":
        return ResourceRef("process", str(args.get("process_id") or ""))
    if name == "run_command":
        return ResourceRef("command", command_identifier(args))
    if name in {"plan_verification", "get_verification_result"}:
        return ResourceRef("workspace", name, workspace_relative=True)
    if name in {"run_verification", "rerun_check"}:
        return ResourceRef("workspace", name, workspace_relative=True)
    if spec.permission_level == PermissionLevel.SHELL:
        return ResourceRef("command", command_identifier(args) or name)
    return ResourceRef("tool", name)


def command_identifier(args: dict[str, Any]) -> str:
    if args.get("shell"):
        return str(args["shell"])
    if args.get("argv"):
        return " ".join(str(part) for part in args["argv"])
    return ""


def is_read_only_side_effect(side_effect: ToolSideEffectKind | None) -> bool:
    return side_effect in {ToolSideEffectKind.NONE, ToolSideEffectKind.READ_WORKSPACE}
