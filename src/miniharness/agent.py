from __future__ import annotations

import json
from typing import Any

from rich.console import Console

from miniharness.provider import OpenAICompatibleProvider
from miniharness.tools import ToolRegistry
from miniharness.trace import TraceWriter


SYSTEM_PROMPT = """You are Miniharness, a minimal read-only local coding agent.

You can inspect the current project by using the provided tools. The tools are read-only:
- list_files lists project files.
- read_file reads one project file.
- search_text searches for text in project files.

Do not claim that you edited files, ran shell commands, used Git, browsed the web, stored memory, or contacted other agents. When you have enough information, answer the user directly.
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
        messages: list[dict[str, Any]] = [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_goal},
        ]
        tool_schemas = self.tools.openai_tools()

        for turn in range(1, self.max_turns + 1):
            self.console.print(f"[cyan]model turn {turn}[/cyan]")
            self.trace.record(
                "model_request",
                {"turn": turn, "messages": messages, "tools": tool_schemas},
            )

            response = self.provider.chat(messages=messages, tools=tool_schemas)
            self.trace.record("model_response", {"turn": turn, "response": response})

            assistant_message = self._extract_assistant_message(response)
            messages.append(assistant_message)

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
                self.trace.record(
                    "tool_call", {"turn": turn, "tool_call": tool_call}
                )

                result = self.tools.dispatch(tool_call)
                self.trace.record(
                    "tool_result",
                    {
                        "turn": turn,
                        "tool_call_id": tool_call.get("id"),
                        "name": name,
                        "result": result,
                    },
                )
                messages.append(
                    {
                        "role": "tool",
                        "tool_call_id": tool_call.get("id"),
                        "name": name,
                        "content": json.dumps(result, ensure_ascii=False),
                    }
                )

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
