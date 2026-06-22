from __future__ import annotations

import json
import inspect
from pathlib import Path
from typing import Any

from singularity.agent import SingularityAgent
from singularity.agent import SingularityAgentRunStatus
from singularity.trace import TraceWriter
from tests.agent_runtime_helpers import make_agent_session


class MockProvider:
    def __init__(self, *responses: dict[str, Any]) -> None:
        self.responses = list(responses)
        self.calls: list[dict[str, Any]] = []

    def chat(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]], tool_choice: Any = None
    ) -> dict[str, Any]:
        self.calls.append({"messages": messages, "tools": tools, "tool_choice": tool_choice})
        return self.responses.pop(0)


def test_agent_delegates_tool_call_processing_to_protocol_runtime(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("Singularity README content", encoding="utf-8")
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
    trace = TraceWriter.create(tmp_path)
    calls: list[dict[str, Any]] = []

    class FakeProtocolRuntime:
        def __init__(self, **kwargs: Any) -> None:
            self.init_kwargs = kwargs

        def process_model_turn(
            self,
            *,
            request: Any,
            result: Any,
            turn: int,
            context: Any,
            tool_runtime: Any,
            planner: Any | None = None,
            policy_runtime: Any | None = None,
        ) -> Any:
            _ = planner, policy_runtime
            calls.append({
                "request": request,
                "result": result,
                "turn": turn,
                "context": context,
                "tool_runtime": tool_runtime,
            })
            context.add_synthetic_tool_error(
                tool_call={
                    "id": "call_readme",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{}"},
                },
                error_code="synthetic",
                message="synthetic tool result",
                turn=turn,
            )
            return type(
                "ProtocolResult",
                (),
                {
                    "status": "processed",
                    "appended_tool_message_count": 1,
                    "executed_count": 0,
                    "failed_count": 0,
                    "rejected_count": 0,
                    "next_action": "continue",
                },
            )()

    agent = make_agent_session(
        tmp_path,
        provider=provider,  # type: ignore[arg-type]
        trace=trace,
        max_turns=2,
        protocol_runtime=FakeProtocolRuntime(),
    )

    answer = agent.run("read the README")

    assert answer.status == SingularityAgentRunStatus.MAX_TURNS_EXCEEDED
    assert len(calls) == 1
    assert hasattr(calls[0]["context"], "add_tool_protocol_result")
    assert hasattr(calls[0]["tool_runtime"], "execute_tool_call")
    second_messages = provider.calls[1]["messages"]
    tool_messages = [message for message in second_messages if message["role"] == "tool"]
    assert len(tool_messages) == 1
    payload = json.loads(tool_messages[0]["content"])
    assert payload["tool_name"] == "read_file"
    assert payload["status"] == "rejected"


def test_agent_run_does_not_manually_loop_tool_calls() -> None:
    source = inspect.getsource(SingularityAgent.run)

    assert "for tool_call in tool_calls" not in source
    assert ".execute_tool_call(tool_call)" not in source
    assert "PlannerRuntime(" not in source
    assert "ToolRuntime(" not in source
    assert "ToolCallingProtocolRuntime(" not in source
    assert "InstructionRuntime(" not in source
    assert "self.protocol_runtime" in source
    assert "process_model_turn" in source
