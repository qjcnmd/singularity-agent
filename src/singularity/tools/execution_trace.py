from __future__ import annotations

from typing import Any

from singularity.tools.execution_pipeline import ToolExecutionPipelineState


class ToolExecutionTraceRecorder:
    def __init__(self, executor: Any) -> None:
        self.executor = executor

    def finalize(self, state: ToolExecutionPipelineState) -> None:
        self.executor._finalize_pipeline_state_impl(state)
