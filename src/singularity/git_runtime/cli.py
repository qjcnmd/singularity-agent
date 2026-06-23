from __future__ import annotations

import json
from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from singularity.cli_paths import resolve_project_root
from singularity.git_runtime.runtime import GitRuntime


git_app = typer.Typer(add_completion=False, no_args_is_help=True)
console = Console()
ProjectRootOption = Annotated[
    Path | None,
    typer.Option("--project-root", help="Workspace/project root; defaults to the current directory."),
]


@git_app.command("status")
def git_status(
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    """Inspect local Git repository state without mutating it."""

    payload = GitRuntime(resolve_project_root(project_root)).status().to_dict()
    _print(payload, json_output=json_output, title="git status")


@git_app.command("diff")
def git_diff(
    staged: Annotated[bool, typer.Option("--staged", help="Inspect staged changes.")] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    """Inspect local Git diff statistics without mutating it."""

    payload = GitRuntime(resolve_project_root(project_root)).diff_stat(staged=staged).to_dict()
    _print(payload, json_output=json_output, title="git diff")


@git_app.command("commit")
def git_commit(
    message: Annotated[str, typer.Option("--message", "-m", help="Local commit message.")],
    path: Annotated[
        list[Path] | None,
        typer.Option("--path", help="Workspace path to stage; repeat for multiple paths."),
    ] = None,
    allow_empty: Annotated[bool, typer.Option("--allow-empty", help="Allow an empty local commit.")] = False,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    """Create a local Git commit. This command never pushes."""

    result = GitRuntime(resolve_project_root(project_root)).commit(
        message,
        paths=[str(item) for item in path] if path else None,
        allow_empty=allow_empty,
    )
    _print(result.to_dict(), json_output=json_output, title="git commit")
    if not result.ok:
        raise typer.Exit(result.exit_code or 1)


def _print(payload: dict[str, object], *, json_output: bool, title: str) -> None:
    text = json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str)
    if json_output:
        typer.echo(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))
