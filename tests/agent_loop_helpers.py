from __future__ import annotations

from io import StringIO
from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.agent_loop import AgentLoop
from singularity.context import ContextManager
from singularity.instructions import PromptAssemblyPipeline
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.model import ModelRunner
from singularity.planner import Planner
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.workspace_state import WorkspaceStateManager
from tests.tool_executor_helpers import make_test_policy_engine


def make_agent_session(
    workspace_root: Path,
    *,
    provider: Any | None = None,
    model_runner: ModelRunner | None = None,
    tools: ToolRegistry | None = None,
    trace: Any | None = None,
    console: Console | None = None,
    max_turns: int = 3,
    workspace_state_manager: WorkspaceStateManager | None = None,
    planner: Planner | None = None,
    policy_engine: Any | None = None,
    tool_executor: ToolExecutor | None = None,
    tool_protocol: Any | None = None,
    prompt_assembly: PromptAssemblyPipeline | None = None,
    context_manager: ContextManager | None = None,
    context_db_path: Path | None = None,
    strict: bool = False,
) -> AgentLoop:
    registry = tools or ToolRegistry(workspace_root)
    resolved_trace = trace or JsonlTraceRecorder.create(workspace_root)
    resolved_policy = policy_engine or make_test_policy_engine(workspace_root)
    resolved_planner = planner or Planner(
        workspace_root,
        session_id=getattr(resolved_trace, "run_id", "session_1"),
        task_id=getattr(resolved_trace, "run_id", "task_1"),
        trace=resolved_trace,
    )
    resolved_model_runner = model_runner or ModelRunner.from_chat_provider(
        provider,
        tool_registry=registry,
        trace=resolved_trace,
    )
    resolved_tool_executor = tool_executor or ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=resolved_trace,
        workspace_root=workspace_root,
        planner=resolved_planner,
        policy_engine=resolved_policy,
    )
    hook = _workspace_state_context_hook(workspace_state_manager) if workspace_state_manager is not None else None
    resolved_tool_protocol = tool_protocol or ToolProtocolEngine(
        registry=registry,
        trace=resolved_trace,
        state_store=ToolProtocolStateStore(_tool_protocol_state_path(workspace_root, resolved_trace)),
        workspace_state_hook=hook,
    )
    if hook is not None and getattr(resolved_tool_protocol, "workspace_state_hook", None) is None:
        resolved_tool_protocol.workspace_state_hook = hook
    resolved_prompt_assembly = prompt_assembly or PromptAssemblyPipeline(
        workspace_root=workspace_root,
        trace=resolved_trace,
    )
    return AgentLoop(
        provider=provider,
        model_runner=resolved_model_runner,
        tools=registry,
        trace=resolved_trace,
        console=console or Console(file=StringIO(), force_terminal=False),
        max_turns=max_turns,
        planner=resolved_planner,
        policy_engine=resolved_policy,
        tool_executor=resolved_tool_executor,
        tool_protocol=resolved_tool_protocol,
        prompt_assembly=resolved_prompt_assembly,
        context_manager=context_manager,
        context_db_path=context_db_path,
        strict=strict,
    )


def _tool_protocol_state_path(workspace_root: Path, trace: Any) -> Path:
    run_dir = getattr(getattr(trace, "store", None), "run_dir", None)
    if run_dir is not None:
        return Path(run_dir) / "tool_protocol.sqlite3"
    run_id = str(getattr(trace, "run_id", "default"))
    return workspace_root / ".singularity" / "runs" / run_id / "tool_protocol.sqlite3"


def _workspace_state_context_hook(workspace_state_manager: WorkspaceStateManager):
    def hook(context, *, batch, tool_call_id: str | None) -> None:
        _ = batch, tool_call_id
        workspace_state_manager.record_external_changes()
        context.add_workspace_state(workspace_state_manager.get_workspace_health().to_observation())

    return hook
