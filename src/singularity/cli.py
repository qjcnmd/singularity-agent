from __future__ import annotations

import sys
import subprocess
from pathlib import Path
from typing import Annotated
from uuid import uuid4

import click
import typer
from rich.console import Console
from rich.panel import Panel

from singularity.code_index import ProjectIndexRuntime
from singularity.config import ProductionRuntimeConfig, adaptive_default_max_turns
from singularity.evaluation import (
    EvaluationProfile,
    EvaluationRuntime,
    GoldenTaskStore,
    RegressionDetector,
    TraceReplayRuntime,
)
from singularity.git_runtime.cli import git_app
from singularity.interaction import RichCliRenderer
from singularity.kernel import CancellationError, KernelBootstrap
from singularity.kernel.models import RunStatus
from singularity.memory.cli import memory_app
from singularity.observability import TraceRedactor, TraceRuntime, TraceStore
from singularity.plugins.cli import plugin_app
from singularity.command import CommandRuntime
from singularity.policy import ApprovalMode, PolicyConfig, PolicyRuntime, SecurityMode
from singularity.policy.cli import approval_app
from singularity.planner import PlannerRuntime, create_or_resume_planner as _create_or_resume_planner
from singularity.diagnostics import DoctorEngine, RepairEngine
from singularity.diagnostics.render import render_diagnostic_result, render_repair_plan
from singularity.release.init import initialize_runtime
from singularity.release.metadata import version_info
from singularity.release.migrations import apply_migrations
from singularity.release.paths import resolve_runtime_paths
from singularity.release.repair import (
    export_user_data,
    repair_runtime,
    uninstall_runtime,
)
from singularity.sandbox import SandboxRuntime
from singularity.verification import VerificationRuntime
from singularity.workspace_state import (
    WorkspaceHealthReport,
)


class _SingularityGroup(typer.core.TyperGroup):
    def resolve_command(self, ctx, args):
        try:
            command_name, command, remaining = super().resolve_command(ctx, args)
            if command is None and args and not str(args[0]).startswith("-"):
                return super().resolve_command(ctx, ["run", *args])
            return command_name, command, remaining
        except Exception as exc:
            if not _is_click_usage_error(exc):
                raise
            if args and not str(args[0]).startswith("-"):
                return super().resolve_command(ctx, ["run", *args])
            raise


app = typer.Typer(
    cls=_SingularityGroup,
    add_completion=False,
    no_args_is_help=True,
    help="production-oriented local CLI coding agent runtime",
)
trace_app = typer.Typer(add_completion=False, no_args_is_help=True)
index_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_task_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_suite_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_trace_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_ab_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_regression_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_report_app = typer.Typer(add_completion=False, no_args_is_help=True)
eval_live_app = typer.Typer(add_completion=False, no_args_is_help=True)
system_app = typer.Typer(add_completion=False, no_args_is_help=True)
app.add_typer(trace_app, name="trace")
app.add_typer(index_app, name="index")
app.add_typer(git_app, name="git")
app.add_typer(approval_app, name="approval")
app.add_typer(memory_app, name="memory")
app.add_typer(plugin_app, name="plugin")
app.add_typer(eval_app, name="eval")
app.add_typer(eval_app, name="benchmark")
app.add_typer(system_app, name="system")
eval_app.add_typer(eval_task_app, name="task")
eval_app.add_typer(eval_suite_app, name="suite")
eval_app.add_typer(eval_trace_app, name="trace")
eval_app.add_typer(eval_ab_app, name="ab")
eval_app.add_typer(eval_regression_app, name="regression")
eval_app.add_typer(eval_report_app, name="report")
eval_app.add_typer(eval_live_app, name="live")
console = Console()
_REDACTOR = TraceRedactor()


def _is_click_usage_error(exc: Exception) -> bool:
    if isinstance(exc, click.UsageError):
        return True
    cls = type(exc)
    return cls.__name__ in {"UsageError", "NoSuchCommand"} and "click" in cls.__module__


def _cli_overrides(names: list[str]) -> set[str] | None:
    ctx = click.get_current_context(silent=True)
    if ctx is None:
        return None
    overrides: set[str] = set()
    for name in names:
        try:
            source = ctx.get_parameter_source(name)
        except Exception:
            continue
        if source == click.core.ParameterSource.COMMANDLINE:
            overrides.add(name)
    return overrides


@app.command("run")
@app.command("main", hidden=True)
def run_goal(
    goal: Annotated[
        str,
        typer.Argument(help="User goal for the production-oriented local CLI coding agent runtime."),
    ],
    max_turns: Annotated[
        int | None,
        typer.Option(
            "--max-turns",
            "-t",
            min=1,
            max=20,
            help="Maximum number of model turns before stopping; overrides the adaptive default.",
        ),
    ] = None,
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
    project_index_enabled: Annotated[
        bool | None,
        typer.Option(
            "--project-index/--no-project-index",
            help="Enable ProjectIndexRuntime bootstrap and context/planner observations.",
        ),
    ] = None,
    project_index_db: Annotated[
        Path | None,
        typer.Option("--project-index-db", help="Exact ProjectIndexRuntime SQLite path."),
    ] = None,
    project_index_build_on_boot: Annotated[
        bool | None,
        typer.Option(
            "--project-index-build-on-boot/--no-project-index-build-on-boot",
            help="Build or refresh the project index during kernel boot.",
        ),
    ] = None,
    approval_mode: Annotated[
        ApprovalMode | None,
        typer.Option(
            "--approval-mode",
            case_sensitive=False,
            help="Runtime approval mode: interactive, review_all, auto_safe, read_only, or non_interactive.",
        ),
    ] = None,
    security_mode: Annotated[
        SecurityMode | None,
        typer.Option(
            "--security-mode",
            case_sensitive=False,
            help="Runtime security mode: strict fails closed by default; compat preserves legacy local execution behavior.",
        ),
    ] = None,
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
            help="Override SINGULARITY_MODEL for this session.",
        ),
    ] = None,
    base_url: Annotated[
        str | None,
        typer.Option(
            "--base-url",
            help="Override SINGULARITY_BASE_URL for this session.",
        ),
    ] = None,
    raw_artifacts: Annotated[
        bool | None,
        typer.Option(
            "--raw-artifacts/--no-raw-artifacts",
            help="Store redacted raw model response artifacts.",
        ),
    ] = None,
    dry_run: Annotated[
        bool | None,
        typer.Option(
            "--dry-run",
            help="Block mutation, command, verification, and other side-effect tools before execution.",
        ),
    ] = None,
    strict: Annotated[
        bool | None,
        typer.Option(
            "--strict",
            help="Enable strict tool schema/protocol validation and redaction hardening.",
        ),
    ] = None,
    project_root: Annotated[
        Path | None,
        typer.Option(
            "--project-root",
            help="Workspace root for this run; defaults to the current directory.",
        ),
    ] = None,
) -> None:
    """Run the production-oriented local CLI coding agent runtime."""

    project_root = (project_root or Path.cwd()).expanduser().resolve(strict=False)
    runtime_config = ProductionRuntimeConfig.from_cli(
        project_root=project_root,
        max_turns=max_turns,
        profile=profile,
        approval_mode=approval_mode,
        security_mode=security_mode,
        strict=strict,
        dry_run=dry_run,
        trace_dir=trace_dir,
        context_db=context_db,
        model=model,
        base_url=base_url,
        raw_artifacts=raw_artifacts,
        resume_session=resume_session,
        project_index_enabled=project_index_enabled,
        project_index_db=project_index_db,
        project_index_build_on_boot=project_index_build_on_boot,
        default_max_turns=adaptive_default_max_turns(goal),
        cli_overrides=_cli_overrides(
            [
                "max_turns",
                "profile",
                "resume_session",
                "project_index_enabled",
                "project_index_db",
                "project_index_build_on_boot",
                "approval_mode",
                "security_mode",
                "trace_dir",
                "context_db",
                "model",
                "base_url",
                "raw_artifacts",
                "dry_run",
                "strict",
            ]
        ),
    )
    kernel = None
    renderer = RichCliRenderer(console)
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
                "security_mode": runtime_config.security_mode.value,
                "strict": runtime_config.strict,
                "dry_run": runtime_config.dry_run,
                "raw_artifacts": runtime_config.raw_artifacts,
                "project_index_enabled": runtime_config.project_index_enabled,
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
            console.print(f"[yellow]cancelled[/yellow] {_REDACTOR.redact_text(str(exc))}")
        else:
            console.print(f"[red]error[/red] {_REDACTOR.redact_text(str(exc))}")
        report = getattr(exc, "final_report", None)
        if report is not None:
            renderer.render_final_report(report, border_style="yellow")
        elif kernel is not None:
            try:
                interaction_report = (
                    kernel.interaction_final_report()
                    if hasattr(kernel, "interaction_final_report")
                    else None
                )
                renderer.render_final_report(
                    interaction_report or kernel.final_report(),
                    border_style="yellow",
                )
            except Exception as report_exc:
                console.print(
                    "[yellow]final report unavailable[/yellow] "
                    + _REDACTOR.redact_text(str(report_exc))
                )
        close_resources = getattr(kernel, "close_resources", None) if kernel is not None else None
        if callable(close_resources):
            close_resources()
        raise typer.Exit(1) from exc

    try:
        console.print(Panel(final_answer, title="final answer", border_style="green"))
        renderer.render_final_report(
            getattr(result, "interaction_report", None) or final_report,
            border_style="green",
        )
        console.print(_workspace_health_panel(final_health))
    finally:
        close_resources = getattr(kernel, "close_resources", None) if kernel is not None else None
        if callable(close_resources):
            close_resources()


@app.command("version")
def version_command(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Print Singularity version and installation information."""

    paths = resolve_runtime_paths(mode=mode, home=home, project_root=Path.cwd())
    info = version_info(paths)
    if json_output:
        _write_stdout(json_dumps(info.to_dict()))
        return
    console.print(f"Singularity {info.version}")
    console.print(f"Python {info.python_version}")
    console.print(f"platform: {info.platform}")
    console.print(f"install_path: {info.install_path}")
    console.print(f"runtime_dir: {info.runtime_dir}")
    console.print(f"config_dir: {info.config_dir}")


@app.command("doctor")
def doctor_command(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    check_id: Annotated[
        str | None,
        typer.Option("--check", help="Run one stable diagnostic check id."),
    ] = None,
    group: Annotated[
        str | None,
        typer.Option("--group", help="Run one diagnostic group."),
    ] = None,
    verbose: Annotated[
        bool,
        typer.Option("--verbose", help="Show technical detail and suggested fixes."),
    ] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Diagnose installed CLI and runtime directory health without modifying data."""

    project_root = Path.cwd()
    report = DoctorEngine.default().run(
        paths=resolve_runtime_paths(mode=mode, home=home, project_root=project_root),
        project_root=project_root,
        check_id=check_id,
        group=group,
    )
    if json_output:
        _write_stdout(report.to_json())
    else:
        render_diagnostic_result(console, report, verbose=verbose)
    if not report.ok:
        raise typer.Exit(1)


@app.command("repair")
def repair_command(
    dry_run: Annotated[
        bool,
        typer.Option("--dry-run", help="Show repair actions without changing local state."),
    ] = False,
    apply_changes: Annotated[
        bool,
        typer.Option("--apply", help="Apply low-risk repair actions."),
    ] = False,
    check_id: Annotated[
        str | None,
        typer.Option("--check", help="Only repair findings from one stable check id."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Plan or apply safe local runtime repairs."""

    if dry_run and apply_changes:
        raise typer.BadParameter("Use either --dry-run or --apply, not both.")
    project_root = Path.cwd()
    paths = resolve_runtime_paths(mode=mode, home=home, project_root=project_root)
    before = DoctorEngine.default().run(paths=paths, project_root=project_root, check_id=check_id)
    plan = RepairEngine().run(before, paths=paths, project_root=project_root, apply=apply_changes)
    if not apply_changes:
        payload = _repair_result_payload(plan.to_dict(), after=None, ok=True)
        if json_output:
            _write_stdout(json_dumps(payload))
        else:
            render_repair_plan(console, payload)
        return

    after = DoctorEngine.default().run(paths=paths, project_root=project_root, check_id=check_id)
    action_failed = any(action.status == "failed" for action in plan.actions)
    payload = _repair_result_payload(
        plan.to_dict(),
        after=after.to_dict(),
        ok=after.ok and not action_failed,
    )
    if json_output:
        _write_stdout(json_dumps(payload))
    else:
        render_repair_plan(console, payload)
        render_diagnostic_result(console, after, verbose=True)
    if not payload["ok"]:
        raise typer.Exit(1)


@system_app.command("init")
def system_init(
    force: Annotated[bool, typer.Option("--force", help="Overwrite default config and manifest.")] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Initialize user-level Singularity runtime directories and defaults."""

    result = initialize_runtime(
        resolve_runtime_paths(mode=mode, home=home, project_root=Path.cwd()),
        force=force,
    )
    _print_release_payload(result, json_output=json_output, title="runtime initialized")


@system_app.command("migrate")
def system_migrate(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Apply pending release/runtime migrations with backup and rollback."""

    result = {
        "applied": apply_migrations(resolve_runtime_paths(mode=mode, home=home, project_root=Path.cwd()))
    }
    _print_release_payload(result, json_output=json_output, title="migrations")


@system_app.command("repair")
def system_repair(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Repair missing runtime directories and default files without overwriting data."""

    result = repair_runtime(resolve_runtime_paths(mode=mode, home=home, project_root=Path.cwd()))
    _print_release_payload(result, json_output=json_output, title="repair")


@system_app.command("uninstall")
def system_uninstall(
    dry_run: Annotated[bool, typer.Option("--dry-run", help="List paths without deleting.")] = False,
    purge_user_data: Annotated[
        bool,
        typer.Option("--purge-user-data", help="Allow deletion of memory, traces, eval data, and logs."),
    ] = False,
    yes: Annotated[bool, typer.Option("--yes", "-y", help="Confirm destructive purge.")] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Remove Singularity runtime-managed files with protected user data defaults."""

    if purge_user_data and not dry_run and not yes:
        confirmed = typer.confirm("Delete Singularity user data including memory, traces, eval data, and logs?")
        if not confirmed:
            raise typer.Abort()
    result = uninstall_runtime(
        resolve_runtime_paths(mode=mode, home=home, project_root=Path.cwd()),
        dry_run=dry_run,
        purge_user_data=purge_user_data,
    )
    _print_release_payload(result, json_output=json_output, title="uninstall")
    if result.get("blocked") and not dry_run:
        raise typer.Exit(2)


@system_app.command("export")
def system_export(
    output: Annotated[Path, typer.Option("--output", help="Output zip file path.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    mode: Annotated[
        str | None,
        typer.Option("--mode", help="Runtime mode: user, development, or portable."),
    ] = None,
    home: Annotated[
        Path | None,
        typer.Option("--home", help="Override runtime root for this command."),
    ] = None,
) -> None:
    """Export Singularity user data into a portable zip archive."""

    result = export_user_data(
        resolve_runtime_paths(mode=mode, home=home, project_root=Path.cwd()),
        output,
    )
    _print_release_payload(result, json_output=json_output, title="export")


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
    return _create_or_resume_planner(
        workspace_root=workspace_root,
        session_id=session_id,
        task_id=task_id,
        user_goal=user_goal,
        trace=trace,
        workspace_health=workspace_health,
    )


def _workspace_health_panel(health: WorkspaceHealthReport) -> Panel:
    return Panel(
        workspace_health_summary(health),
        title="workspace state",
        border_style="blue",
    )


def _format_list(values: list[str]) -> str:
    return ", ".join(values) if values else "-"


@index_app.command("build")
def index_build(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    db_path: Annotated[Path | None, typer.Option("--db", help="Project index SQLite path.")] = None,
) -> None:
    """Build the ProjectIndexRuntime SQLite index."""

    runtime = ProjectIndexRuntime(Path.cwd(), db_path=db_path)
    summary = runtime.build_full_index(reason="cli_build").to_dict()
    _print_index_payload(summary, json_output=json_output, title="project index")


@index_app.command("refresh")
def index_refresh(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    db_path: Annotated[Path | None, typer.Option("--db", help="Project index SQLite path.")] = None,
) -> None:
    """Refresh the ProjectIndexRuntime index incrementally when possible."""

    runtime = ProjectIndexRuntime(Path.cwd(), db_path=db_path)
    summary = runtime.refresh(reason="cli_refresh").to_dict()
    _print_index_payload(summary, json_output=json_output, title="project index")


@index_app.command("explain")
def index_explain(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    db_path: Annotated[Path | None, typer.Option("--db", help="Project index SQLite path.")] = None,
) -> None:
    """Explain indexed project structure and limitations."""

    runtime = ProjectIndexRuntime(Path.cwd(), db_path=db_path)
    runtime.bootstrap(reason="cli_explain")
    _print_index_payload(runtime.explain(), json_output=json_output, title="project index explain")


@index_app.command("relevant")
def index_relevant(
    goal: Annotated[str, typer.Argument(help="Goal or query used to rank relevant files.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    db_path: Annotated[Path | None, typer.Option("--db", help="Project index SQLite path.")] = None,
) -> None:
    """Rank relevant files for a goal."""

    runtime = ProjectIndexRuntime(Path.cwd(), db_path=db_path)
    runtime.bootstrap(reason="cli_relevant")
    payload = {"relevant_files": [item.to_dict() for item in runtime.find_relevant_files(goal)]}
    _print_index_payload(payload, json_output=json_output, title="project index relevant")


@index_app.command("impact")
def index_impact(
    paths: Annotated[list[str], typer.Argument(help="Workspace-relative paths to analyze.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    db_path: Annotated[Path | None, typer.Option("--db", help="Project index SQLite path.")] = None,
) -> None:
    """Analyze code-index impact for paths."""

    runtime = ProjectIndexRuntime(Path.cwd(), db_path=db_path)
    runtime.bootstrap(reason="cli_impact")
    _print_index_payload(runtime.analyze_impact(paths).to_dict(), json_output=json_output, title="project index impact")


@index_app.command("tests")
def index_tests(
    paths: Annotated[list[str], typer.Argument(help="Changed workspace-relative paths.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    db_path: Annotated[Path | None, typer.Option("--db", help="Project index SQLite path.")] = None,
) -> None:
    """Return test impact for changed paths."""

    runtime = ProjectIndexRuntime(Path.cwd(), db_path=db_path)
    runtime.bootstrap(reason="cli_tests")
    _print_index_payload(runtime.get_test_impact(paths).to_dict(), json_output=json_output, title="project index tests")


def _print_index_payload(payload: object, *, json_output: bool, title: str) -> None:
    text = json_dumps(payload)
    if json_output:
        _write_stdout(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))


@eval_task_app.command("validate")
def eval_task_validate(
    task_set: Annotated[Path, typer.Argument(help="Golden task set JSON/YAML path.")],
    version: Annotated[str | None, typer.Option("--version", help="Only validate this task version.")] = None,
    tag: Annotated[
        list[str] | None,
        typer.Option("--tag", help="Require a tag; repeat for multiple tags."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    """Validate a local Golden Task Set document."""

    tasks = GoldenTaskStore(task_set).load(version=version, tags=tag or None)
    payload = {"task_count": len(tasks), "task_ids": [task.task_id for task in tasks]}
    _print_eval_payload(payload, json_output=json_output, title="evaluation task validation")


@eval_task_app.command("list")
def eval_task_list(
    task_set: Annotated[Path, typer.Argument(help="Golden task set JSON/YAML path.")],
    version: Annotated[str | None, typer.Option("--version", help="Only list this task version.")] = None,
    tag: Annotated[
        list[str] | None,
        typer.Option("--tag", help="Require a tag; repeat for multiple tags."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    """List Golden Task Set tasks with optional version/tag filters."""

    tasks = GoldenTaskStore(task_set).load(version=version, tags=tag or None)
    payload = {
        "tasks": [
            {
                "task_id": task.task_id,
                "version": task.version,
                "title": task.title,
                "tags": task.tags,
            }
            for task in tasks
        ]
    }
    _print_eval_payload(payload, json_output=json_output, title="evaluation tasks")


@eval_suite_app.command("run")
def eval_suite_run(
    task_set: Annotated[Path, typer.Argument(help="Golden task set JSON/YAML path.")],
    profile_json: Annotated[
        list[str] | None,
        typer.Option(
            "--profile-json",
            help="EvaluationProfile JSON object; repeat for A/B or multi-profile runs.",
        ),
    ] = None,
    trace_run_dir: Annotated[
        Path | None,
        typer.Option("--trace-run-dir", help="Existing trace run directory to replay."),
    ] = None,
    output_dir: Annotated[
        Path | None,
        typer.Option("--output-dir", help="Evaluation output root; defaults to work/evaluations."),
    ] = None,
    run_id: Annotated[str | None, typer.Option("--run-id", help="Stable evaluation run id.")] = None,
    execute: Annotated[
        bool,
        typer.Option(
            "--execute/--no-execute",
            help=(
                "Run executable hooks/tests through CommandRuntime and "
                "VerificationRuntime. Defaults to deterministic offline scoring."
            ),
        ),
    ] = False,
    version: Annotated[str | None, typer.Option("--version", help="Only run this task version.")] = None,
    tag: Annotated[
        list[str] | None,
        typer.Option("--tag", help="Require a tag; repeat for multiple tags."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print report JSON.")] = False,
) -> None:
    """Run a benchmark suite against one or more fixed profiles."""

    tasks = GoldenTaskStore(task_set).load(version=version, tags=tag or None)
    profiles = _profiles_from_cli(profile_json)
    runtime = _evaluation_runtime_from_cli(
        project_root=Path.cwd(),
        output_root=output_dir,
        run_id=run_id,
        execute=execute,
    )
    report = runtime.run_suite(
        tasks=tasks,
        profiles=profiles,
        trace_run_dir=trace_run_dir,
        run_id=run_id,
        write_report=True,
        execute=execute,
    )
    _print_report(report, json_output=json_output)


@eval_trace_app.command("replay")
def eval_trace_replay(
    trace_run_dir: Annotated[Path, typer.Argument(help="Existing trace run directory.")],
    profile_json: Annotated[
        str | None,
        typer.Option("--profile-json", help="EvaluationProfile JSON object."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    """Replay a stored trace deterministically under a fixed evaluation profile."""

    profile = _profiles_from_cli([profile_json] if profile_json else None)[0]
    result = TraceReplayRuntime(project_root=Path.cwd()).replay(trace_run_dir, profile=profile)
    _print_eval_payload(result.to_dict(), json_output=json_output, title="trace replay")


@eval_ab_app.command("run")
def eval_ab_run(
    task_set: Annotated[Path, typer.Argument(help="Golden task set JSON/YAML path.")],
    baseline_profile_json: Annotated[
        str,
        typer.Option("--baseline-profile-json", help="Baseline EvaluationProfile JSON object."),
    ],
    candidate_profile_json: Annotated[
        str,
        typer.Option("--candidate-profile-json", help="Candidate EvaluationProfile JSON object."),
    ],
    trace_run_dir: Annotated[
        Path | None,
        typer.Option("--trace-run-dir", help="Existing trace run directory to replay."),
    ] = None,
    output_dir: Annotated[
        Path | None,
        typer.Option("--output-dir", help="Evaluation output root; defaults to work/evaluations."),
    ] = None,
    run_id: Annotated[str | None, typer.Option("--run-id", help="Stable evaluation run id.")] = None,
    execute: Annotated[
        bool,
        typer.Option(
            "--execute/--no-execute",
            help=(
                "Run executable hooks/tests through CommandRuntime and "
                "VerificationRuntime. Defaults to deterministic offline scoring."
            ),
        ),
    ] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print report JSON.")] = False,
) -> None:
    """Run an A/B evaluation for model, prompt, memory, or tool-policy profiles."""

    import json

    baseline = EvaluationProfile.from_dict(json.loads(baseline_profile_json))
    candidate = EvaluationProfile.from_dict(json.loads(candidate_profile_json))
    runtime = _evaluation_runtime_from_cli(
        project_root=Path.cwd(),
        output_root=output_dir,
        run_id=run_id,
        execute=execute,
    )
    report = runtime.run_ab(
        tasks=GoldenTaskStore(task_set).load(),
        baseline=baseline,
        candidate=candidate,
        trace_run_dir=trace_run_dir,
        run_id=run_id,
        write_report=True,
        execute=execute,
    )
    _print_report(report, json_output=json_output)


@eval_regression_app.command("run")
def eval_regression_run(
    task_set: Annotated[Path, typer.Argument(help="Golden task set JSON/YAML path.")],
    baseline_profile_json: Annotated[
        str,
        typer.Option("--baseline-profile-json", help="Baseline EvaluationProfile JSON object."),
    ],
    candidate_profile_json: Annotated[
        str,
        typer.Option("--candidate-profile-json", help="Candidate EvaluationProfile JSON object."),
    ],
    trace_run_dir: Annotated[
        Path | None,
        typer.Option("--trace-run-dir", help="Existing trace run directory to replay."),
    ] = None,
    threshold: Annotated[
        float,
        typer.Option("--threshold", min=0.0, max=1.0, help="Regression threshold."),
    ] = 0.05,
    block_on_regression: Annotated[
        bool,
        typer.Option("--block-on-regression", help="Exit non-zero if regressions exceed threshold."),
    ] = False,
    output_dir: Annotated[
        Path | None,
        typer.Option("--output-dir", help="Evaluation output root; defaults to work/evaluations."),
    ] = None,
    run_id: Annotated[str | None, typer.Option("--run-id", help="Stable evaluation run id.")] = None,
    execute: Annotated[
        bool,
        typer.Option(
            "--execute/--no-execute",
            help=(
                "Run executable hooks/tests through CommandRuntime and "
                "VerificationRuntime. Defaults to deterministic offline scoring."
            ),
        ),
    ] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print regression report JSON.")] = False,
) -> None:
    """Compare a candidate profile against a baseline benchmark report."""

    import json

    baseline = EvaluationProfile.from_dict(json.loads(baseline_profile_json))
    candidate = EvaluationProfile.from_dict(json.loads(candidate_profile_json))
    runtime = _evaluation_runtime_from_cli(
        project_root=Path.cwd(),
        output_root=output_dir,
        run_id=run_id,
        execute=execute,
    )
    report = runtime.run_ab(
        tasks=GoldenTaskStore(task_set).load(),
        baseline=baseline,
        candidate=candidate,
        trace_run_dir=trace_run_dir,
        run_id=run_id,
        write_report=True,
        execute=execute,
    )
    regression = RegressionDetector().compare(
        report.profile_reports[0],
        report.profile_reports[1],
        threshold=threshold,
        block_on_regression=block_on_regression,
    )
    runtime.write_regression_report(run_id=report.run_id, regression=regression)
    _print_eval_payload(
        regression.to_dict() if json_output else regression.to_markdown(),
        json_output=json_output,
        title="regression report",
    )
    if regression.blocking:
        raise typer.Exit(2)


@eval_report_app.command("show")
def eval_report_show(
    report_path: Annotated[Path, typer.Argument(help="Evaluation report JSON or Markdown path.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print raw JSON for JSON reports.")] = False,
) -> None:
    """Show an evaluation report generated by a suite, A/B, or regression run."""

    if report_path.suffix.lower() == ".json":
        text = report_path.read_text(encoding="utf-8")
        if json_output:
            _write_stdout(text)
            return
        console.print(Panel(text, title="evaluation report", border_style="cyan"))
        return
    console.print(report_path.read_text(encoding="utf-8"))


@eval_live_app.command("quicksort")
def eval_live_quicksort(
    output_dir: Annotated[
        Path | None,
        typer.Option("--output-dir", help="Directory for the live benchmark workspace and report."),
    ] = None,
    run_id: Annotated[
        str | None,
        typer.Option("--run-id", help="Stable live benchmark run id."),
    ] = None,
    max_turns: Annotated[
        int,
        typer.Option("--max-turns", min=1, max=40, help="Maximum live model turns."),
    ] = 12,
    model: Annotated[
        str | None,
        typer.Option("--model", help="Override SINGULARITY_MODEL for this live benchmark."),
    ] = None,
    base_url: Annotated[
        str | None,
        typer.Option("--base-url", help="Override SINGULARITY_BASE_URL for this live benchmark."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    """Run the live provider through a controlled quicksort create-and-verify task."""

    result = _run_live_quicksort_benchmark(
        output_dir=output_dir,
        run_id=run_id,
        max_turns=max_turns,
        model=model,
        base_url=base_url,
    )
    if json_output:
        _write_stdout(json_dumps(result))
        return
    console.print(Panel(json_dumps(result), title="live quicksort benchmark", border_style="cyan"))
    if not result["ok"]:
        raise typer.Exit(1)


def _run_live_quicksort_benchmark(
    *,
    output_dir: Path | None,
    run_id: str | None,
    max_turns: int,
    model: str | None,
    base_url: str | None,
) -> dict[str, object]:
    resolved_run_id = run_id or f"live_quicksort_{uuid4().hex[:8]}"
    root = (output_dir or (Path.cwd() / "work" / "evaluations-live")).expanduser().resolve(strict=False)
    workspace = root / resolved_run_id / "workspace"
    workspace.mkdir(parents=True, exist_ok=True)
    readme = workspace / "README.md"
    if not readme.exists():
        readme.write_text(
            "Live benchmark workspace. Create quicksort.py and verify it with python quicksort.py.\n",
            encoding="utf-8",
        )
    goal = (
        "Create quicksort.py in this workspace. It must define quicksort(values), "
        "include a __main__ smoke assertion, and run python quicksort.py through "
        "VerificationRuntime. Finish only after verification passes."
    )
    config = ProductionRuntimeConfig.from_cli(
        project_root=workspace,
        max_turns=max_turns,
        model=model,
        base_url=base_url,
        approval_mode=ApprovalMode.AUTO_SAFE,
        security_mode=SecurityMode.COMPAT,
        profile="live-quicksort",
        cli_overrides={"max_turns", "model", "base_url", "approval_mode", "security_mode", "profile"},
    )
    kernel = KernelBootstrap(project_root=workspace, config=config, console=console).boot(goal)
    try:
        agent_result = kernel.run_task(goal)
        quicksort_path = workspace / "quicksort.py"
        smoke = subprocess.run(
            [sys.executable, "quicksort.py"],
            cwd=workspace,
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        ) if quicksort_path.exists() else None
        ok = bool(
            agent_result.status == RunStatus.COMPLETED
            and quicksort_path.exists()
            and smoke is not None
            and smoke.returncode == 0
        )
        return {
            "schema_version": "evaluation.live_provider_benchmark/v1",
            "benchmark": "quicksort",
            "run_id": resolved_run_id,
            "ok": ok,
            "status": agent_result.status.value,
            "workspace": str(workspace),
            "trace": str(kernel.graph.trace.store.run_dir),
            "final_report": agent_result.final_report.to_dict(),
            "independent_smoke": {
                "command": [sys.executable, "quicksort.py"],
                "exit_code": smoke.returncode if smoke is not None else None,
                "stdout": smoke.stdout[:1000] if smoke is not None else "",
                "stderr": smoke.stderr[:1000] if smoke is not None else "quicksort.py was not created",
            },
        }
    finally:
        close_resources = getattr(kernel, "close_resources", None)
        if callable(close_resources):
            close_resources()


def _profiles_from_cli(profile_json: list[str] | None) -> list[EvaluationProfile]:
    import json

    if not profile_json:
        return [
            EvaluationProfile(
                name="baseline",
                model="default",
                prompt_profile="default",
                memory_enabled=True,
                allowed_tools=[],
                tool_policy="read_write",
            )
        ]
    return [EvaluationProfile.from_dict(json.loads(item)) for item in profile_json if item]


class _NoopTrace:
    def emit(self, *args, **kwargs) -> None:
        return None

    def record(self, *args, **kwargs) -> None:
        return None

    def append(self, *args, **kwargs) -> None:
        return None


def _evaluation_runtime_from_cli(
    *,
    project_root: Path,
    output_root: Path | None,
    run_id: str | None,
    execute: bool,
) -> EvaluationRuntime:
    if not execute:
        return EvaluationRuntime(project_root=project_root, output_root=output_root)
    root = output_root or (project_root / "work" / "evaluations")
    audit_root = root / (run_id or "cli_execute")
    policy_runtime = PolicyRuntime(
        PolicyConfig(
            workspace_root=project_root,
            approval_mode=ApprovalMode.NON_INTERACTIVE,
            security_mode=SecurityMode.STRICT,
            audit_log_path=audit_root / "policy-audit.jsonl",
        )
    )
    sandbox_runtime = SandboxRuntime(
        project_root,
        trace=_NoopTrace(),
        security_mode=SecurityMode.STRICT,
    )
    command_runtime = CommandRuntime(
        project_root,
        trace=None,
        policy_runtime=policy_runtime,
        sandbox_runtime=sandbox_runtime,
    )
    verification_runtime = VerificationRuntime(
        project_root,
        command_runtime=command_runtime,
        trace=None,
        policy_runtime=policy_runtime,
    )
    return EvaluationRuntime(
        project_root=project_root,
        output_root=output_root,
        verification_runtime=verification_runtime,
        command_runtime=command_runtime,
    )


def _print_report(report, *, json_output: bool) -> None:
    if json_output:
        _write_stdout(report.to_json())
        return
    console.print(report.to_markdown())
    if report.output_dir is not None:
        console.print(f"report: {report.output_dir}")


def _print_eval_payload(payload: object, *, json_output: bool, title: str) -> None:
    if isinstance(payload, str):
        text = payload
    else:
        text = json_dumps(payload)
    if json_output:
        _write_stdout(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))


def _write_stdout(text: str) -> None:
    sys.stdout.write(text)
    if not text.endswith("\n"):
        sys.stdout.write("\n")


def _print_release_payload(payload: object, *, json_output: bool, title: str) -> None:
    text = json_dumps(payload)
    if json_output:
        _write_stdout(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))


def _repair_result_payload(
    repair: dict[str, object],
    *,
    after: dict[str, object] | None,
    ok: bool,
) -> dict[str, object]:
    return {
        "schema_version": "repair-result/v1",
        "ok": ok,
        "repair": repair,
        "after": after,
    }


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
            f"{artifact.size_bytes} bytes handle={artifact.relative_path}"
        )


def json_dumps(payload: object) -> str:
    import json

    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str)


def main() -> None:
    app()


if __name__ == "__main__":
    main()
