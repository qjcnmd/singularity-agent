from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from singularity.agent import SingularityAgentRunStatus
from singularity.command import CommandRuntime
from singularity.edit import EditRuntime
from singularity.planner import PlannerRuntime, TaskStatus
from singularity.policy import ApprovalMode, PolicyConfig, PolicyRuntime, SecurityMode
from singularity.tools import ToolPolicy, ToolRegistry, ToolRuntime
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.trace import TraceWriter
from singularity.verification import VerificationRuntime
from singularity.workspace import MutationRuntime
from singularity.workspace_state import LocalWorkspaceStateRuntime
from tests.agent_runtime_helpers import make_agent_session


def _tool_call(name: str, arguments: dict[str, Any], *, call_id: str = "call_1") -> dict[str, Any]:
    return {
        "id": call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def _patch_for(path: str, before: str, after: str) -> str:
    import difflib

    return "".join(
        difflib.unified_diff(
            before.splitlines(keepends=True),
            after.splitlines(keepends=True),
            fromfile=f"a/{path}",
            tofile=f"b/{path}",
        )
    )


def _new_file_patch(path: str, content: str) -> str:
    import difflib

    return "".join(
        difflib.unified_diff(
            [],
            content.splitlines(keepends=True),
            fromfile="/dev/null",
            tofile=f"b/{path}",
        )
    )


def _tool_runtime(
    tmp_path: Path,
    *,
    planner: PlannerRuntime | None = None,
    trace: TraceWriter | None = None,
    state_runtime: LocalWorkspaceStateRuntime | None = None,
) -> ToolRuntime:
    trace = trace or TraceWriter.create(tmp_path)
    policy = PolicyRuntime(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_mode=ApprovalMode.AUTO_SAFE,
            security_mode=SecurityMode.COMPAT,
        )
    )
    registry = ToolRegistry(tmp_path)
    mutation = MutationRuntime(
        tmp_path,
        trace=trace,
        planner=planner,
        policy_runtime=policy,
        state_runtime=state_runtime,
    )
    register_mutation_tools(registry, mutation)
    register_edit_tools(
        registry,
        EditRuntime(tmp_path, mutation_runtime=mutation, trace=trace, planner=planner),
    )
    command = CommandRuntime(tmp_path, trace=trace, planner=planner, policy_runtime=policy)
    verification = VerificationRuntime(tmp_path, command_runtime=command, trace=trace, planner=planner, policy_runtime=policy)
    register_verification_tools(registry, verification)
    tool_runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=trace,
        workspace_root=tmp_path,
        planner=planner,
        policy_runtime=policy,
    )
    tool_runtime.mutation_runtime = mutation  # controller/internal test hook
    return tool_runtime


def test_write_file_facade_creates_inspect_diff_and_rolls_back(tmp_path: Path) -> None:
    runtime = _tool_runtime(tmp_path)

    result = runtime.execute_tool_call(
        _tool_call("write_file", {"path": "quicksort.py", "content": QUICK_SORT, "mode": "create"}, call_id="write")
    )
    inspect = runtime.execute_tool_call(_tool_call("inspect_diff", {"scope": "current_run"}, call_id="inspect"))

    assert result.ok is True
    assert result.content["status"] == "applied"
    assert result.content["created"] == ["quicksort.py"]
    assert result.content["overwritten"] == []
    assert result.content["changeset_id"]
    assert result.content["diff_digest"]
    assert inspect.ok is True
    assert inspect.content["added_files"] == ["quicksort.py"]
    assert "quicksort.py" in inspect.content["changed_files"]

    rollback = runtime.mutation_runtime.rollback_changeset(result.content["changeset_id"])
    assert rollback.ok is True
    assert not (tmp_path / "quicksort.py").exists()


def test_apply_patch_facade_creates_and_modifies_without_git(tmp_path: Path) -> None:
    runtime = _tool_runtime(tmp_path)
    create = runtime.execute_tool_call(
        _tool_call("apply_patch", {"patch": _new_file_patch("quicksort.py", QUICK_SORT)}, call_id="create")
    )
    before = QUICK_SORT
    after = QUICK_SORT.replace("print(\"ok\")", "print(\"sorted ok\")")
    modify = runtime.execute_tool_call(
        _tool_call("apply_patch", {"patch": _patch_for("quicksort.py", before, after)}, call_id="modify")
    )
    inspect = runtime.execute_tool_call(
        _tool_call("inspect_diff", {"scope": "changeset", "changeset_id": modify.content["changeset_id"]}, call_id="inspect")
    )

    assert create.ok is True
    assert modify.ok is True
    assert (tmp_path / "quicksort.py").read_text(encoding="utf-8") == after
    assert inspect.content["modified_files"] == ["quicksort.py"]
    assert inspect.content["diff_digest"] == modify.content["diff_digest"]


def test_apply_patch_modifies_existing_python_and_smoke_verification_succeeds(tmp_path: Path) -> None:
    (tmp_path / "calc.py").write_text(
        "def add(a, b):\n    return a - b\n\nif __name__ == '__main__':\n    assert add(2, 3) == 5\n",
        encoding="utf-8",
    )
    runtime = _tool_runtime(tmp_path)
    before = (tmp_path / "calc.py").read_text(encoding="utf-8")
    after = before.replace("return a - b", "return a + b")

    patch = runtime.execute_tool_call(
        _tool_call("apply_patch", {"patch": _patch_for("calc.py", before, after)}, call_id="patch")
    )
    verify = runtime.execute_tool_call(
        _tool_call(
            "run_verification",
            {
                "changed_files": ["calc.py"],
                "task_intent": "verify patched calc script",
                "smoke_commands": [["python", "calc.py"]],
                "changeset_id": patch.content["changeset_id"],
            },
            call_id="verify",
        )
    )

    assert patch.ok is True
    assert verify.ok is True
    smoke = next(item for item in verify.content["verification"]["results"] if item["kind"] == "runtime_smoke")
    assert smoke["evidence"]["exit_code"] == 0
    assert verify.content["verification"]["completion_assessment"]["status"] == "ready"


def test_inspect_diff_reports_multi_file_patch_and_rollback_restores(tmp_path: Path) -> None:
    state = LocalWorkspaceStateRuntime(tmp_path)
    state.begin_session(session_id="session_1", task_id="task_1")
    runtime = _tool_runtime(tmp_path, state_runtime=state)
    patch_text = _new_file_patch("a.py", "print('a')\n") + _new_file_patch("b.py", "print('b')\n")

    patch = runtime.execute_tool_call(_tool_call("apply_patch", {"patch": patch_text}, call_id="patch"))
    inspect = runtime.execute_tool_call(_tool_call("inspect_diff", {"scope": "current_run"}, call_id="inspect"))
    before_rollback = state.get_workspace_health()
    rollback = runtime.mutation_runtime.rollback_changeset(patch.content["changeset_id"])
    after_rollback = state.get_workspace_health()

    assert patch.ok is True
    assert sorted(inspect.content["added_files"]) == ["a.py", "b.py"]
    assert sorted(inspect.content["changed_files"]) == ["a.py", "b.py"]
    assert before_rollback.agent_changes == ["a.py", "b.py"]
    assert rollback.ok is True
    assert not (tmp_path / "a.py").exists()
    assert not (tmp_path / "b.py").exists()
    assert after_rollback.agent_changes == []


def test_apply_patch_conflict_and_illegal_patch_leave_workspace_unchanged(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("print('current')\n", encoding="utf-8")
    other = tmp_path / "other.py"
    other.write_text("old\n", encoding="utf-8")
    runtime = _tool_runtime(tmp_path)
    stale = _patch_for("app.py", "print('old')\n", "print('new')\n")
    good = _patch_for("other.py", "old\n", "new\n")

    conflict = runtime.execute_tool_call(_tool_call("apply_patch", {"patch": stale}, call_id="conflict"))
    mixed = runtime.execute_tool_call(_tool_call("apply_patch", {"patch": good + stale}, call_id="mixed"))
    illegal = runtime.execute_tool_call(_tool_call("apply_patch", {"patch": "not a patch"}, call_id="illegal"))

    assert conflict.ok is False
    assert conflict.error_code in {"patch_context_not_found", "invalid_patch", "transaction_failed"}
    assert mixed.ok is False
    assert other.read_text(encoding="utf-8") == "old\n"
    assert illegal.ok is False
    assert source.read_text(encoding="utf-8") == "print('current')\n"


def test_workspace_escape_is_rejected_and_low_level_tools_stay_hidden(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    planner.start_task("change code")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    runtime = _tool_runtime(tmp_path, planner=planner)

    denied = runtime.execute_tool_call(
        _tool_call("write_file", {"path": "../outside.txt", "content": "x", "mode": "create"}, call_id="escape")
    )
    visible = {
        tool["function"]["name"]
        for tool in planner.filtered_tools(runtime.registry.openai_tools())
    }

    assert denied.ok is False
    assert denied.error_code in {"path_outside_workspace", "policy_denied"}
    assert {"write_file", "apply_patch", "inspect_diff"} <= visible
    assert "workspace_create_file" not in visible


def test_checklist_schema_aliases_and_file_diff_scope(tmp_path: Path) -> None:
    runtime = _tool_runtime(tmp_path)
    missing_parent = runtime.execute_tool_call(
        _tool_call(
            "write_file",
            {
                "path": "pkg/quicksort.py",
                "content": QUICK_SORT,
                "overwrite_policy": "create",
            },
            call_id="missing_parent",
        )
    )
    created = runtime.execute_tool_call(
        _tool_call(
            "write_file",
            {
                "path": "pkg/quicksort.py",
                "content": QUICK_SORT,
                "create_dirs": True,
                "overwrite_policy": "create",
            },
            call_id="create_dirs",
        )
    )
    before = QUICK_SORT
    after = QUICK_SORT.replace("print(\"ok\")", "print(\"schema ok\")")
    patched = runtime.execute_tool_call(
        _tool_call(
            "apply_patch",
            {"unified_diff": _patch_for("pkg/quicksort.py", before, after), "strict": True},
            call_id="unified_diff",
        )
    )
    inspected = runtime.execute_tool_call(
        _tool_call(
            "inspect_diff",
            {"scope": "file", "path": "pkg/quicksort.py"},
            call_id="file_diff",
        )
    )

    assert missing_parent.ok is False
    assert missing_parent.error_code == "parent_directory_missing"
    assert created.ok is True
    assert patched.ok is True
    assert (tmp_path / "pkg" / "quicksort.py").read_text(encoding="utf-8") == after
    assert inspected.ok is True
    assert inspected.content["changed_files"] == ["pkg/quicksort.py"]


def test_facades_record_policy_trace_and_completion_evidence(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1", trace=trace)
    planner.start_task("implement quicksort")
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    runtime = _tool_runtime(tmp_path, planner=planner, trace=trace)

    write = runtime.execute_tool_call(
        _tool_call("write_file", {"path": "quicksort.py", "content": QUICK_SORT, "mode": "create"}, call_id="write")
    )
    inspect = runtime.execute_tool_call(_tool_call("inspect_diff", {"scope": "current_run"}, call_id="inspect"))
    planner.state.status = TaskStatus.APPLYING_CHANGES
    planner.state.current_phase = "applying_changes"
    planner.plan.current_phase = "applying_changes"
    patch = runtime.execute_tool_call(
        _tool_call(
            "apply_patch",
            {"patch": _new_file_patch("extra.py", "print('extra')\n")},
            call_id="patch",
        )
    )

    assert write.ok is True
    assert inspect.ok is True
    assert patch.ok is True
    write_evidence = next(
        item for item in planner.evidence.applied_changes if item["changeset_id"] == write.content["changeset_id"]
    )
    assert write_evidence["diff_digest"] == write.content["diff_digest"]
    assert planner.evidence.diff_observations[-1]["diff_digest"] == inspect.content["diff_digest"]
    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    assert any(event["event"] == "policy" for event in events)
    assert any(event["event"] == "tool_call" and event["data"]["tool_name"] == "write_file" for event in events)
    assert any(event["event"] == "tool_call" and event["data"]["tool_name"] == "apply_patch" for event in events)
    assert any(event["event"] == "tool_call" and event["data"]["tool_name"] == "inspect_diff" for event in events)


def test_deterministic_quicksort_tasks_complete_with_write_file_and_apply_patch(tmp_path: Path) -> None:
    write_result, write_planner = _run_quicksort_agent(
        tmp_path / "write",
        "write_file",
        {"path": "quicksort.py", "content": QUICK_SORT, "mode": "create"},
    )
    patch_result, patch_planner = _run_quicksort_agent(
        tmp_path / "patch",
        "apply_patch",
        {"patch": _new_file_patch("quicksort.py", QUICK_SORT)},
    )

    assert write_result.status == SingularityAgentRunStatus.COMPLETED
    assert patch_result.status == SingularityAgentRunStatus.COMPLETED
    assert write_planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "ready"
    assert patch_planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "ready"


class _FakeProvider:
    def __init__(self, *responses: dict[str, Any]) -> None:
        self.responses = list(responses)

    def chat(self, **_kwargs: Any) -> dict[str, Any]:
        return self.responses.pop(0)


def _run_quicksort_agent(root: Path, mutation_tool: str, mutation_args: dict[str, Any]):
    root.mkdir(parents=True)
    (root / "README.md").write_text("task context", encoding="utf-8")
    planner = PlannerRuntime(root, session_id="session_1", task_id="task_1")
    policy = PolicyRuntime(
        PolicyConfig(
            workspace_root=root,
            approval_mode=ApprovalMode.AUTO_SAFE,
            security_mode=SecurityMode.COMPAT,
        )
    )
    trace = TraceWriter.create(root)
    registry = ToolRegistry(root)
    mutation = MutationRuntime(root, trace=trace, planner=planner, policy_runtime=policy)
    register_mutation_tools(registry, mutation)
    register_edit_tools(registry, EditRuntime(root, mutation_runtime=mutation, trace=trace, planner=planner))
    command = CommandRuntime(root, trace=trace, planner=planner, policy_runtime=policy)
    verification = VerificationRuntime(root, command_runtime=command, trace=trace, planner=planner, policy_runtime=policy)
    register_verification_tools(registry, verification)
    provider = _FakeProvider(
        _tool_response("call_read_1", "read_file", {"path": "README.md"}),
        _tool_response("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        _tool_response("call_mutate", mutation_tool, mutation_args),
        _tool_response(
            "call_verify",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        {"choices": [{"message": {"role": "assistant", "content": "done with evidence"}}]},
    )
    agent = make_agent_session(
        root,
        provider=provider,
        tools=registry,
        trace=trace,
        max_turns=5,
        planner=planner,
        policy_runtime=policy,
    )
    return agent.run("implement quicksort.py and verify it"), planner


def _tool_response(call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": json.dumps(arguments)},
                        }
                    ],
                }
            }
        ]
    }


QUICK_SORT = """\
def quicksort(values):
    if len(values) <= 1:
        return values
    pivot = values[0]
    tail = values[1:]
    return quicksort([item for item in tail if item <= pivot]) + [pivot] + quicksort([item for item in tail if item > pivot])


if __name__ == "__main__":
    assert quicksort([3, 1, 2]) == [1, 2, 3]
    print("ok")
"""
