import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.context import ContextManager
from singularity.instructions import InstructionRuntime
from singularity.model import MockModelProvider, ModelPurpose, ModelRuntime
from singularity.planner import PlannerRuntime, TaskStatus
from singularity.tools import ToolRegistry
from singularity.tools.code_index import register_code_index_tools
from singularity.tools.command import register_command_tools
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.tools.workspace_state import register_workspace_state_tools
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolSpec,
)
from singularity.trace import TraceWriter


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def _spec(
    name: str,
    *,
    permission: PermissionLevel = PermissionLevel.READ_ONLY,
    backend: ToolExecutionBackendKind = ToolExecutionBackendKind.IN_PROCESS,
) -> ToolSpec:
    return ToolSpec(
        name=name,
        version="test",
        description=name,
        input_model=EmptyInput,
        handler=lambda _args: {},
        permission_level=permission,
        execution_backend=backend,
        uses_edit_runtime=backend == ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
        uses_mutation_runtime=permission == PermissionLevel.WRITE,
        uses_command_runtime=permission == PermissionLevel.SHELL,
        delegates_policy_constraints=backend == ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME,
    )


def _tool_specs() -> list[ToolSpec]:
    return [
        _spec("list_files"),
        _spec("read_file"),
        _spec("search_text"),
        _spec("index_symbols"),
        _spec("workspace_health"),
        _spec("edit_plan"),
        _spec("edit_preview"),
        _spec(
            "edit_apply",
            permission=PermissionLevel.WRITE,
            backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
        ),
        _spec(
            "apply_patch",
            permission=PermissionLevel.WRITE,
            backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
        ),
        _spec(
            "write_file",
            permission=PermissionLevel.WRITE,
            backend=ToolExecutionBackendKind.DELEGATED_EDIT_RUNTIME,
        ),
        _spec("inspect_diff"),
        _spec(
            "workspace_replace_text",
            permission=PermissionLevel.WRITE,
            backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
        ),
        _spec(
            "run_command",
            permission=PermissionLevel.SHELL,
            backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
        ),
        _spec("plan_verification"),
        _spec(
            "run_verification",
            permission=PermissionLevel.SHELL,
            backend=ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNTIME,
        ),
        _spec("get_verification_result"),
    ]


def _planner(tmp_path: Path, phase: str, *, trace: TraceWriter | None = None) -> PlannerRuntime:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1", trace=trace)
    planner.start_task("Change code")
    planner.state.status = TaskStatus(phase)
    planner.state.current_phase = phase
    return planner


def _production_registry(tmp_path: Path) -> ToolRegistry:
    registry = ToolRegistry(tmp_path)
    register_mutation_tools(registry)
    register_edit_tools(registry)
    register_command_tools(registry)
    register_workspace_state_tools(registry)
    register_code_index_tools(registry)
    register_verification_tools(registry)
    return registry


def test_tool_router_selects_minimal_tools_by_phase(tmp_path: Path) -> None:
    inspection = _planner(tmp_path, "inspecting_workspace")
    edit = _planner(tmp_path, "applying_changes")
    verification = _planner(tmp_path, "running_verification")

    inspection_decision = inspection.decide_tool_exposure(_tool_specs())
    edit_decision = edit.decide_tool_exposure(_tool_specs())
    verification_decision = verification.decide_tool_exposure(_tool_specs())

    assert set(inspection_decision.selected_tool_names) == {
        "index_symbols",
        "list_files",
        "read_file",
        "search_text",
        "workspace_health",
    }
    assert {"edit_apply", "apply_patch", "write_file", "inspect_diff"} <= set(
        edit_decision.selected_tool_names
    )
    assert {"plan_verification", "run_verification", "get_verification_result"} <= set(
        verification_decision.selected_tool_names
    )
    assert "workspace_replace_text" not in edit_decision.selected_tool_names
    assert "run_command" not in verification_decision.selected_tool_names
    assert any(item.reason_code == "low_level_internal_capability" for item in edit_decision.deferred_tools)
    assert any(item.reason_code == "command_runtime_indirect" for item in verification_decision.deferred_tools)


def test_registered_tool_pool_has_sufficient_facades_and_routes_internals_down(tmp_path: Path) -> None:
    registry = _production_registry(tmp_path)
    available = {spec.name for spec in registry.list()}
    expected_facades = {
        "list_files",
        "read_file",
        "search_text",
        "index_relevant",
        "index_symbols",
        "index_explain",
        "index_impact",
        "index_tests",
        "edit_plan",
        "edit_preview",
        "edit_apply",
        "write_file",
        "apply_patch",
        "inspect_diff",
        "plan_verification",
        "run_verification",
        "get_verification_result",
        "rerun_check",
        "workspace_health",
    }
    internal_capabilities = {
        "workspace_replace_text",
        "workspace_create_file",
        "workspace_delete_file",
        "workspace_move_file",
        "run_command",
        "start_process",
        "read_process_output",
        "stop_process",
        "list_processes",
    }

    assert expected_facades <= available
    assert internal_capabilities <= available

    inspection = _planner(tmp_path, "inspecting_workspace").decide_tool_exposure(registry.list())
    planning = _planner(tmp_path, "planning_changes").decide_tool_exposure(registry.list())
    edit = _planner(tmp_path, "applying_changes").decide_tool_exposure(registry.list())
    verification = _planner(tmp_path, "running_verification").decide_tool_exposure(registry.list())

    assert {"list_files", "read_file", "search_text", "workspace_health"} <= set(
        inspection.selected_tool_names
    )
    assert {"edit_plan", "edit_preview"} <= set(planning.selected_tool_names)
    assert {"edit_apply", "write_file", "apply_patch", "inspect_diff"} <= set(
        edit.selected_tool_names
    )
    assert {"plan_verification", "run_verification", "get_verification_result"} <= set(
        verification.selected_tool_names
    )
    for decision in (inspection, planning, edit, verification):
        assert not internal_capabilities.intersection(decision.selected_tool_names)


def test_tool_router_blocks_write_tools_for_active_tests_write_constraint(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)
    planner = _planner(tmp_path, "applying_changes", trace=trace)
    assert planner.state is not None
    planner.state.constraints.append("不要修改 tests/")

    decision = planner.decide_tool_exposure(
        _tool_specs(),
        workspace_state={"target_paths": ["tests/test_sample.py"]},
    )

    assert "apply_patch" not in decision.selected_tool_names
    assert "write_file" not in decision.selected_tool_names
    assert {item.name for item in decision.blocked_tools} >= {"apply_patch", "write_file"}
    assert all(item.reason_code == "user_constraint_blocks_write_path" for item in decision.blocked_tools)

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    exposure = [event for event in events if event["event"] == "tool.exposure_decided"][-1]
    assert exposure["data"]["phase"] == "applying_changes"
    assert "apply_patch" in exposure["data"]["blocked_tools"]
    assert exposure["data"]["blocked"][0]["reason_code"] == "user_constraint_blocks_write_path"
    assert exposure["data"]["factors"]["active_user_constraints"] == ["不要修改 tests/"]


def test_tool_router_keeps_write_tools_visible_until_target_path_is_known(tmp_path: Path) -> None:
    planner = _planner(tmp_path, "applying_changes")
    assert planner.state is not None
    planner.state.constraints.append("不要修改 tests/")

    decision = planner.decide_tool_exposure(_tool_specs())

    assert "apply_patch" in decision.selected_tool_names
    assert "write_file" in decision.selected_tool_names
    assert "edit_apply" in decision.selected_tool_names


def test_tool_router_does_not_treat_no_test_execution_as_write_path_block(tmp_path: Path) -> None:
    planner = _planner(tmp_path, "applying_changes")
    assert planner.state is not None
    planner.state.constraints.append("不要运行 tests")

    decision = planner.decide_tool_exposure(
        _tool_specs(),
        workspace_state={"target_paths": ["src/app.py"]},
    )

    assert "apply_patch" in decision.selected_tool_names
    assert "write_file" in decision.selected_tool_names


def test_model_request_contains_only_selected_tool_schemas_not_router_internals(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    for tool_spec in _tool_specs():
        registry.register(tool_spec)
    planner = _planner(tmp_path, "running_verification")
    decision = planner.decide_tool_exposure(registry.list())
    runtime = ModelRuntime.with_mock_provider(MockModelProvider(text="ok"), tool_registry=registry)
    context = ContextManager(system_prompt="system", user_goal="verify")
    request = runtime.build_request_from_context(
        context,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="running_verification",
        action_id="turn_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        allowed_tool_names=decision.selected_tool_names,
        instruction_runtime=InstructionRuntime(workspace_root=tmp_path),
        user_task="verify",
    )

    payload = request.to_dict()
    visible_names = {tool["name"] for tool in payload["tools"]}
    serialized = json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    assert visible_names == set(decision.selected_tool_names)
    assert "run_command" not in visible_names
    assert "workspace_replace_text" not in visible_names
    assert "reason_code" not in serialized
    assert "hidden_tools" not in serialized
    assert "suppressed_tools" not in serialized
    assert "blocked_tools" not in serialized
