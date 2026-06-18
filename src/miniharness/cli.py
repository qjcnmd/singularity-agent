from __future__ import annotations

from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from miniharness.config import ProductionRuntimeConfig
from miniharness.kernel import CancellationError, KernelBootstrap
from miniharness.observability import TraceStore
from miniharness.policy import ApprovalMode
from miniharness.planner import PlannerRuntime
from miniharness.workspace_state import (
    WorkspaceHealthReport,
)


app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="production-grade local CLI coding agent harness",
)
trace_app = typer.Typer(add_completion=False, no_args_is_help=True)
app.add_typer(trace_app, name="trace")
console = Console()


@app.command()
def main(
    goal: Annotated[
        str,
        typer.Argument(help="User goal for the production-grade local CLI coding agent harness."),
    ],
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
    profile: Annotated[
        str | None,
        typer.Option(
            "--profile",
            help="Label this run with a runtime profile name for trace and final report summaries.",
        ),
    ] = None,
    resume_session: Annotated[
        str | None,
        typer.Option(
            "--resume",
            "--resume-session",
            help="Resume a PlannerRuntime, context, protocol, and workspace state session by id.",
        ),
    ] = None,
    approval_mode: Annotated[
        ApprovalMode,
        typer.Option(
            "--approval-mode",
            case_sensitive=False,
            help="Runtime approval mode: interactive, review_all, auto_safe, read_only, or non_interactive.",
        ),
    ] = ApprovalMode.AUTO_SAFE,
    trace_dir: Annotated[
        Path | None,
        typer.Option(
            "--trace-dir",
            help="Directory that contains trace run/session directories.",
        ),
    ] = None,
    context_db: Annotated[
        Path | None,
        typer.Option(
            "--context-db",
            help="Exact ContextStore SQLite path; defaults to the trace run directory.",
        ),
    ] = None,
    model: Annotated[
        str | None,
        typer.Option(
            "--model",
            help="Override MINIHARNESS_MODEL for this session.",
        ),
    ] = None,
    base_url: Annotated[
        str | None,
        typer.Option(
            "--base-url",
            help="Override MINIHARNESS_BASE_URL for this session.",
        ),
    ] = None,
    raw_artifacts: Annotated[
        bool,
        typer.Option(
            "--raw-artifacts/--no-raw-artifacts",
            help="Store redacted raw model response artifacts.",
        ),
    ] = False,
    dry_run: Annotated[
        bool,
        typer.Option(
            "--dry-run",
            help="Block mutation, command, verification, and other side-effect tools before execution.",
        ),
    ] = False,
    strict: Annotated[
        bool,
        typer.Option(
            "--strict",
            help="Enable strict tool schema/protocol validation and redaction hardening.",
        ),
    ] = False,
) -> None:
    """Run the production-grade local CLI coding agent harness."""

    project_root = Path.cwd()
    runtime_config = ProductionRuntimeConfig.from_cli(
        project_root=project_root,
        max_turns=max_turns,
        profile=profile,
        approval_mode=approval_mode,
        strict=strict,
        dry_run=dry_run,
        trace_dir=trace_dir,
        context_db=context_db,
        model=model,
        base_url=base_url,
        raw_artifacts=raw_artifacts,
        resume_session=resume_session,
    )
    kernel = None
    try:
        kernel = KernelBootstrap(
            project_root=project_root,
            config=runtime_config,
            console=console,
        ).boot(goal)
        kernel.graph.trace.record(
            "user_goal",
            {
                "goal": goal,
                "project_root": str(project_root),
                "max_turns": runtime_config.max_turns,
                "profile": runtime_config.profile,
                "resume_session": runtime_config.resume_session,
                "approval_mode": runtime_config.approval_mode.value,
                "strict": runtime_config.strict,
                "dry_run": runtime_config.dry_run,
                "raw_artifacts": runtime_config.raw_artifacts,
            },
        )
        console.print(f"[bold]run_id[/bold] {kernel.context.identity.run_id}")
        console.print(f"[bold]trace[/bold] {kernel.graph.trace.store.run_dir}")
        if kernel.recovery_report and kernel.recovery_report.recovered:
            console.print(
                "[yellow]workspace recovery[/yellow] "
                + json_dumps(kernel.recovery_report.to_dict())
            )
        if kernel.graph.workspace_state.baseline is not None:
            baseline = kernel.graph.workspace_state.baseline
            console.print(
                f"[bold]workspace baseline[/bold] {baseline.baseline_id} "
                f"files={len(baseline.snapshots)}"
            )
        result = kernel.run_task(goal)
        final_answer = result.final_answer
        final_report = result.final_report
        final_health = kernel.graph.workspace_state.get_workspace_health()
    except Exception as exc:
        if isinstance(exc, CancellationError):
            console.print(f"[yellow]cancelled[/yellow] {exc}")
        else:
            console.print(f"[red]error[/red] {exc}")
        report = getattr(exc, "final_report", None)
        if report is not None:
            console.print(
                Panel(
                    json_dumps(report.to_dict()),
                    title="final report",
                    border_style="yellow",
                )
            )
        elif kernel is not None:
            try:
                console.print(
                    Panel(
                        json_dumps(kernel.final_report().to_dict()),
                        title="final report",
                        border_style="yellow",
                    )
                )
            except Exception as report_exc:
                console.print(f"[yellow]final report unavailable[/yellow] {report_exc}")
        raise typer.Exit(1) from exc

    console.print(Panel(final_answer, title="final answer", border_style="green"))
    console.print(Panel(json_dumps(final_report.to_dict()), title="final report", border_style="green"))
    console.print(_workspace_health_panel(final_health))


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
    trace: TraceRuntime | None,
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


@trace_app.command("list")
def trace_list(
    trace_dir: Annotated[
        Path | None,
        typer.Option("--trace-dir", help="Directory that contains trace run/session directories."),
    ] = None,
) -> None:
    """List local structured trace runs."""

    traces_root = trace_dir or (Path.cwd() / "work" / "traces" / "runs")
    if not traces_root.exists():
        console.print("No trace runs found.")
        return
    for run_dir in sorted(path for path in traces_root.iterdir() if path.is_dir()):
        console.print(run_dir.name)


@trace_app.command("show")
def trace_show(
    run_id: str,
    trace_dir: Annotated[
        Path | None,
        typer.Option("--trace-dir", help="Directory that contains trace run/session directories."),
    ] = None,
) -> None:
    """Show a trace run summary."""

    store = TraceStore(Path.cwd(), run_id=run_id, trace_dir=trace_dir)
    summary = store.summarize(run_id=run_id).to_dict()
    console.print(json_dumps(summary))


@trace_app.command("timeline")
def trace_timeline(
    run_id: str,
    trace_dir: Annotated[
        Path | None,
        typer.Option("--trace-dir", help="Directory that contains trace run/session directories."),
    ] = None,
) -> None:
    """Show a trace run timeline."""

    store = TraceStore(Path.cwd(), run_id=run_id, trace_dir=trace_dir)
    for item in store.get_timeline(run_id=run_id):
        console.print(
            f"{item.timestamp.isoformat()} {item.event_type} "
            f"[{item.runtime}] {item.summary}"
        )


@trace_app.command("errors")
def trace_errors(
    run_id: str,
    trace_dir: Annotated[
        Path | None,
        typer.Option("--trace-dir", help="Directory that contains trace run/session directories."),
    ] = None,
) -> None:
    """Show warning/error/critical events for a trace run."""

    store = TraceStore(Path.cwd(), run_id=run_id, trace_dir=trace_dir)
    for event in store.query_events(run_id=run_id):
        if event.severity.value in {"warning", "error", "critical"}:
            console.print(
                f"{event.timestamp.isoformat()} {event.event_type.value} "
                f"[{event.severity.value}] {event.summary}"
            )


@trace_app.command("artifacts")
def trace_artifacts(
    run_id: str,
    trace_dir: Annotated[
        Path | None,
        typer.Option("--trace-dir", help="Directory that contains trace run/session directories."),
    ] = None,
) -> None:
    """List artifacts for a trace run."""

    store = TraceStore(Path.cwd(), run_id=run_id, trace_dir=trace_dir)
    for artifact in store.artifacts():
        console.print(
            f"{artifact.artifact_id} {artifact.kind.value} "
            f"{artifact.size_bytes} bytes {artifact.relative_path}"
        )


def json_dumps(payload: object) -> str:
    import json

    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str)


if __name__ == "__main__":
    app()
