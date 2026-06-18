from __future__ import annotations

from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from miniharness.agent import MiniAgent
from miniharness.command import CommandRuntime
from miniharness.config import ProductionRuntimeConfig
from miniharness.instructions import InstructionRuntime
from miniharness.model import (
    ModelProviderRegistry,
    ModelRuntime,
    OpenAICompatibleModelProvider,
)
from miniharness.observability import TraceRuntime, TraceStore
from miniharness.policy import ApprovalGate, ApprovalMode, PolicyRuntime
from miniharness.planner import PlannerRuntime
from miniharness.tool_protocol.runtime import ToolCallingProtocolRuntime
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.tools.command import register_command_tools
from miniharness.tools.mutation import register_mutation_tools
from miniharness.tools.verification import register_verification_tools
from miniharness.tools.workspace_state import register_workspace_state_tools
from miniharness.verification import VerificationRuntime
from miniharness.workspace import MutationRuntime
from miniharness.workspace_state import (
    LocalWorkspaceStateRuntime,
    RecoveryStatus,
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
    trace = TraceRuntime.create(
        project_root,
        run_id=runtime_config.resume_session,
        session_id=runtime_config.resume_session,
        trace_dir=runtime_config.trace_dir,
    )
    trace.record(
        "user_goal",
        {
            "goal": goal,
            "project_root": str(project_root),
            "max_turns": runtime_config.max_turns,
            "resume_session": runtime_config.resume_session,
            "approval_mode": runtime_config.approval_mode.value,
            "strict": runtime_config.strict,
            "dry_run": runtime_config.dry_run,
            "raw_artifacts": runtime_config.raw_artifacts,
        },
    )

    console.print(f"[bold]run_id[/bold] {trace.run_id}")
    console.print(f"[bold]trace[/bold] {trace.store.run_dir}")

    state_runtime = LocalWorkspaceStateRuntime(project_root, trace=trace)
    recovery = state_runtime.recover_session(runtime_config.resume_session)
    resume_health: WorkspaceHealthReport | None = None
    if recovery.status != RecoveryStatus.CLEAN:
        console.print(
            f"[yellow]workspace recovery[/yellow] {recovery.status.value} "
            f"session={recovery.session_id}"
        )
    if runtime_config.resume_session:
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
        settings = runtime_config.to_settings()
        model_config = runtime_config.to_model_runtime_config()
        tools = ToolRegistry(project_root)
        model_provider = OpenAICompatibleModelProvider(
            settings,
            timeout_seconds=model_config.request_timeout_seconds,
        )
        model_registry = ModelProviderRegistry(
            default_provider_name=model_config.default_provider
        )
        model_registry.register(model_provider)
        model_runtime = ModelRuntime(
            registry=model_registry,
            tool_registry=tools,
            config=model_config,
            trace=trace,
        )
        planner = create_or_resume_planner(
            workspace_root=project_root,
            session_id=resume_session,
            task_id=trace.run_id,
            user_goal=goal,
            trace=trace,
            workspace_health=resume_health or state_runtime.get_workspace_health(),
        )
        policy_config = runtime_config.to_policy_config()
        policy_runtime = PolicyRuntime(policy_config, trace=trace)
        approval_gate = ApprovalGate(policy_config, trace=trace)
        command_runtime = CommandRuntime(
            project_root,
            trace=trace,
            state_runtime=state_runtime,
            planner=planner,
            policy_runtime=policy_runtime,
        )
        register_mutation_tools(
            tools,
            MutationRuntime(
                project_root,
                trace=trace,
                state_runtime=state_runtime,
                planner=planner,
                policy_runtime=policy_runtime,
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
                policy_runtime=policy_runtime,
            ),
        )
        tool_runtime = ToolRuntime(
            registry=tools,
            policy=ToolPolicy.coding_agent(),
            trace=trace,
            workspace_root=project_root,
            planner=planner,
            policy_runtime=policy_runtime,
            approval_gate=approval_gate,
            dry_run=runtime_config.dry_run,
        )
        protocol_runtime = ToolCallingProtocolRuntime(
            registry=tools,
            trace=trace,
            state_store=ToolProtocolStateStore(trace.store.run_dir / "tool_protocol.sqlite3"),
        )
        instruction_runtime = InstructionRuntime(workspace_root=project_root, trace=trace)
        context_db_path = runtime_config.context_db_path(trace.store.run_dir)
        agent = MiniAgent(
            model_runtime=model_runtime,
            tools=tools,
            trace=trace,
            console=console,
            max_turns=runtime_config.max_turns,
            state_runtime=state_runtime,
            planner=planner,
            policy_runtime=policy_runtime,
            approval_gate=approval_gate,
            tool_runtime=tool_runtime,
            protocol_runtime=protocol_runtime,
            instruction_runtime=instruction_runtime,
            context_db_path=context_db_path,
            strict=runtime_config.strict,
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
