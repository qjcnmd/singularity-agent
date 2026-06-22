from __future__ import annotations

from io import StringIO
from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.agent import SingularityAgent
from singularity.instructions import InstructionRuntime
from singularity.model import ModelRuntime
from singularity.planner import PlannerRuntime
from singularity.tool_protocol.runtime import ToolCallingProtocolRuntime
from singularity.tools import ToolPolicy, ToolRegistry, ToolRuntime
from singularity.trace import TraceWriter
from singularity.workspace_state import LocalWorkspaceStateRuntime
from tests.tool_runtime_helpers import make_test_policy_runtime


def make_agent_session(
    workspace_root: Path,
    *,
    provider: Any | None = None,
    model_runtime: ModelRuntime | None = None,
    tools: ToolRegistry | None = None,
    trace: Any | None = None,
    console: Console | None = None,
    max_turns: int = 3,
    state_runtime: LocalWorkspaceStateRuntime | None = None,
    planner: PlannerRuntime | None = None,
    policy_runtime: Any | None = None,
    tool_runtime: ToolRuntime | None = None,
    protocol_runtime: Any | None = None,
    instruction_runtime: InstructionRuntime | None = None,
    context_manager: ContextManager | None = None,
    context_db_path: Path | None = None,
    strict: bool = False,
) -> SingularityAgent:
    registry = tools or ToolRegistry(workspace_root)
    resolved_trace = trace or TraceWriter.create(workspace_root)
    resolved_policy = policy_runtime or make_test_policy_runtime(workspace_root)
    resolved_planner = planner or PlannerRuntime(
        workspace_root,
        session_id=getattr(resolved_trace, "run_id", "session_1"),
        task_id=getattr(resolved_trace, "run_id", "task_1"),
        trace=resolved_trace,
    )
    resolved_model_runtime = model_runtime or ModelRuntime.from_chat_provider(
        provider,
        tool_registry=registry,
        trace=resolved_trace,
    )
    resolved_tool_runtime = tool_runtime or ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=resolved_trace,
        workspace_root=workspace_root,
        planner=resolved_planner,
        policy_runtime=resolved_policy,
    )
    hook = _workspace_state_context_hook(state_runtime) if state_runtime is not None else None
    resolved_protocol_runtime = protocol_runtime or ToolCallingProtocolRuntime(
        registry=registry,
        trace=resolved_trace,
        workspace_state_hook=hook,
    )
    if hook is not None and getattr(resolved_protocol_runtime, "workspace_state_hook", None) is None:
        resolved_protocol_runtime.workspace_state_hook = hook
    resolved_instruction_runtime = instruction_runtime or InstructionRuntime(
        workspace_root=workspace_root,
        trace=resolved_trace,
    )
    return SingularityAgent(
        provider=provider,
        model_runtime=resolved_model_runtime,
        tools=registry,
        trace=resolved_trace,
        console=console or Console(file=StringIO(), force_terminal=False),
        max_turns=max_turns,
        planner=resolved_planner,
        policy_runtime=resolved_policy,
        tool_runtime=resolved_tool_runtime,
        protocol_runtime=resolved_protocol_runtime,
        instruction_runtime=resolved_instruction_runtime,
        context_manager=context_manager,
        context_db_path=context_db_path,
        strict=strict,
    )


def _workspace_state_context_hook(state_runtime: LocalWorkspaceStateRuntime):
    def hook(context, *, batch, tool_call_id: str | None) -> None:
        _ = batch, tool_call_id
        state_runtime.record_external_changes()
        context.add_workspace_state(state_runtime.get_workspace_health().to_observation())

    return hook
