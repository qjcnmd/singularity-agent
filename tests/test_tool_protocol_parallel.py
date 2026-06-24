from __future__ import annotations

import time
from threading import Lock
from typing import Any

from singularity.model import ModelToolParseStatus
from singularity.tool_protocol.models import ToolCallEnvelope
from singularity.tool_protocol.parallel import ParallelToolExecutor
from singularity.tools import ToolResult


def _call(call_id: str, name: str) -> ToolCallEnvelope:
    return ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="msg_1",
        tool_call_id=call_id,
        tool_name=name,
        raw_arguments="{}",
        parsed_arguments={},
        normalized_arguments={},
        parse_status=ModelToolParseStatus.VALID,
    )


def test_parallel_executor_collects_completed_futures_without_losing_input_order() -> None:
    completion_order: list[str] = []
    lock = Lock()

    class ExecutorStub:
        def execute_tool_call(self, tool_call: dict[str, Any]) -> ToolResult:
            name = tool_call["function"]["name"]
            if name == "slow":
                time.sleep(0.1)
            with lock:
                completion_order.append(name)
            return ToolResult.success(content={"name": name})

    results = ParallelToolExecutor(max_workers=2).execute(
        [_call("call_slow", "slow"), _call("call_fast", "fast")],
        tool_executor=ExecutorStub(),  # type: ignore[arg-type]
    )

    assert completion_order == ["fast", "slow"]
    assert [item.call.tool_name for item in results] == ["slow", "fast"]
