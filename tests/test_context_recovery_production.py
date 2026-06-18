from pathlib import Path

from miniharness.context import ContextManager, RecoveryManager
from miniharness.context.models import (
    MutationEvidence,
    PolicyObservation,
    RecoveredContext,
)
from miniharness.context.tokens import TokenCounter


def test_recovery_reports_pending_tool_calls_without_reexecuting_completed(
    tmp_path: Path,
) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        db_path=tmp_path / "context.sqlite3",
        run_id="run_1",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    completed = {
        "id": "call_done",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{}"},
    }
    pending = {
        "id": "call_pending",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{}"},
    }
    context.add_assistant_message(
        {"role": "assistant", "content": None, "tool_calls": [completed, pending]}
    )
    context.add_tool_result(tool_call=completed, result={"ok": True, "content": "done"})

    recovered = RecoveryManager(tmp_path / "context.sqlite3").recover("run_1")

    assert isinstance(recovered, RecoveredContext)
    assert recovered.completed_tool_call_ids == {"call_done"}
    assert [call["id"] for call in recovered.pending_tool_calls] == ["call_pending"]
    assert recovered.recommended_next_action == "execute_pending_tool"
    assert recovered.next_action == "execute_pending_tool"


def test_recovery_detects_pending_approval_and_open_mutation(
    tmp_path: Path,
) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="change code",
        db_path=tmp_path / "context.sqlite3",
        run_id="run_1",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    context.add_policy_observation(
        PolicyObservation(
            decision_id="decision_1",
            request_id="request_1",
            outcome="require_review",
            risk_level="high",
            reason="needs approval",
            constraints_summary=[],
            user_decision=None,
            approval_grant_id=None,
            runtime="policy",
            operation="write",
            resource="src/app.py",
            reference="ref_policy",
        )
    )
    context.add_mutation_evidence(
        MutationEvidence(
            transaction_id="tx_open",
            files_changed=["src/app.py"],
            diff_summary="pending edit",
            rollback_ref="rollback_1",
            status="open",
        )
    )

    recovered = RecoveryManager(tmp_path / "context.sqlite3").recover("run_1")

    assert recovered.pending_policy_approval["decision_id"] == "decision_1"
    assert recovered.open_mutation_transactions == ["tx_open"]
    assert recovered.recommended_next_action == "ask_user_for_pending_approval"
    assert recovered.recovery_warnings


def test_recovery_reads_structured_trace_event_type(tmp_path: Path) -> None:
    db_path = tmp_path / "context.sqlite3"
    trace_path = tmp_path / "events.jsonl"
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        db_path=db_path,
        run_id="run_1",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )
    trace_path.write_text(
        '{"event_type": "model.request.created", "run_id": "run_1"}\n',
        encoding="utf-8",
    )

    recovered = RecoveryManager(db_path, trace_path=trace_path).recover(context.run_id)

    assert recovered.trace_last_event == "model_request"
    assert recovered.recommended_next_action == "request_model"
