from pathlib import Path

from miniharness.model import MockModelProvider, ModelRuntime
from miniharness.planner import PlannerRuntime
from miniharness.tools import ToolRegistry
from tests.agent_runtime_helpers import make_agent_session


def test_agent_uses_model_runtime_for_turns_and_final_report_has_usage(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    runtime = ModelRuntime.with_mock_provider(
        MockModelProvider(text="done"),
        tool_registry=ToolRegistry(tmp_path),
    )
    agent = make_agent_session(
        tmp_path,
        model_runtime=runtime,
        tools=ToolRegistry(tmp_path),
        max_turns=1,
        planner=planner,
    )

    answer = agent.run("say something")

    assert answer == "done"
    assert runtime.turn_count == 1

