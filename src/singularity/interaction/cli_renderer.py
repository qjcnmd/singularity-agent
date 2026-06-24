from __future__ import annotations

import json
from typing import Any

from rich.console import Console
from rich.panel import Panel
from rich.prompt import Prompt
from rich.table import Table

from singularity.interaction.models import (
    ClarificationAnswer,
    ClarificationRequest,
    DecisionPrompt,
    FinalReport,
    InteractionEvent,
    UserDecision,
)


class RichCliRenderer:
    def __init__(self, console: Console | None = None) -> None:
        self.console = console or Console()
        self.current_phase: str | None = None

    def __call__(self, event: InteractionEvent) -> None:
        self.handle(event)

    def handle(self, event: InteractionEvent) -> None:
        event_type = event.event_type
        if event_type.startswith("progress."):
            self._render_progress(event)
        elif event_type in {"phase.started", "phase.completed"}:
            self._render_phase(event)
        elif event_type in {"action.proposed", "planner.replan_triggered"}:
            self._render_plan(event)
        elif event_type in {"tool.dispatch.started", "tool.dispatch.completed", "tool.dispatch.failed"}:
            self._render_tool(event)
        elif event_type in {"patch.proposed", "edit.patch_validated", "edit.plan_created"}:
            self._render_patch(event)
        elif event_type in {"policy.blocked", "policy.decided", "approval.requested"}:
            self._render_policy(event)
        elif event_type == "decision.prompted":
            self._render_decision_prompt(event)
        elif event_type in {"verification.check_completed", "verification.failed"}:
            self._render_verification(event)
        elif event_type == "review.finding":
            self._render_review(event)
        elif event_type in {"mutation.rollback_started", "mutation.rollback_completed"}:
            self._render_rollback(event)
        elif event_type == "control_command.received":
            self.console.print(f"[yellow]control[/yellow] {event.summary}")
        elif event_type == "final_report.completed":
            self._render_final_report_event(event)

    def render_final_report(self, report: Any, *, border_style: str = "green") -> None:
        payload = report.to_dict() if hasattr(report, "to_dict") else report
        if isinstance(report, FinalReport):
            self.console.print(
                Panel(
                    self._report_text(report),
                    title=f"final report: {_outcome_value(report.outcome)}",
                    border_style=border_style,
                )
            )
            return
        if isinstance(payload, dict) and payload.get("context_usage_diagnostic"):
            self.console.print(
                Panel(
                    self._planner_report_text(payload),
                    title="final report",
                    border_style=border_style,
                )
            )
            return
        self.console.print(
            Panel(
                json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str),
                title="final report",
                border_style=border_style,
            )
        )

    def _render_progress(self, event: InteractionEvent) -> None:
        payload = event.payload
        phase = str(payload.get("phase") or event.phase_id or "component")
        current = payload.get("current")
        total = payload.get("total")
        suffix = f" {current}/{total}" if current is not None and total is not None else ""
        self.console.print(f"[cyan]{phase}[/cyan] {event.summary}{suffix}")

    def _render_phase(self, event: InteractionEvent) -> None:
        self.current_phase = event.phase_id or str(event.payload.get("phase") or "")
        style = "cyan" if event.event_type.endswith("started") else "green"
        self.console.print(f"[{style}]phase[/] {event.summary}")

    def _render_plan(self, event: InteractionEvent) -> None:
        payload = event.payload
        table = Table(title="plan", show_header=False)
        table.add_column("key", style="cyan")
        table.add_column("value")
        for key in ("phase", "action_kind", "decision", "reason"):
            value = payload.get(key)
            if value:
                table.add_row(key, str(value))
        if payload.get("replan_decision"):
            table.add_row("replan", json.dumps(payload["replan_decision"], ensure_ascii=False))
        if table.row_count:
            self.console.print(table)
        else:
            self.console.print(f"[cyan]plan[/cyan] {event.summary}")

    def _render_tool(self, event: InteractionEvent) -> None:
        tool = event.payload.get("tool_name") or event.payload.get("name") or "<unknown>"
        style = "red" if event.event_type.endswith("failed") else "cyan"
        self.console.print(f"[{style}]tool[/] {tool}: {event.summary}")

    def _render_patch(self, event: InteractionEvent) -> None:
        payload = event.payload
        summary = payload.get("diff_summary") or payload.get("summary") or event.summary
        self.console.print(Panel(str(summary), title="patch summary", border_style="cyan"))

    def _render_policy(self, event: InteractionEvent) -> None:
        payload = event.payload
        risk = payload.get("risk_level") or payload.get("risk")
        outcome = payload.get("outcome")
        resource = payload.get("resource")
        lines = [event.summary]
        if outcome:
            lines.append(f"outcome: {outcome}")
        if risk:
            lines.append(f"risk: {risk}")
        if resource:
            lines.append(f"resource: {resource}")
        self.console.print(Panel("\n".join(lines), title="policy risk", border_style="yellow"))

    def _render_decision_prompt(self, event: InteractionEvent) -> None:
        prompt = ((event.payload or {}).get("prompt") or {})
        title = prompt.get("title") or "approval request"
        message = prompt.get("message") or event.summary
        choices = ", ".join(prompt.get("choices") or [])
        suffix = f"\nchoices: {choices}" if choices else ""
        self.console.print(Panel(f"{message}{suffix}", title=title, border_style="yellow"))

    def _render_verification(self, event: InteractionEvent) -> None:
        payload = event.payload
        status = payload.get("status") or payload.get("semantic_status") or event.event_type
        self.console.print(
            Panel(
                f"{event.summary}\nstatus: {status}",
                title="verification result",
                border_style="green" if event.event_type.endswith("completed") else "red",
            )
        )

    def _render_review(self, event: InteractionEvent) -> None:
        self.console.print(Panel(event.summary, title="review finding", border_style="yellow"))

    def _render_rollback(self, event: InteractionEvent) -> None:
        reason = event.payload.get("reason") or event.summary
        self.console.print(Panel(str(reason), title="rollback reason", border_style="yellow"))

    def _render_final_report_event(self, event: InteractionEvent) -> None:
        payload = (event.payload or {}).get("final_report")
        if isinstance(payload, dict) and payload.get("outcome"):
            self.render_final_report(FinalReport.from_dict(payload))
            return
        self.console.print(Panel(event.summary, title="final report", border_style="green"))

    @staticmethod
    def _report_text(report: FinalReport) -> str:
        lines = [
            f"outcome: {_outcome_value(report.outcome)}",
            f"summary: {report.summary}",
        ]
        if report.verification_status:
            lines.append(f"verification: {report.verification_status}")
        if report.files_changed:
            lines.append("files_changed: " + ", ".join(report.files_changed))
        if report.blocked_reasons:
            lines.append("blocked: " + "; ".join(report.blocked_reasons))
        if report.cancelled_reason:
            lines.append(f"cancelled_reason: {report.cancelled_reason}")
        if report.next_steps:
            lines.append("next_steps: " + "; ".join(report.next_steps))
        return "\n".join(lines)

    @staticmethod
    def _planner_report_text(payload: dict[str, Any]) -> str:
        diagnostic = dict(payload.get("context_usage_diagnostic") or {})
        attribution = dict(diagnostic.get("cache_attribution") or {})
        lines = [
            f"status: {payload.get('status', 'unknown')}",
            "context_usage:",
            f"  layer_token_usage: {diagnostic.get('layer_token_usage') or {}}",
            f"  included_items: {len(diagnostic.get('included_item_ids') or [])}",
            f"  excluded_items: {len(diagnostic.get('excluded_item_ids') or [])}",
            f"  stale_items: {len(diagnostic.get('stale_item_ids') or [])}",
            f"  summary_items: {len(diagnostic.get('summary_item_ids') or [])}",
            f"  recent_tail_items: {len(diagnostic.get('recent_tail_item_ids') or [])}",
            f"  cache_hit_ratio: {diagnostic.get('cache_hit_ratio', 0.0)}",
            f"  cache_attribution_source: {attribution.get('source') or 'unknown'}",
            f"  cache_miss_reasons: {diagnostic.get('cache_miss_reasons') or []}",
        ]
        return "\n".join(lines)


def _outcome_value(outcome: Any) -> str:
    value = getattr(outcome, "value", outcome)
    return str(value)


class RichInteractionProvider:
    def __init__(self, console: Console | None = None) -> None:
        self.console = console or Console()

    def request_decision(self, prompt: DecisionPrompt) -> UserDecision:
        renderer = RichCliRenderer(self.console)
        renderer.handle(
            InteractionEvent(
                event_type="decision.prompted",
                summary=prompt.message,
                payload={"prompt": prompt.to_dict()},
                severity="warning",
            )
        )
        default = prompt.recommended or prompt.default_decision
        answer = Prompt.ask(
            "Decision",
            choices=None if prompt.allow_freeform else prompt.choices,
            default=default,
            console=self.console,
        )
        normalized = str(answer).strip().lower()
        return UserDecision(
            prompt_id=prompt.prompt_id,
            decision=normalized,
            reason="provided via rich cli",
        )

    def request_clarification(self, request: ClarificationRequest) -> ClarificationAnswer:
        self.console.print(
            Panel(
                f"{request.question}\nreason: {request.reason}",
                title="clarification request",
                border_style="yellow",
            )
        )
        answer = Prompt.ask("Answer", console=self.console)
        revised_goal = Prompt.ask(
            "Revised goal",
            default=request.current_goal,
            console=self.console,
        )
        return ClarificationAnswer(
            request_id=request.request_id,
            answer=answer,
            revised_goal=revised_goal,
        )


def json_dumps(payload: object) -> str:
    return json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True, default=str)
