from io import StringIO
from pathlib import Path

from rich.console import Console

from miniharness.agent import MiniAgent
from miniharness.model import MockModelProvider, ModelRuntime
from miniharness.planner import PlannerRuntime
from miniharness.tools import ToolRegistry
from miniharness.trace import TraceWriter


def test_agent_uses_model_runtime_for_turns_and_final_report_has_usage(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    runtime = ModelRuntime.with_mock_provider(
        MockModelProvider(text="done"),
        tool_registry=ToolRegistry(tmp_path),
    )
    agent = MiniAgent(
        model_runtime=runtime,
        tools=ToolRegistry(tmp_path),
        trace=TraceWriter.create(tmp_path),
        console=Console(file=StringIO(), force_terminal=False),
        max_turns=1,
        planner=planner,
    )

    answer = agent.run("say something")

    assert answer == "done"
    assert runtime.turn_count == 1

