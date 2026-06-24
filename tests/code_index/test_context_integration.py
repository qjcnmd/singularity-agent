from pathlib import Path

from singularity.context import ContextManager
from singularity.planner import Planner
from singularity.code_index import ProjectIndex


class FakeTrace:
    def __init__(self) -> None:
        self.events = []

    def emit(self, event_type, **kwargs):
        self.events.append((event_type, kwargs))


def test_project_index_context_and_planner_observation_are_untrusted_and_not_inspected(tmp_path: Path) -> None:
    (tmp_path / "app.py").write_text("def main(): pass\n", encoding="utf-8")
    component = ProjectIndex(tmp_path, trace=FakeTrace())
    component.build_full_index(reason="test")
    observation = component.observation_for_goal("main")

    context = ContextManager(system_prompt="system", user_goal="goal", db_path=tmp_path / "context.sqlite")
    planner = Planner(tmp_path, session_id="s1", task_id="t1")
    planner.start_task("main")

    item = context.add_project_index(observation)
    planner.record_project_index_observation(observation)

    assert item.source_component.value == "project_index"
    assert item.content["trust_level"] == "untrusted_workspace_data"
    assert planner.evidence.project_index_observations
    assert planner.evidence.inspected_files == []
