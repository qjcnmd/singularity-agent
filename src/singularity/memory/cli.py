from __future__ import annotations

from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from singularity.cli_paths import resolve_project_root
from singularity.memory.runtime import MemoryRuntime
from singularity.memory.sync import MemorySyncRuntime


memory_app = typer.Typer(add_completion=False, no_args_is_help=True)
rules_app = typer.Typer(add_completion=False, no_args_is_help=True)
sync_app = typer.Typer(add_completion=False, no_args_is_help=True)
memory_app.add_typer(rules_app, name="rules")
memory_app.add_typer(sync_app, name="sync")
console = Console()
ProjectRootOption = Annotated[
    Path | None,
    typer.Option("--project-root", help="Workspace/project root; defaults to the current directory."),
]


@memory_app.command("list")
def memory_list(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=True, project_root=project_root)
    entries = [entry.to_dict() for entry in runtime.store.load_entries(rebuild_index=False)]
    _print(entries, json_output=json_output, title="memory entries")


@memory_app.command("candidates")
def memory_candidates(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=True, project_root=project_root)
    candidates = [
        candidate.to_dict()
        for candidate in runtime.store.load_candidates(rebuild_index=False)
    ]
    _print(candidates, json_output=json_output, title="memory candidates")


@memory_app.command("show")
def memory_show(
    memory_id: Annotated[str, typer.Argument(help="Memory entry id.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=True, project_root=project_root)
    entry = runtime.store.get_entry(memory_id, rebuild_index=False)
    _print(entry.to_dict(), json_output=json_output, title=f"memory {memory_id}")


@memory_app.command("search")
def memory_search(
    query: Annotated[str, typer.Argument(help="Goal/query text.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=True, project_root=project_root)
    results = [result.to_dict() for result in runtime.retrieve(goal=query)]
    _print(results, json_output=json_output, title="memory search")


@memory_app.command("accept")
def memory_accept(
    candidate_id: Annotated[str, typer.Argument(help="Memory candidate id.")],
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(project_root=project_root)
    entry = runtime.accept_candidate(candidate_id)
    console.print(f"accepted {candidate_id} -> {entry.id}")


@memory_app.command("reject")
def memory_reject(
    candidate_id: Annotated[str, typer.Argument(help="Memory candidate id.")],
    reason: Annotated[str, typer.Option("--reason", help="Rejection reason.")] = "rejected",
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(project_root=project_root)
    candidate = runtime.reject_candidate(candidate_id, reason=reason)
    console.print(f"rejected {candidate.id}")


@memory_app.command("delete")
def memory_delete(
    memory_id: Annotated[str, typer.Argument(help="Memory entry id.")],
    reason: Annotated[str, typer.Option("--reason", help="Tombstone reason.")] = "deleted",
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(project_root=project_root)
    entry = runtime.delete_entry(memory_id, reason=reason)
    console.print(f"deleted {entry.id}")


@memory_app.command("doctor")
def memory_doctor(
    repair: Annotated[bool, typer.Option("--repair", help="Repair refreshable memory issues.")] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=not repair, project_root=project_root)
    report = runtime.doctor(repair=repair)
    _print(report, json_output=json_output, title="memory doctor")


@memory_app.command("refresh")
def memory_refresh(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(project_root=project_root)
    report = runtime.refresh()
    _print(report, json_output=json_output, title="memory refresh")


@rules_app.command("list")
def memory_rules_list(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=True, project_root=project_root)
    rules = runtime.list_rules()
    _print(rules, json_output=json_output, title="memory rules")


@sync_app.command("export")
def memory_sync_export(
    output_path: Annotated[Path, typer.Argument(help="Output memory sync bundle JSON path.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=True, project_root=project_root)
    result = MemorySyncRuntime(runtime.store).export_bundle(output_path)
    _print(result.to_dict(), json_output=json_output, title="memory sync export")


@sync_app.command("import")
def memory_sync_import(
    bundle_path: Annotated[Path, typer.Argument(help="Memory sync bundle JSON path.")],
    trust_entries: Annotated[
        bool,
        typer.Option(
            "--trust-entries",
            help="Import active entries directly instead of reviewable candidates.",
        ),
    ] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    runtime = _runtime(read_only=False, project_root=project_root)
    result = MemorySyncRuntime(runtime.store).import_bundle(
        bundle_path,
        trust_entries=trust_entries,
    )
    _print(result.to_dict(), json_output=json_output, title="memory sync import")


def _runtime(*, read_only: bool = False, project_root: Path | None = None) -> MemoryRuntime:
    runtime = MemoryRuntime(resolve_project_root(project_root))
    runtime.start_session(
        session_id="memory_cli",
        user_goal="memory cli",
        rebuild_index=not read_only,
    )
    return runtime


def _print(payload: object, *, json_output: bool, title: str) -> None:
    from singularity.cli import json_dumps

    text = json_dumps(payload)
    if json_output:
        typer.echo(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))
