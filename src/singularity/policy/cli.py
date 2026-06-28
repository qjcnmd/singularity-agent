from __future__ import annotations

import json
from pathlib import Path
from typing import Annotated

import typer
from rich.console import Console
from rich.panel import Panel

from singularity.cli_paths import resolve_project_root
from singularity.policy.approval import ApprovalGate
from singularity.policy.config import PolicyConfig
from singularity.policy.remote import RemoteApprovalExchange

approval_app = typer.Typer(add_completion=False, no_args_is_help=True)
remote_app = typer.Typer(add_completion=False, no_args_is_help=True)
approval_app.add_typer(remote_app, name="remote")
console = Console()
ProjectRootOption = Annotated[
    Path | None,
    typer.Option("--project-root", help="Workspace/project root; defaults to the current directory."),
]


@remote_app.command("export-request")
def export_remote_request(
    request_json: Annotated[Path, typer.Argument(help="PolicyRequest JSON file.")],
    decision_json: Annotated[Path, typer.Argument(help="PolicyDecision JSON file.")],
    output: Annotated[
        Path | None,
        typer.Option("--output", "-o", help="Output approval request JSON path."),
    ] = None,
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    """Export a policy request/decision pair for file-backed remote review."""

    remote = RemoteApprovalExchange(resolve_project_root(project_root))
    exported = remote.export_request_from_files(request_json, decision_json, output_path=output)
    _print(exported.to_dict(), json_output=json_output, title="remote approval request")


@remote_app.command("import-grant")
def import_remote_grant(
    grant_json: Annotated[Path, typer.Argument(help="Remote ApprovalGrant JSON file.")],
    json_output: Annotated[bool, typer.Option("--json", help="Print machine-readable JSON.")] = False,
    project_root: ProjectRootOption = None,
) -> None:
    """Import and register a file-backed remote approval grant."""

    project_root = resolve_project_root(project_root)
    remote = RemoteApprovalExchange(project_root)
    approval_gate = ApprovalGate(PolicyConfig(workspace_root=project_root))
    grant = remote.register_grant(grant_json, approval_gate)
    _print(
        {
            "ok": True,
            "grant_id": grant.grant_id,
            "request_id": grant.request_id,
            "decision_id": grant.decision_id,
            "approved_by": grant.approved_by,
            "approval_grants_path": str(approval_gate.config.approval_grants_path),
        },
        json_output=json_output,
        title="remote approval grant",
    )


def _print(payload: dict[str, object], *, json_output: bool, title: str) -> None:
    text = json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str)
    if json_output:
        typer.echo(text)
        return
    console.print(Panel(text, title=title, border_style="cyan"))
