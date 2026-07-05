from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from singularity.context import ContextManager
from singularity.execution_outcome import ExecutionOutcome
from singularity.model import ModelPurpose, ModelRunner, ModelTurnStatus
from singularity.planner import Planner
from singularity.run_controller import RunController
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tools import ToolExecutor, ToolRegistry


@dataclass(frozen=True)
class TurnCoordinatorCallbacks:
    publish_progress: Callable[[int], None]
    record_model_failure: Callable[..., None]
    outcome_from_model_failure: Callable[[Any], ExecutionOutcome]
    terminal_result_from_outcome: Callable[..., Any]
    record_outcome_context: Callable[[ContextManager, Planner, ExecutionOutcome], None]
    assistant_message_from_result: Callable[[Any], dict[str, Any]]
    attempt_finalize: Callable[..., Any]
    maybe_analyze_failure: Callable[..., ExecutionOutcome | None]
    should_auto_finalize_after_tools: Callable[[Planner, Any], bool]


@dataclass(frozen=True)
class TurnRuntimeDependencies:
    model_runner: ModelRunner
    tools: ToolRegistry
    tool_executor: ToolExecutor
    tool_protocol: ToolProtocolEngine


class TurnCoordinator:
    def __init__(
        self,
        *,
        trace: Any,
        dependencies: TurnRuntimeDependencies,
        prompt_assembly: Any,
        strict: bool,
        callbacks: TurnCoordinatorCallbacks,
    ) -> None:
        self.trace = trace
        self.dependencies = dependencies
        self.prompt_assembly = prompt_assembly
        self.strict = strict
        self.callbacks = callbacks

    def run_turn(
        self,
        turn: int,
        *,
        user_goal: str,
        planner: Planner,
        controller: RunController,
        context: ContextManager,
        tool_schemas: list[dict[str, Any]],
    ) -> Any | None:
        self.callbacks.publish_progress(turn)
        planner.step()
        effective_goal = getattr(planner.state, "effective_goal", None) or user_goal
        context.set_user_goal(effective_goal)
        turn_action_id = f"turn_{turn}"
        active_tool_schemas = planner.filtered_tools(
            tool_schemas,
            tool_specs=self.dependencies.tools.list(),
            action_id=turn_action_id,
        )
        allowed_tool_names = [
            tool.get("function", {}).get("name")
            for tool in active_tool_schemas
            if tool.get("function", {}).get("name")
        ]
        request = self.dependencies.model_runner.build_request_from_context(
            context,
            run_id=self.trace.run_id,
            session_id=getattr(planner, "session_id", self.trace.run_id),
            task_id=getattr(planner, "task_id", self.trace.run_id),
            phase_id=planner.state.current_phase if planner.state else "model",
            action_id=turn_action_id,
            purpose=ModelPurpose.PLAN_NEXT_ACTION,
            allowed_tool_names=allowed_tool_names,
            planner_context=planner.planner_context_message(),
            prompt_assembly=self.prompt_assembly,
            user_task=effective_goal,
            strict_tools=self.strict,
        )
        planner.record_instruction_prompt_observation(dict(self.prompt_assembly.summary()))
        result = self.dependencies.model_runner.run_turn(request)
        context.record_model_usage(result)
        if result.status != ModelTurnStatus.SUCCESS:
            self.callbacks.record_model_failure(planner, result, turn=turn)
            outcome = self.callbacks.outcome_from_model_failure(result)
            controller.apply_outcome(outcome)
            self.callbacks.record_outcome_context(context, planner, outcome)
            terminal = self.callbacks.terminal_result_from_outcome(outcome, turn=turn)
            if terminal is not None:
                return terminal
            return None

        assistant_message = self.callbacks.assistant_message_from_result(result)
        if not result.tool_calls:
            context.add_assistant_message(assistant_message)
            final = self.callbacks.attempt_finalize(
                planner,
                controller=controller,
                context=context,
                turn=turn,
                model_answer=assistant_message.get("content") or "",
            )
            if final is not None:
                return final
            return None

        observation_start = len(context.tool_observations)
        protocol_result = self.dependencies.tool_protocol.process_model_turn(
            request=request,
            result=result,
            turn=turn,
            context=context,
            tool_executor=self.dependencies.tool_executor,
            planner=planner,
        )
        if protocol_result.next_action == "finalize":
            final = self.callbacks.attempt_finalize(
                planner,
                controller=controller,
                context=context,
                turn=turn,
                model_answer=assistant_message.get("content") or "",
            )
            if final is not None:
                return final
            return None

        observations = context.tool_observations[observation_start:]
        controller.apply_protocol_result(protocol_result, observations=observations)
        reduced_outcome = controller.reduce_protocol_result(
            protocol_result,
            observations=observations,
        )
        if reduced_outcome is not None:
            controller.apply_outcome(reduced_outcome)
            self.callbacks.record_outcome_context(context, planner, reduced_outcome)
            blocked = self.callbacks.maybe_analyze_failure(
                planner,
                context,
                outcome=reduced_outcome,
                failure_source="tool",
                turn=turn,
            )
            if blocked is not None:
                controller.apply_outcome(blocked)
                self.callbacks.record_outcome_context(context, planner, blocked)
                terminal = self.callbacks.terminal_result_from_outcome(blocked, turn=turn)
                if terminal is not None:
                    return terminal
            terminal = self.callbacks.terminal_result_from_outcome(reduced_outcome, turn=turn)
            if terminal is not None:
                return terminal
        blocked = self.callbacks.maybe_analyze_failure(
            planner,
            context,
            failure_source="verification",
            turn=turn,
        )
        if blocked is not None:
            controller.apply_outcome(blocked)
            self.callbacks.record_outcome_context(context, planner, blocked)
            terminal = self.callbacks.terminal_result_from_outcome(blocked, turn=turn)
            if terminal is not None:
                return terminal
        if self.callbacks.should_auto_finalize_after_tools(planner, protocol_result):
            final = self.callbacks.attempt_finalize(
                planner,
                controller=controller,
                context=context,
                turn=turn,
                model_answer=assistant_message.get("content") or "",
            )
            if final is not None:
                return final
        return None
