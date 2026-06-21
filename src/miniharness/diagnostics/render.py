from __future__ import annotations

from typing import Any

from rich.console import Console
from rich.panel import Panel
from rich.table import Table

from miniharness.diagnostics.models import DiagnosticResult, RepairPlan


SEVERITY_STYLES = {
    "error": "red",
    "warning": "yellow",
    "info": "cyan",
    "suggestion": "magenta",
}


def render_diagnostic_result(console: Console, result: DiagnosticResult, *, verbose: bool = False) -> None:
    table = Table(title="MiniHarness doctor")
    table.add_column("severity")
    table.add_column("status")
    table.add_column("check")
    table.add_column("message")
    if verbose:
        table.add_column("detail")
        table.add_column("suggested fix")
    for finding in result.findings:
        style = SEVERITY_STYLES[finding.severity.value]
        row = [
            f"[{style}]{finding.severity.value}[/{style}]",
            finding.status,
            finding.check_id,
            finding.message,
        ]
        if verbose:
            row.extend([finding.technical_detail, finding.suggested_fix])
        table.add_row(*row)
    console.print(table)
    status = "ok" if result.ok else "errors found"
    console.print(Panel(f"{status} | summary={result.summary}", title="doctor summary", border_style="green" if result.ok else "red"))


def render_repair_plan(console: Console, payload: RepairPlan | dict[str, Any]) -> None:
    data = payload.to_dict() if isinstance(payload, RepairPlan) else payload
    repair = data.get("repair", data)
    table = Table(title="MiniHarness repair")
    table.add_column("status")
    table.add_column("check")
    table.add_column("action")
    table.add_column("target")
    for action in repair.get("actions", []):
        table.add_row(
            str(action.get("status")),
            str(action.get("check_id")),
            str(action.get("kind")),
            str(action.get("target")),
        )
    console.print(table)
    console.print(Panel(f"applied={repair.get('applied')} audit={repair.get('audit_log_path')}", title="repair summary", border_style="cyan"))
