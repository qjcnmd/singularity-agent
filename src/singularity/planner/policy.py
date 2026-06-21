from __future__ import annotations

from typing import Any

from singularity.planner.models import ActionKind, TaskPhase
from singularity.tools.models import PermissionLevel, ToolSpec


INDEX_TOOLS = {"index_relevant", "index_symbols", "index_explain", "index_impact", "index_tests"}
READ_TOOLS = {"list_files", "read_file", "search_text", "workspace_health", *INDEX_TOOLS}
LOW_LEVEL_MUTATION_TOOLS = {
    "workspace_replace_text",
    "workspace_create_file",
    "workspace_delete_file",
    "workspace_move_file",
}
EDIT_PLAN_TOOLS = {"edit_plan", "edit_preview"}
MUTATION_TOOLS = {"edit_apply"}
EDIT_TOOLS = {*EDIT_PLAN_TOOLS, *MUTATION_TOOLS}
VERIFICATION_TOOLS = {
    "plan_verification",
    "run_verification",
    "get_verification_result",
    "rerun_check",
}
COMMAND_TOOLS = {
    "run_command",
    "start_process",
    "read_process_output",
    "stop_process",
    "list_processes",
}


class PlannerPolicy:
    def action_for_tool(self, tool_name: str) -> ActionKind:
        if tool_name == "list_files":
            return ActionKind.INSPECT_WORKSPACE
        if tool_name == "read_file":
            return ActionKind.READ_RELEVANT_FILES
        if tool_name == "search_text":
            return ActionKind.SEARCH_CODE
        if tool_name == "workspace_health":
            return ActionKind.ANALYZE_ISSUE
        if tool_name in INDEX_TOOLS:
            return ActionKind.INSPECT_WORKSPACE
        if tool_name in EDIT_PLAN_TOOLS:
            return ActionKind.PROPOSE_CHANGE_SET
        if tool_name in MUTATION_TOOLS or tool_name in LOW_LEVEL_MUTATION_TOOLS:
            return ActionKind.APPLY_MUTATION
        if tool_name in VERIFICATION_TOOLS:
            return ActionKind.RUN_VERIFICATION
        if tool_name in COMMAND_TOOLS:
            return ActionKind.ANALYZE_ISSUE
        return ActionKind.ANALYZE_ISSUE

    def is_allowed(self, *, phase: TaskPhase, tool_name: str, spec: ToolSpec) -> bool:
        if tool_name not in phase.allowed_tools:
            return False
        if spec.permission_level == PermissionLevel.WRITE and tool_name not in MUTATION_TOOLS:
            return False
        if tool_name in MUTATION_TOOLS and not spec.uses_mutation_runtime:
            return False
        if tool_name in VERIFICATION_TOOLS and spec.permission_level == PermissionLevel.SHELL and not spec.uses_command_runtime:
            return False
        return self.action_for_tool(tool_name) in phase.allowed_actions

    @staticmethod
    def expected_evidence(tool_name: str) -> list[str]:
        if tool_name == "read_file":
            return ["inspected_files"]
        if tool_name == "search_text":
            return ["search_results"]
        if tool_name in INDEX_TOOLS:
            return ["project_index"]
        if tool_name in EDIT_PLAN_TOOLS:
            return ["edit_plan"]
        if tool_name in MUTATION_TOOLS:
            return ["edit_result", "applied_changes"]
        if tool_name in VERIFICATION_TOOLS:
            return ["verification_results"]
        if tool_name in COMMAND_TOOLS:
            return ["command_results"]
        return ["tool_results"]

    @staticmethod
    def normalize_arguments(arguments: Any) -> dict[str, Any]:
        return arguments if isinstance(arguments, dict) else {}
