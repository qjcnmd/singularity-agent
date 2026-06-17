from __future__ import annotations

from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from miniharness.agent import MiniAgent
from miniharness.command import CommandRuntime
from miniharness.config import Settings
from miniharness.planner import PlannerRuntime
from miniharness.provider import OpenAICompatibleProvider
from miniharness.tools import ToolRegistry
from miniharness.tools.command import register_command_tools
from miniharness.tools.mutation import register_mutation_tools
from miniharness.tools.verification import register_verification_tools
from miniharness.tools.workspace_state import register_workspace_state_tools
from miniharness.trace import TraceWriter
from miniharness.verification import VerificationRuntime
from miniharness.workspace import MutationRuntime
from miniharness.workspace_state import (
    LocalWorkspaceStateRuntime,
    RecoveryStatus,
    WorkspaceHealthReport,
)


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
    resume_session: Annotated[
        str | None,
        typer.Option(
            "--resume-session",
            help="Resume a PlannerRuntime and workspace state session by id.",
        ),
    ] = None,
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
            "resume_session": resume_session,
        },
    )

    console.print(f"[bold]run_id[/bold] {trace.run_id}")
    console.print(f"[bold]trace[/bold] {trace.path}")

    state_runtime = LocalWorkspaceStateRuntime(project_root, trace=trace)
    recovery = state_runtime.recover_session(resume_session)
    resume_health: WorkspaceHealthReport | None = None
    if recovery.status != RecoveryStatus.CLEAN:
        console.print(
            f"[yellow]workspace recovery[/yellow] {recovery.status.value} "
            f"session={recovery.session_id}"
        )
    if resume_session:
        resume_health = state_runtime.get_workspace_health()
        if state_runtime.baseline is not None:
            console.print(
                f"[bold]workspace baseline[/bold] {state_runtime.baseline.baseline_id} "
                f"files={len(state_runtime.baseline.snapshots)}"
            )
    else:
        baseline = state_runtime.begin_session(task_id=trace.run_id, session_id=trace.run_id)
        console.print(
            f"[bold]workspace baseline[/bold] {baseline.baseline_id} "
            f"files={len(baseline.snapshots)}"
        )
    session_status = "interrupted"
    final_health: WorkspaceHealthReport | None = None
    try:
        settings = Settings.from_env()
        provider = OpenAICompatibleProvider(settings)
        tools = ToolRegistry(project_root)
        planner = create_or_resume_planner(
            workspace_root=project_root,
            session_id=resume_session,
            task_id=trace.run_id,
            user_goal=goal,
            trace=trace,
            workspace_health=resume_health or state_runtime.get_workspace_health(),
        )
        command_runtime = CommandRuntime(
            project_root,
            trace=trace,
            state_runtime=state_runtime,
            planner=planner,
        )
        register_mutation_tools(
            tools,
            MutationRuntime(
                project_root,
                trace=trace,
                state_runtime=state_runtime,
                planner=planner,
            ),
        )
        register_command_tools(tools, command_runtime)
        register_workspace_state_tools(tools, state_runtime)
        register_verification_tools(
            tools,
            VerificationRuntime(
                project_root,
                command_runtime=command_runtime,
                trace=trace,
                planner=planner,
            ),
        )
        agent = MiniAgent(
            provider=provider,
            tools=tools,
            trace=trace,
            console=console,
            max_turns=max_turns,
            state_runtime=state_runtime,
            planner=planner,
        )
        final_answer = agent.run(goal)
        state_runtime.record_external_changes()
        final_health = state_runtime.get_workspace_health()
        session_status = "closed"
    except Exception as exc:
        trace.record("error", {"type": type(exc).__name__, "message": str(exc)})
        console.print(f"[red]error[/red] {exc}")
        raise typer.Exit(1) from exc
    finally:
        state_runtime.close_session(status=session_status)

    console.print(Panel(final_answer, title="final answer", border_style="green"))
    console.print(_workspace_health_panel(final_health or state_runtime.get_workspace_health()))


def workspace_health_summary(health: WorkspaceHealthReport) -> str:
    payload = health.to_dict()
    lines = [
        f"status: {payload['status']}",
        f"agent_changes: {_format_list(payload['agent_changes'])}",
        f"command_side_effects: {_format_list(payload['command_side_effects'])}",
        f"external_changes: {_format_list(payload['external_changes'])}",
        f"rollback_available: {str(payload['rollback_available']).lower()}",
        f"rollback_conflicts: {_format_list(payload['rollback_conflicts'])}",
        f"recommended_next_action: {payload['recommended_next_action']}",
    ]
    return "\n".join(lines)


def create_or_resume_planner(
    *,
    workspace_root: Path,
    session_id: str | None,
    task_id: str,
    user_goal: str,
    trace: TraceWriter | None,
    workspace_health: WorkspaceHealthReport,
) -> PlannerRuntime:
    planner = PlannerRuntime(
        workspace_root,
        session_id=session_id or task_id,
        task_id=task_id,
        trace=trace,
    )
    if session_id:
        return planner.resume(
            session_id,
            workspace_health=workspace_health.to_dict(),
        )
    planner.start_task(user_goal)
    return planner


def _workspace_health_panel(health: WorkspaceHealthReport) -> Panel:
    return Panel(
        workspace_health_summary(health),
        title="workspace state",
        border_style="blue",
    )


def _format_list(values: list[str]) -> str:
    return ", ".join(values) if values else "-"


if __name__ == "__main__":
    app()
