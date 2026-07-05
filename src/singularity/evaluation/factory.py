from __future__ import annotations

from typing import Any

from singularity.evaluation.harness import EvaluationHarness


def build_evaluation_harness_factory(
    *,
    project_root: Any,
    trace: Any,
    infra: Any,
    execution_core: Any,
    tool_protocol: Any,
    verification_review: Any,
    planner: Any,
):
    def build_evaluation_harness() -> EvaluationHarness:
        return EvaluationHarness(
            project_root=project_root,
            trace_recorder=trace,
            verification_runner=verification_review.verification_runner,
            memory_pipeline=infra.memory_pipeline,
            planner=planner,
            tool_executor=tool_protocol.tool_executor,
            command_executor=execution_core.command_executor,
            mutation_manager=execution_core.mutation_manager,
        )

    return build_evaluation_harness
