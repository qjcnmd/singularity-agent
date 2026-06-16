from __future__ import annotations

from typing import Any

from rich.console import Console

from miniharness.context import ContextManager
from miniharness.provider import OpenAICompatibleProvider
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.trace import TraceWriter


SYSTEM_PROMPT = """You are Miniharness, a local coding agent harness.

You can inspect the current project by using the provided read-only tools:
- list_files lists project files.
- read_file reads one project file.
- search_text searches for text in project files.

All file mutations must use the workspace mutation tools. Never claim that you
edited files unless a workspace mutation tool returned an applied mutation.
All command, test, build, formatter, package-manager, dev-server, and git
read-only execution must use the command runtime tools. Never claim that you ran
commands unless run_command or a process-session tool returned a command result.
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
    ) -> None:
        self.provider = provider
        self.tools = tools
        self.trace = trace
        self.console = console
        self.max_turns = max_turns

    def run(self, user_goal: str) -> str:
        context = ContextManager(
            system_prompt=SYSTEM_PROMPT,
            user_goal=user_goal,
            provider=self.provider,
            run_id=self.trace.run_id,
            db_path=self.trace.path.parent / self.trace.run_id / "context.sqlite3",
        )
        tool_schemas = self.tools.openai_tools()
        runtime = ToolRuntime(
            registry=self.tools,
            policy=ToolPolicy.coding_agent(),
            trace=self.trace,
            workspace_root=self.tools.project_root,
        )

        for turn in range(1, self.max_turns + 1):
            self.console.print(f"[cyan]model turn {turn}[/cyan]")
            messages = context.messages(tools=tool_schemas)
            self.trace.record(
                "model_request",
                {"turn": turn, "messages": messages, "tools": tool_schemas},
            )

            response = self.provider.chat(messages=messages, tools=tool_schemas)
            self.trace.record("model_response", {"turn": turn, "response": response})

            assistant_message = self._extract_assistant_message(response)
            context.add_assistant_message(assistant_message)

            tool_calls = assistant_message.get("tool_calls") or []
            if not tool_calls:
                final_answer = assistant_message.get("content") or ""
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

        message = f"Stopped after max_turns={self.max_turns}; the model did not produce a final answer."
        self.trace.record("error", {"type": "MaxTurnsExceeded", "message": message})
        self.trace.record("final_answer", {"turn": self.max_turns, "content": message})
        return message

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
