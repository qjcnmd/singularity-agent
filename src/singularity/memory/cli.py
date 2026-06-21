from __future__ import annotations

from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from singularity.memory.runtime import MemoryRuntime


memory_app = typer.Typer(add_completion=False, no_args_is_help=True)
rules_app = typer.Typer(add_completion=False, no_args_is_help=True)
memory_app.add_typer(rules_app, name="rules")
console = Console()


@memory_app.command("list")
def memory_list(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime(read_only=True)
    entries = [entry.to_dict() for entry in runtime.store.load_entries(rebuild_index=False)]
    _print(entries, json_output=json_output, title="memory entries")


@memory_app.command("candidates")
def memory_candidates(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime(read_only=True)
    candidates = [
        candidate.to_dict()
        for candidate in runtime.store.load_candidates(rebuild_index=False)
    ]
    _print(candidates, json_output=json_output, title="memory candidates")


@memory_app.command("show")
def memory_show(
    memory_id: Annotated[str, typer.Argument(help="Memory entry id.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime(read_only=True)
    entry = runtime.store.get_entry(memory_id, rebuild_index=False)
    _print(entry.to_dict(), json_output=json_output, title=f"memory {memory_id}")


@memory_app.command("search")
def memory_search(
    query: Annotated[str, typer.Argument(help="Goal/query text.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime(read_only=True)
    results = [result.to_dict() for result in runtime.retrieve(goal=query)]
    _print(results, json_output=json_output, title="memory search")


@memory_app.command("accept")
def memory_accept(candidate_id: Annotated[str, typer.Argument(help="Memory candidate id.")]) -> None:
    runtime = _runtime()
    entry = runtime.accept_candidate(candidate_id)
    console.print(f"accepted {candidate_id} -> {entry.id}")


@memory_app.command("reject")
def memory_reject(
    candidate_id: Annotated[str, typer.Argument(help="Memory candidate id.")],
    reason: Annotated[str, typer.Option("--reason", help="Rejection reason.")] = "rejected",
) -> None:
    runtime = _runtime()
    candidate = runtime.reject_candidate(candidate_id, reason=reason)
    console.print(f"rejected {candidate.id}")


@memory_app.command("delete")
def memory_delete(
    memory_id: Annotated[str, typer.Argument(help="Memory entry id.")],
    reason: Annotated[str, typer.Option("--reason", help="Tombstone reason.")] = "deleted",
) -> None:
    runtime = _runtime()
    entry = runtime.delete_entry(memory_id, reason=reason)
    console.print(f"deleted {entry.id}")


@memory_app.command("doctor")
def memory_doctor(
    repair: Annotated[bool, typer.Option("--repair", help="Repair refreshable memory issues.")] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime(read_only=not repair)
    report = runtime.doctor(repair=repair)
    _print(report, json_output=json_output, title="memory doctor")


@memory_app.command("refresh")
def memory_refresh(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime()
    report = runtime.refresh()
    _print(report, json_output=json_output, title="memory refresh")


@rules_app.command("list")
def memory_rules_list(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
) -> None:
    runtime = _runtime(read_only=True)
    rules = runtime.list_rules()
    _print(rules, json_output=json_output, title="memory rules")


def _runtime(*, read_only: bool = False) -> MemoryRuntime:
    runtime = MemoryRuntime(Path.cwd())
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
        console.print(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))
