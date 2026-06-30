from __future__ import annotations

from pathlib import Path

import pytest
from rich.console import Console

from singularity.interaction import (
    ClarificationAnswer,
    ClarificationRequest,
    ControlCommand,
    DecisionPrompt,
    FinalReport,
    InteractionController,
    InteractionEvent,
    InteractionMode,
    OutcomeStatus,
    ProgressEvent,
    RichCliRenderer,
    UserDecision,
)
from singularity.kernel.cancellation import CancellationManager
from singularity.observability import TraceEventType, TraceRecorder
from singularity.planner import Planner


class FakeProvider:
    def __init__(
        self,
        *,
        decision: str = "approve",
        answer: str = "use the safer path",
        revised_goal: str | None = "Implement the safer clarified goal",
    ) -> None:
        self.decision = decision
        self.answer = answer
        self.revised_goal = revised_goal

    def request_decision(self, prompt: DecisionPrompt) -> UserDecision:
        return UserDecision(
            prompt_id=prompt.prompt_id,
            decision=self.decision,
            reason=f"{self.decision} from fake provider",
            decided_by="test-user",
        )

    def request_clarification(self, request: ClarificationRequest) -> ClarificationAnswer:
        return ClarificationAnswer(
            request_id=request.request_id,
            answer=self.answer,
            revised_goal=self.revised_goal,
            answered_by="test-user",
        )


def test_models_round_trip() -> None:
    event = InteractionEvent(
        event_type="phase.started",
        summary="started",
        component="planner",
        payload={"phase": "planning"},
    )
    report = FinalReport(
        outcome=OutcomeStatus.UNVERIFIED,
        summary="missing verification",
        files_changed=["src/app.py"],
    )

    assert InteractionEvent.from_json(event.to_json()).to_dict() == event.to_dict()
    assert FinalReport.from_dict(report.to_dict()) == report
    assert ProgressEvent(
        phase="planning",
        status="started",
        summary="planning",
    ).to_interaction_event().event_type == "progress.started"


def test_publish_consumes_sinks_and_writes_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path)
    seen: list[InteractionEvent] = []
    interaction = InteractionController(trace=trace, sinks=[seen.append])
    trace.set_interaction_sink(interaction.consume_trace_event)

    interaction.publish(InteractionEvent(event_type="phase.started", summary="Planning"))

    assert seen[0].summary == "Planning"
    events = trace.store.query_events()
    assert events[-1].payload["interaction_event_type"] == "phase.started"


def test_trace_bridge_converts_trace_events_to_interaction_events(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path)
    seen: list[InteractionEvent] = []
    interaction = InteractionController(sinks=[seen.append])
    trace.set_interaction_sink(interaction.consume_trace_event)

    trace.emit(
        TraceEventType.VERIFICATION_CHECK_COMPLETED,
        component="verification",
        summary="Verification completed.",
        payload={"status": "ready"},
    )

    assert seen[-1].event_type == "verification.check_completed"
    assert seen[-1].component == "verification"
    assert seen[-1].payload["status"] == "ready"


def test_renderer_outputs_user_visible_sections() -> None:
    console = Console(record=True, width=100)
    renderer = RichCliRenderer(console)

    renderer.handle(InteractionEvent(event_type="patch.proposed", summary="Patch", payload={"summary": "2 files"}))
    renderer.handle(InteractionEvent(event_type="policy.blocked", summary="Risk", payload={"risk_level": "high"}))
    renderer.handle(InteractionEvent(event_type="verification.check_completed", summary="Tests passed", payload={"status": "ready"}))
    renderer.handle(InteractionEvent(event_type="review.finding", summary="Blocking issue"))
    renderer.render_final_report(
        FinalReport(
            outcome=OutcomeStatus.SUCCESS,
            summary="done",
            verification_status="ready",
        )
    )

    output = console.export_text()
    assert "patch summary" in output
    assert "policy risk" in output
    assert "verification result" in output
    assert "review finding" in output
    assert "final report: success" in output


def test_renderer_outputs_planner_context_usage_diagnostic() -> None:
    console = Console(record=True, width=120)
    renderer = RichCliRenderer(console)

    renderer.render_final_report(
        {
            "status": "completed",
            "context_usage_diagnostic": {
                "layer_token_usage": {"recent_dialogue": 12},
                "included_item_ids": ["included_1"],
                "excluded_item_ids": ["excluded_1"],
                "stale_item_ids": ["stale_1"],
                "summary_item_ids": ["summary_1"],
                "recent_tail_item_ids": ["tail_1"],
                "cache_hit_ratio": 0.25,
                "cache_attribution": {"source": "component_inferred"},
                "cache_miss_reasons": ["context_shape_change"],
            },
        }
    )

    output = console.export_text()
    assert "context_usage" in output
    assert "layer_token_usage" in output
    assert "included_items: 1" in output
    assert "excluded_items: 1" in output
    assert "stale_items: 1" in output
    assert "summary_items: 1" in output
    assert "recent_tail_items: 1" in output
    assert "cache_hit_ratio: 0.25" in output
    assert "cache_attribution_source: component_inferred" in output
    assert "context_shape_change" in output


def test_interactive_and_non_interactive_decisions_write_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path)
    interactive = InteractionController(trace=trace, provider=FakeProvider(decision="revise"))
    prompt = DecisionPrompt(
        title="Approval",
        message="Approve?",
        choices=["approve", "reject", "revise", "abort"],
    )

    decision = interactive.request_decision(prompt)
    non_interactive = InteractionController(
        mode=InteractionMode.NON_INTERACTIVE,
        trace=trace,
    ).request_decision(prompt)

    assert decision.decision == "revise"
    assert non_interactive.decision == "reject"
    assert non_interactive.metadata["fail_closed"] is True
    event_types = {event.event_type.value for event in trace.store.query_events()}
    assert "user_decision.recorded" in event_types


def test_clarification_flow_records_goal_revision_and_trace(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path)
    planner = Planner(tmp_path, session_id="session", task_id="task", trace=trace)
    planner.start_task("Original goal")
    interaction = InteractionController(trace=trace, provider=FakeProvider())

    answer = interaction.request_clarification(
        ClarificationRequest(
            question="Which scope?",
            reason="goal unclear",
            current_goal="Original goal",
        ),
        planner=planner,
    )

    assert answer.revised_goal == "Implement the safer clarified goal"
    assert planner.state.user_goal == "Original goal"
    assert planner.state.effective_goal == "Implement the safer clarified goal"
    assert planner.state.goal_revisions[-1]["answer"] == "use the safer path"
    event_types = {event.event_type.value for event in trace.store.query_events()}
    assert "clarification.requested" in event_types
    assert "clarification.answered" in event_types


def test_cancel_command_triggers_token_and_cancelled_report(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path)
    cancellation = CancellationManager()
    interaction = InteractionController(trace=trace, cancellation_manager=cancellation)

    interaction.handle_command(ControlCommand.CANCEL, message="Ctrl+C")
    report = interaction.build_final_report(cancelled=True, cancellation_reason="Ctrl+C")

    assert cancellation.token.cancelled is True
    assert report.outcome == OutcomeStatus.CANCELLED
    event_types = {event.event_type.value for event in trace.store.query_events()}
    assert "control_command.received" in event_types
    assert "final_report.completed" in event_types


@pytest.mark.parametrize(
    ("kwargs", "expected"),
    [
        (
            {
                "planner_report": {
                    "status": "completed",
                    "files_changed": ["a.py"],
                    "verification_summary": {"status": "ready"},
                }
            },
            OutcomeStatus.SUCCESS,
        ),
        (
            {
                "planner_report": {
                    "status": "running",
                    "files_changed": ["a.py"],
                    "unresolved_issues": ["test failed"],
                    "verification_summary": {"status": "failed"},
                }
            },
            OutcomeStatus.PARTIAL_SUCCESS,
        ),
        ({"error": RuntimeError("boom")}, OutcomeStatus.FAILED),
        ({"cancelled": True}, OutcomeStatus.CANCELLED),
        ({"blocked_reasons": ["approval denied"]}, OutcomeStatus.BLOCKED),
        (
            {"planner_report": {"status": "completed", "files_changed": ["a.py"]}},
            OutcomeStatus.UNVERIFIED,
        ),
        (
            {
                "planner_report": {"status": "completed", "files_changed": ["a.py"]},
                "blocked_reasons": ["required_verifications_passed"],
            },
            OutcomeStatus.UNVERIFIED,
        ),
        (
            {
                "kernel_report": {
                    "planner_summary": {
                        "status": "completed",
                        "files_changed": ["a.py"],
                        "verification_summary": {"status": "ready"},
                    }
                },
                "blocked_reasons": ["required_verifications_passed"],
            },
            OutcomeStatus.SUCCESS,
        ),
    ],
)
def test_final_report_outcome_mapping(kwargs: dict, expected: OutcomeStatus) -> None:
    report = InteractionController().build_final_report(**kwargs)

    assert report.outcome == expected
