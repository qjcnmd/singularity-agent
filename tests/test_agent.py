from pathlib import Path
from typing import Any

from miniharness.planner import PlannerRuntime
from miniharness.tools import ToolRegistry
from miniharness.tools.mutation import register_mutation_tools
from miniharness.trace import TraceWriter
from miniharness.workspace import MutationRuntime
from miniharness.workspace_state import LocalWorkspaceStateRuntime
from tests.agent_runtime_helpers import make_agent_session


class MockProvider:
    def __init__(self, *responses: dict[str, Any]) -> None:
        self.responses = list(responses)
        self.calls: list[dict[str, Any]] = []

    def chat(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]]
    ) -> dict[str, Any]:
        self.calls.append({"messages": messages, "tools": tools})
        return self.responses.pop(0)


def test_agent_returns_final_answer_without_tool_calls(tmp_path: Path) -> None:
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "plain final answer",
                    }
                }
            ]
        }
    )
    agent = make_agent_session(tmp_path, provider=provider)

    answer = agent.run("say something")

    assert answer == "plain final answer"
    assert len(provider.calls) == 1
    assert provider.calls[0]["messages"][0]["role"] == "system"
    assert provider.calls[0]["messages"][1] == {
        "role": "user",
        "content": "say something",
    }


def test_agent_runs_complete_tool_call_loop(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("MiniHarness README content", encoding="utf-8")
    final_response = "README says this is MiniHarness."
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {
                                "id": "call_readme",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": '{"path": "README.md"}',
                                },
                            }
                        ],
                    }
                }
            ]
        },
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": final_response,
                    }
                }
            ]
        },
    )
    agent = make_agent_session(tmp_path, provider=provider)

    answer = agent.run("read the README")

    assert answer == final_response
    assert len(provider.calls) == 2
    second_messages = provider.calls[1]["messages"]
    tool_messages = [message for message in second_messages if message["role"] == "tool"]
    assert len(tool_messages) == 1
    assert "MiniHarness README content" in tool_messages[0]["content"]


def test_agent_injects_workspace_state_observation_after_tool_call(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("MiniHarness README content", encoding="utf-8")
    trace = TraceWriter.create(tmp_path)
    state = LocalWorkspaceStateRuntime(tmp_path, trace=trace)
    state.begin_session(task_id="task_1", session_id="session_1")
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {
                                "id": "call_readme",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": '{"path": "README.md"}',
                                },
                            }
                        ],
                    }
                }
            ]
        },
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "done",
                    }
                }
            ]
        },
    )
    agent = make_agent_session(
        tmp_path,
        provider=provider,
        trace=trace,
        state_runtime=state,
    )

    answer = agent.run("read the README")

    assert answer == "done"
    second_messages = provider.calls[1]["messages"]
    assert not any(message["role"] == "tool" and message.get("name") == "workspace_health" for message in second_messages)
    workspace_messages = [
        message
        for message in second_messages
        if message["role"] == "system" and "workspace_state" in str(message.get("content"))
    ]
    assert len(workspace_messages) == 1
    assert "journal" not in workspace_messages[0]["content"].lower()


def test_agent_filters_tools_and_injects_planner_context(tmp_path: Path) -> None:
    trace = TraceWriter.create(tmp_path)
    tools = ToolRegistry(tmp_path)
    register_mutation_tools(tools, MutationRuntime(tmp_path, trace=trace))
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1", trace=trace)
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "no changes needed",
                    }
                }
            ]
        }
    )
    agent = make_agent_session(
        tmp_path,
        provider=provider,
        tools=tools,
        trace=trace,
        max_turns=1,
        planner=planner,
    )

    agent.run("inspect only")

    exposed_tool_names = {
        tool["function"]["name"] for tool in provider.calls[0]["tools"]
    }
    assert "read_file" in exposed_tool_names
    assert "workspace_create_file" not in exposed_tool_names
    assert any(
        message["role"] == "system" and '"planner"' in message["content"]
        for message in provider.calls[0]["messages"]
    )


def test_agent_returns_planner_final_report_when_completion_evidence_exists(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "model says done",
                    }
                }
            ]
        }
    )
    agent = make_agent_session(
        tmp_path,
        provider=provider,
        max_turns=1,
        planner=planner,
    )

    original_start = planner.start_task

    def start_with_evidence(goal: str) -> Any:
        state = original_start(goal)
        planner.evidence.inspected_files.append("README.md")
        planner.evidence.applied_changes.append(
            {"changed_files": ["README.md"], "transaction_id": "tx_1"}
        )
        planner.state.linked_transactions.append("tx_1")
        planner.evidence.verification_results.append(
            {
                "completion_assessment": {"status": "ready", "warnings": [], "remaining_risks": []},
                "check_status": [{"check_id": "check_1", "status": "passed"}],
            }
        )
        planner.state.final_assessment = {"status": "ready"}
        return state

    planner.start_task = start_with_evidence  # type: ignore[method-assign]

    answer = agent.run("finish with facts")

    assert "status: completed" in answer
    assert "files_changed: README.md" in answer
    assert "model says done" not in answer


def test_agent_blocks_final_answer_when_completion_evidence_is_missing(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "done without evidence",
                    }
                }
            ]
        }
    )
    agent = make_agent_session(
        tmp_path,
        provider=provider,
        max_turns=1,
        planner=planner,
    )

    answer = agent.run("change code")

    assert "Planner blocked finalization" in answer
    assert "required_changes_applied" in answer
