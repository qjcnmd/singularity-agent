from pathlib import Path

from singularity.model import MockModelProvider, ModelRunner
from singularity.planner import Planner
from singularity.tools import ToolRegistry
from tests.agent_loop_helpers import make_agent_session


def test_agent_uses_model_runner_for_turns_and_final_report_has_usage(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    component = ModelRunner.with_mock_provider(
        MockModelProvider(text="done"),
        tool_registry=ToolRegistry(tmp_path),
    )
    agent = make_agent_session(
        tmp_path,
        model_runner=component,
        tools=ToolRegistry(tmp_path),
        max_turns=1,
        planner=planner,
    )

    answer = agent.run("say something")

    assert answer == "done"
    assert component.turn_count == 1

