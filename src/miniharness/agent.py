from __future__ import annotations

from typing import Any

from rich.console import Console

from miniharness.context import ContextManager
from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.planner import PlannerRuntime, TaskStatus
from miniharness.provider import OpenAICompatibleProvider
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.trace import TraceWriter
from miniharness.workspace_state import LocalWorkspaceStateRuntime


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
        provider: OpenAICompatibleProvider,
        tools: ToolRegistry,
        trace: TraceWriter,
        console: Console,
        max_turns: int,
        state_runtime: LocalWorkspaceStateRuntime | None = None,
        planner: PlannerRuntime | None = None,
    ) -> None:
        self.provider = provider
        self.tools = tools
        self.trace = trace
        self.console = console
        self.max_turns = max_turns
        self.state_runtime = state_runtime
        self.planner = planner

    def run(self, user_goal: str) -> str:
        planner = self.planner or PlannerRuntime(
            self.tools.project_root,
            session_id=self.trace.run_id,
            task_id=self.trace.run_id,
            trace=self.trace,
        )
        self.planner = planner
        if planner.state is None:
            planner.start_task(user_goal)
        context = ContextManager(
            system_prompt=SYSTEM_PROMPT,
            user_goal=user_goal,
            provider=self.provider,
            run_id=self.trace.run_id,
            db_path=self._context_db_path(),
            trace=self.trace,
        )
        tool_schemas = self.tools.openai_tools()
        runtime = ToolRuntime(
            registry=self.tools,
            policy=ToolPolicy.coding_agent(),
            trace=self.trace,
            workspace_root=self.tools.project_root,
            planner=planner,
        )

        for turn in range(1, self.max_turns + 1):
            self.console.print(f"[cyan]model turn {turn}[/cyan]")
            planner.step()
            active_tool_schemas = planner.filtered_tools(tool_schemas)
            messages = context.messages(
                tools=active_tool_schemas,
                planner_context=planner.planner_context_message(),
            )
            self.trace.record(
                "model_request",
                {"turn": turn, "messages": messages, "tools": active_tool_schemas},
            )

            response = self.provider.chat(messages=messages, tools=active_tool_schemas)
            self.trace.record("model_response", {"turn": turn, "response": response})

            assistant_message = self._extract_assistant_message(response)
            context.add_assistant_message(assistant_message)

            tool_calls = assistant_message.get("tool_calls") or []
            for tool_call in tool_calls:
                if hasattr(self.trace, "emit"):
                    self.trace.emit(
                        TraceEventType.MODEL_TOOL_CALL_PROPOSED,
                        runtime="model",
                        summary=(
                            "Model proposed tool call "
                            f"{tool_call.get('function', {}).get('name', '<unknown>')}."
                        ),
                        payload={
                            "turn": turn,
                            "tool_call_id": tool_call.get("id"),
                            "function": tool_call.get("function", {}).get("name"),
                        },
                        ids={
                            "task_id": planner.task_id,
                            "session_id": planner.session_id,
                            "phase_id": planner.state.current_phase if planner.state else None,
                            "action_id": tool_call.get("id"),
                        },
                    )
            if not tool_calls:
                final_answer = self._planner_final_answer(
                    planner,
                    model_answer=assistant_message.get("content") or "",
                )
                self.trace.record(
                    "final_answer", {"turn": turn, "content": final_answer}
                )
                return final_answer

            for tool_call in tool_calls:
                name = tool_call.get("function", {}).get("name", "<unknown>")
                self.console.print(f"[magenta]tool[/magenta] {name}")

                result_model = runtime.execute_tool_call(tool_call)
                result = result_model.model_dump(mode="json")
                self.trace.record(
                    "tool_result",
                    {
                        "turn": turn,
                        "tool_call_id": tool_call.get("id"),
                        "name": name,
                        "result": result,
                    },
                )
                context.add_tool_result(tool_call=tool_call, result=result, turn=turn)
                self._inject_workspace_state(
                    context,
                    tool_call_name=name,
                    turn=turn,
                )

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

    def _inject_workspace_state(
        self,
        context: ContextManager,
        *,
        tool_call_name: str,
        turn: int,
    ) -> None:
        if self.state_runtime is None or tool_call_name == "workspace_health":
            return
        self.state_runtime.record_external_changes()
        context.add_tool_result(
            tool_call={
                "id": f"workspace_state_{self.trace.run_id}_{turn}",
                "type": "function",
                "function": {"name": "workspace_health", "arguments": "{}"},
            },
            result={
                "ok": True,
                "content": self.state_runtime.get_workspace_health().to_observation(),
                "metadata": {"tool_version": "internal"},
            },
            turn=turn,
        )

    def _context_db_path(self) -> Any:
        if hasattr(self.trace, "store"):
            return self.trace.store.run_dir / "context.sqlite3"
        return self.trace.path.parent / self.trace.run_id / "context.sqlite3"

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
