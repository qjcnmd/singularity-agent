from io import StringIO
from pathlib import Path
from typing import Any

from rich.console import Console

from miniharness.agent import MiniAgent
from miniharness.tools import ToolRegistry
from miniharness.trace import TraceWriter


class MockProvider:
    def __init__(self, response: dict[str, Any]) -> None:
        self.response = response
        self.calls: list[dict[str, Any]] = []

    def chat(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]]
    ) -> dict[str, Any]:
        self.calls.append({"messages": messages, "tools": tools})
        return self.response


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
