from __future__ import annotations

from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from miniharness.agent import MiniAgent
from miniharness.command import CommandRuntime
from miniharness.config import Settings
from miniharness.provider import OpenAICompatibleProvider
from miniharness.tools import ToolRegistry
from miniharness.tools.command import register_command_tools
from miniharness.tools.mutation import register_mutation_tools
from miniharness.trace import TraceWriter
from miniharness.workspace import MutationRuntime


app = typer.Typer(add_completion=False, no_args_is_help=True)
console = Console()


@app.command()
def main(
    goal: Annotated[str, typer.Argument(help="User goal for the read-only agent.")],
    max_turns: Annotated[
        int,
        typer.Option(
            "--max-turns",
            "-t",
            min=1,
            max=20,
            help="Maximum number of model turns before stopping.",
        ),
    ] = 8,
) -> None:
    """Run the minimal read-only agent loop."""

    project_root = Path.cwd()
    trace = TraceWriter.create(project_root)
    trace.record(
        "user_goal",
        {
            "goal": goal,
            "project_root": str(project_root),
            "max_turns": max_turns,
        },
    )

    console.print(f"[bold]run_id[/bold] {trace.run_id}")
    console.print(f"[bold]trace[/bold] {trace.path}")

    try:
        settings = Settings.from_env()
        provider = OpenAICompatibleProvider(settings)
        tools = ToolRegistry(project_root)
        register_mutation_tools(tools, MutationRuntime(project_root, trace=trace))
        register_command_tools(tools, CommandRuntime(project_root, trace=trace))
        agent = MiniAgent(
            provider=provider,
            tools=tools,
            trace=trace,
            console=console,
            max_turns=max_turns,
        )
        final_answer = agent.run(goal)
    except Exception as exc:
        trace.record("error", {"type": type(exc).__name__, "message": str(exc)})
        console.print(f"[red]error[/red] {exc}")
        raise typer.Exit(1) from exc

    console.print(Panel(final_answer, title="final answer", border_style="green"))


if __name__ == "__main__":
    app()
