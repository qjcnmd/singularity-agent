from io import StringIO
from pathlib import Path
from typing import Any

from rich.console import Console

from miniharness.agent import MiniAgent
from miniharness.tools import ToolRegistry
from miniharness.trace import TraceWriter


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
    agent = MiniAgent(
        provider=provider,  # type: ignore[arg-type]
        tools=ToolRegistry(tmp_path),
        trace=TraceWriter.create(tmp_path),
        console=Console(file=StringIO(), force_terminal=False),
        max_turns=3,
    )

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
    agent = MiniAgent(
        provider=provider,  # type: ignore[arg-type]
        tools=ToolRegistry(tmp_path),
        trace=TraceWriter.create(tmp_path),
        console=Console(file=StringIO(), force_terminal=False),
        max_turns=3,
    )

    answer = agent.run("read the README")

    assert answer == final_response
    assert len(provider.calls) == 2
    second_messages = provider.calls[1]["messages"]
    tool_messages = [message for message in second_messages if message["role"] == "tool"]
    assert len(tool_messages) == 1
    assert "MiniHarness README content" in tool_messages[0]["content"]
