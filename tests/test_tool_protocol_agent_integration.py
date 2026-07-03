from __future__ import annotations

import inspect
import json
from pathlib import Path
from typing import Any

from singularity.agent_loop import AgentLoop, AgentLoopStatus
from singularity.agent_loop_turns import TurnCoordinator
from singularity.context import ContextManager
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.observability.models import TraceEvent, TraceEventType
from singularity.observability.summary import TraceSummaryBuilder
from singularity.planner import Planner, TaskStatus
from tests.agent_loop_helpers import make_agent_session


class MockProvider:
    def __init__(self, *responses: dict[str, Any]) -> None:
        self.responses = list(responses)
        self.calls: list[dict[str, Any]] = []

    def chat(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]], tool_choice: Any = None
    ) -> dict[str, Any]:
        self.calls.append({"messages": messages, "tools": tools, "tool_choice": tool_choice})
        return self.responses.pop(0)


class ContextAwareProvider:
    def __init__(self) -> None:
        self.calls: list[dict[str, Any]] = []

    def chat(
        self, *, messages: list[dict[str, Any]], tools: list[dict[str, Any]], tool_choice: Any = None
    ) -> dict[str, Any]:
        self.calls.append({"messages": messages, "tools": tools, "tool_choice": tool_choice})
        if tool_choice == "none":
            return {
                "usage": {
                    "prompt_tokens": 40,
                    "completion_tokens": 4,
                    "total_tokens": 44,
                },
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": json.dumps(
                                {
                                    "goal": "read README",
                                    "current_state": "old dialogue compacted",
                                    "completed_actions": [],
                                    "pending_actions": [],
                                    "verified_facts": [],
                                    "failed_attempts": [],
                                    "policy_constraints": [],
                                    "workspace_changes": [],
                                    "verification_status": "unknown",
                                    "open_questions": [],
                                    "reference_ids": [],
                                    "omitted_item_ids": [],
                                    "confidence": 0.8,
                                }
                            ),
                        }
                    }
                ],
            }
        model_turn = len([call for call in self.calls if call.get("tool_choice") != "none"])
        if model_turn == 1:
            return {
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 8,
                    "total_tokens": 108,
                    "prompt_tokens_details": {"cached_tokens": 25},
                },
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
                ],
            }
        return {
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 5,
                "total_tokens": 125,
            },
            "choices": [{"message": {"role": "assistant", "content": "done with README"}}],
        }


def jsonl_final_report_summary(trace: JsonlTraceRecorder) -> dict[str, Any]:
    events: list[TraceEvent] = []
    for index, line in enumerate(trace.path.read_text(encoding="utf-8").splitlines()):
        entry = json.loads(line)
        event = entry.get("event")
        if event not in {
            TraceEventType.MODEL_REQUEST_CREATED.value,
            TraceEventType.MODEL_RESPONSE_RECEIVED.value,
        }:
            continue
        data = dict(entry.get("data") or {})
        events.append(
            TraceEvent(
                event_id=f"jsonl_event_{index}",
                event_type=TraceEventType(event),
                run_id=str(entry.get("run_id") or trace.run_id),
                session_id=str(data.get("session_id") or trace.run_id),
                task_id=data.get("task_id"),
                phase_id=data.get("phase_id"),
                action_id=data.get("action_id"),
                parent_event_id=None,
                timestamp=entry["ts"],
                monotonic_ms=index,
                component="model",
                severity="info",
                summary=event,
                payload=data,
            )
        )
    return TraceSummaryBuilder().final_report_summary(events=events, spans=[], artifacts=[])


def test_agent_delegates_tool_call_processing_to_tool_protocol(tmp_path: Path) -> None:
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
    trace = JsonlTraceRecorder.create(tmp_path)
    calls: list[dict[str, Any]] = []

    class FakeProtocolEngine:
        def __init__(self, **kwargs: Any) -> None:
            self.init_kwargs = kwargs

        def process_model_turn(
            self,
            *,
            request: Any,
            result: Any,
            turn: int,
            context: Any,
            tool_executor: Any,
            planner: Any | None = None,
            policy_engine: Any | None = None,
        ) -> Any:
            _ = planner, policy_engine
            calls.append({
                "request": request,
                "result": result,
                "turn": turn,
                "context": context,
                "tool_executor": tool_executor,
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
        tool_protocol=FakeProtocolEngine(),
    )

    answer = agent.run("read the README")

    assert answer.status == AgentLoopStatus.MAX_TURNS_EXCEEDED
    assert len(calls) == 1
    assert hasattr(calls[0]["context"], "add_tool_protocol_result")
    assert hasattr(calls[0]["tool_executor"], "execute_tool_call")
    second_messages = provider.calls[1]["messages"]
    tool_messages = [message for message in second_messages if message["role"] == "tool"]
    assert len(tool_messages) == 1
    payload = json.loads(tool_messages[0]["content"])
    assert payload["tool_name"] == "read_file"
    assert payload["status"] == "rejected"


def test_agent_loop_uses_single_tool_projection_for_schema_choice_and_policy_trace(
    tmp_path: Path,
) -> None:
    provider = MockProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": "done",
                    }
                }
            ],
            "usage": {"prompt_tokens": 20, "completion_tokens": 2, "total_tokens": 22},
        },
    )
    trace = JsonlTraceRecorder.create(tmp_path)
    planner = Planner(tmp_path, session_id=trace.run_id, task_id=trace.run_id, trace=trace)
    planner.start_task("verify safely")
    assert planner.state is not None
    planner.state.status = TaskStatus.RUNNING_VERIFICATION
    planner.state.current_phase = "running_verification"
    planner.state.rolling_plan = {
        "plan_id": "rolling_1",
        "version": 1,
        "user_goal": "verify safely",
        "current_step_id": "step_verify",
        "steps": [
            {
                "step_id": "step_verify",
                "title": "Search then verify",
                "kind": "verify",
                "allowed_capabilities": ["search_text", "run_verification", "read_file"],
                "expected_evidence": [],
                "fallback_steps": [],
            }
        ],
    }
    agent = make_agent_session(
        tmp_path,
        provider=provider,  # type: ignore[arg-type]
        trace=trace,
        planner=planner,
        max_turns=1,
    )

    agent.run("verify safely")

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    exposure = next(event["data"] for event in events if event["event"] == "tool.exposure_decided")
    request = next(event["data"] for event in events if event["event"] == "model.request.created")
    schema_names = [tool["function"]["name"] for tool in provider.calls[0]["tools"]]

    assert exposure["action_id"] == request["action_id"] == "turn_1"
    assert schema_names == exposure["selected_tools"]
    assert request["tool_choice"]["allowed_tool_names"] == schema_names
    assert "search_text" not in schema_names
    assert any(
        item["name"] == "search_text" and item["reason_code"] == "phase_not_allowed"
        for item in exposure["blocked"]
    )


def test_agent_loop_continues_after_compaction_with_usage_tool_observation_and_finalization(
    tmp_path: Path,
) -> None:
    (tmp_path / "README.md").write_text("Singularity README content", encoding="utf-8")
    provider = ContextAwareProvider()
    trace = JsonlTraceRecorder.create(tmp_path)
    planner = Planner(tmp_path, session_id=trace.run_id, task_id=trace.run_id, trace=trace)
    context = ContextManager(
        system_prompt="system",
        user_goal="read README",
        provider=provider,  # type: ignore[arg-type]
        db_path=tmp_path / "context.sqlite3",
        run_id=trace.run_id,
        session_id=trace.run_id,
        task_id=trace.run_id,
        trace=trace,
        model_context_window=1400,
        output_token_reserve=20,
    )
    context.add_assistant_message({"role": "assistant", "content": "old history " * 1000})
    agent = make_agent_session(
        tmp_path,
        provider=provider,  # type: ignore[arg-type]
        trace=trace,
        planner=planner,
        context_manager=context,
        max_turns=3,
    )

    result = agent.run("read README")

    assert result.status == AgentLoopStatus.COMPLETED
    assert "done with README" in result.final_answer
    model_calls = [call for call in provider.calls if call["tool_choice"] != "none"]
    assert len(model_calls) == 2
    assert any("old dialogue compacted" in str(message.get("content")) for message in model_calls[0]["messages"])
    tool_payloads = [
        json.loads(str(message.get("content") or "{}"))
        for message in model_calls[1]["messages"]
        if message["role"] == "tool"
    ]
    assert any(
        payload.get("tool_name") == "read_file" and payload.get("ok") is True
        for payload in tool_payloads
    )
    latest = context.store.latest_bundle(context.run_id)
    assert latest is not None
    assert latest.metadata["cache"]["cache_attribution"]["source"] == "component_inferred"
    assert latest.metadata["context_usage_report"]["cache_attribution"]["source"] == "component_inferred"
    assert context.context_usage_diagnostic()["cache_hit_ratio"] == 0.0
    assert context.store.observation_count(context.run_id) == 1
    report_summary = jsonl_final_report_summary(trace)
    assert report_summary["model_usage_summary"]["cache_attribution_source_counts"]["provider_native"] >= 1
    assert report_summary["model_usage_summary"]["cache_attribution_source_counts"]["component_inferred"] >= 1


def test_agent_run_does_not_manually_loop_tool_calls() -> None:
    source = inspect.getsource(AgentLoop.run)
    turn_source = inspect.getsource(TurnCoordinator.run_turn)
    combined_source = source + turn_source

    assert "for tool_call in tool_calls" not in combined_source
    assert ".execute_tool_call(tool_call)" not in combined_source
    assert "Planner(" not in source
    assert "ToolExecutor(" not in source
    assert "ToolProtocolEngine(" not in source
    assert "PromptAssemblyPipeline(" not in source
    assert "_turn_coordinator()" in source
    assert "self.tool_protocol" in turn_source
    assert "process_model_turn" in turn_source
