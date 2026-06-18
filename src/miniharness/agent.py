from __future__ import annotations

from pathlib import Path
from typing import Any

from rich.console import Console

from miniharness.context import ContextManager
from miniharness.instructions import InstructionRuntime
from miniharness.model import ModelPurpose, ModelRuntime, ModelTurnStatus
from miniharness.planner import PlannerRuntime, TaskStatus
from miniharness.provider import OpenAICompatibleProvider
from miniharness.policy import PolicyRuntime
from miniharness.tool_protocol.runtime import ToolCallingProtocolRuntime
from miniharness.tools import ToolRegistry, ToolRuntime
from miniharness.trace import TraceWriter


SYSTEM_PROMPT = """You are Miniharness, a local coding agent harness.

You can inspect the current project by using the provided read-only tools:
- list_files lists project files.
- read_file reads one project file.
- search_text searches for text in project files.

All file mutations must use the workspace mutation tools. Never claim that you
edited files unless a workspace mutation tool returned an applied mutation.
All ad-hoc commands, formatter, package-manager, dev-server, and git read-only
execution must use the command runtime tools. Verification behavior such as
tests, lint, typecheck, builds, syntax checks, and smoke checks must use the
VerificationRuntime tools, not direct run_command. Never claim that you ran
commands unless run_command, a process-session tool, or run_verification
returned a command or verification result.
Never claim a coding task is complete unless the latest VerificationRuntime
CompletionAssessment says it is ready or ready_with_warnings, and report any
warnings or remaining risks.
Do not claim that you browsed the web, stored memory, or contacted other agents.
When you have enough information, answer the user directly.
"""


class MiniAgent:
    def __init__(
        self,
        *,
        provider: OpenAICompatibleProvider | None = None,
        model_runtime: ModelRuntime,
        tools: ToolRegistry,
        trace: TraceWriter,
        console: Console,
        max_turns: int,
        planner: PlannerRuntime,
        policy_runtime: PolicyRuntime | None = None,
        tool_runtime: ToolRuntime,
        protocol_runtime: ToolCallingProtocolRuntime,
        instruction_runtime: InstructionRuntime,
        context_manager: ContextManager | None = None,
        context_db_path: Path | None = None,
        strict: bool = False,
    ) -> None:
        if model_runtime is None:
            raise ValueError("model_runtime is required; MiniAgent does not assemble model runtimes.")
        if planner is None:
            raise ValueError("planner is required; MiniAgent does not assemble PlannerRuntime.")
        if tool_runtime is None:
            raise ValueError("tool_runtime is required; MiniAgent does not assemble ToolRuntime.")
        if protocol_runtime is None:
            raise ValueError(
                "protocol_runtime is required; MiniAgent does not assemble ToolCallingProtocolRuntime."
            )
        if instruction_runtime is None:
            raise ValueError(
                "instruction_runtime is required; MiniAgent does not assemble InstructionRuntime."
            )
        self.provider = provider
        self.model_runtime = model_runtime
        self.tools = tools
        self.trace = trace
        self.console = console
        self.max_turns = max_turns
        self.planner = planner
        self.policy_runtime = policy_runtime
        self.tool_runtime = tool_runtime
        self.protocol_runtime = protocol_runtime
        self.instruction_runtime = instruction_runtime
        self.context_manager = context_manager
        self.context_db_path = context_db_path
        self.strict = strict

    def run(self, user_goal: str) -> str:
        planner = self.planner
        if planner.state is None:
            planner.start_task(user_goal)
        context = self.context_manager
        if context is None:
            context = ContextManager(
                system_prompt=SYSTEM_PROMPT,
                user_goal=user_goal,
                provider=self.provider,
                model_runtime=self.model_runtime,
                run_id=self.trace.run_id,
                session_id=getattr(planner, "session_id", self.trace.run_id),
                task_id=getattr(planner, "task_id", self.trace.run_id),
                db_path=self.context_db_path or self._context_db_path(),
                trace=self.trace,
            )
        else:
            context.user_goal = user_goal
        model_runtime = self.model_runtime
        instruction_runtime = self.instruction_runtime
        tool_schemas = self.tools.openai_tools(strict=self.strict)
        runtime = self.tool_runtime
        protocol_runtime = self.protocol_runtime

        for turn in range(1, self.max_turns + 1):
            self.console.print(f"[cyan]model turn {turn}[/cyan]")
            planner.step()
            active_tool_schemas = planner.filtered_tools(tool_schemas)
            allowed_tool_names = [
                tool.get("function", {}).get("name")
                for tool in active_tool_schemas
                if tool.get("function", {}).get("name")
            ]
            request = model_runtime.build_request_from_context(
                context,
                run_id=self.trace.run_id,
                session_id=getattr(planner, "session_id", self.trace.run_id),
                task_id=getattr(planner, "task_id", self.trace.run_id),
                phase_id=planner.state.current_phase if planner.state else "model",
                action_id=f"turn_{turn}",
                purpose=ModelPurpose.PLAN_NEXT_ACTION,
                allowed_tool_names=allowed_tool_names,
                planner_context=planner.planner_context_message(),
                instruction_runtime=instruction_runtime,
                user_task=user_goal,
                strict_tools=self.strict,
            )
            planner.record_instruction_prompt_observation(instruction_runtime.summary())
            result = model_runtime.run_turn(request)
            if result.status != ModelTurnStatus.SUCCESS:
                self._record_model_failure(planner, result, turn=turn)
                final_answer = (
                    "Model turn did not produce a valid response: "
                    + (
                        result.error.message
                        if result.error is not None
                        else ", ".join(result.validation.errors if result.validation else [])
                    )
                )
                self.trace.record(
                    "final_answer", {"turn": turn, "content": final_answer}
                )
                return final_answer

            assistant_message = self._assistant_message_from_result(result)
            if not result.tool_calls:
                context.add_assistant_message(assistant_message)
                final_answer = self._planner_final_answer(
                    planner,
                    model_answer=assistant_message.get("content") or "",
                )
                self.trace.record(
                    "final_answer", {"turn": turn, "content": final_answer}
                )
                return final_answer

            protocol_result = protocol_runtime.process_model_turn(
                request=request,
                result=result,
                turn=turn,
                context=context,
                tool_runtime=runtime,
                planner=planner,
                policy_runtime=self.policy_runtime,
            )
            if protocol_result.next_action == "finalize":
                final_answer = self._planner_final_answer(
                    planner,
                    model_answer=assistant_message.get("content") or "",
                )
                self.trace.record(
                    "final_answer", {"turn": turn, "content": final_answer}
                )
                return final_answer

        message = f"Stopped after max_turns={self.max_turns}; the model did not produce a final answer."
        self.trace.record("error", {"type": "MaxTurnsExceeded", "message": message})
        self.trace.record("final_answer", {"turn": self.max_turns, "content": message})
        return message

    @staticmethod
    def _planner_final_answer(planner: PlannerRuntime, *, model_answer: str) -> str:
        assessment = planner.assess_completion()
        if assessment["status"] != TaskStatus.COMPLETED.value:
            return (
                "Planner blocked finalization because completion criteria are unmet: "
                + ", ".join(assessment["unmet"])
            )
        if (
            planner.state is not None
            and not planner.state.completion_criteria.required_changes_applied
            and not planner.state.completion_criteria.required_verifications_passed
        ):
            return model_answer
        report = planner.finalize()
        return "\n".join(
            [
                f"status: {report.status.value}",
                f"files_changed: {', '.join(report.files_changed) if report.files_changed else '-'}",
                f"verification: {report.verification_summary.get('status', 'unknown')}",
                f"unresolved_issues: {len(report.unresolved_issues)}",
                f"risks: {len(report.risks)}",
            ]
        )

    def _context_db_path(self) -> Any:
        if hasattr(self.trace, "store"):
            return self.trace.store.run_dir / "context.sqlite3"
        return self.trace.path.parent / self.trace.run_id / "context.sqlite3"

    def _record_model_failure(
        self,
        planner: PlannerRuntime,
        result: Any,
        *,
        turn: int,
    ) -> None:
        details = {
            "turn": turn,
            "status": result.status.value,
            "error": result.error.to_dict() if result.error else None,
            "validation": result.validation.to_dict() if result.validation else None,
        }
        planner.evidence.unresolved_failures.append({"model_turn": details})
        if planner.state is not None:
            planner.state.blocked_reasons.append("model_runtime_failed")

    @staticmethod
    def _assistant_message_from_result(result: Any) -> dict[str, Any]:
        message = result.assistant_message
        assistant_message: dict[str, Any] = {
            "role": "assistant",
            "content": message.text if message is not None else "",
        }
        if result.tool_calls:
            assistant_message["tool_calls"] = [
                tool_call.to_provider_tool_call() for tool_call in result.tool_calls
            ]
            if not assistant_message["content"]:
                assistant_message["content"] = None
        return assistant_message

    @staticmethod
    def _extract_assistant_message(response: dict[str, Any]) -> dict[str, Any]:
        choices = response.get("choices") or []
        if not choices:
            raise ValueError("Model response did not include choices.")

        message = choices[0].get("message") or {}
        if message.get("role") != "assistant":
            message["role"] = "assistant"

        assistant_message: dict[str, Any] = {
            "role": "assistant",
            "content": message.get("content"),
        }
        if message.get("tool_calls"):
            assistant_message["tool_calls"] = message["tool_calls"]
        return assistant_message
