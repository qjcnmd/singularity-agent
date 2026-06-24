import json
import sqlite3

import pytest

from singularity.context.compression import (
    ContextCompressor,
    ContextSummaryValidationError,
)
from singularity.context import ContextManager
from singularity.context.models import (
    ContextAuthority,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    PartialCompactionRange,
    ContextSource,
    ContextSensitivity,
    MutationEvidence,
    VerificationEvidence,
)


def evidence_item(item_id: str) -> ContextItem:
    return ContextItem(
        item_id=item_id,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspect",
        layer=ContextLayer.EVIDENCE,
        source_component=ContextSource.TOOL,
        item_type=ContextItemType.TOOL_OBSERVATION,
        content={"preview": "README inspected"},
        authority=ContextAuthority.TOOL,
        sensitivity=ContextSensitivity.WORKSPACE,
        token_count=5,
    )


def test_compressor_accepts_structured_summary_with_reference_ids() -> None:
    compressor = ContextCompressor()
    payload = {
        "goal": "inspect project",
        "current_state": "README read",
        "completed_actions": ["read README"],
        "pending_actions": [],
        "verified_facts": [{"fact": "README exists", "reference_ids": ["ref_readme"]}],
        "failed_attempts": [],
        "policy_constraints": ["read-only"],
        "workspace_changes": [],
        "verification_status": "not_run",
        "open_questions": [],
        "reference_ids": ["ref_readme"],
        "omitted_item_ids": ["item_old"],
        "confidence": 0.8,
    }

    summary = compressor.parse_summary(json.dumps(payload), source_items=[evidence_item("item_old")])

    assert summary.goal == "inspect project"
    assert summary.reference_ids == ["ref_readme"]
    assert summary.omitted_item_ids == ["item_old"]


def test_compressor_rejects_invalid_json_and_unreferenced_verified_facts() -> None:
    compressor = ContextCompressor()

    with pytest.raises(ContextSummaryValidationError):
        compressor.parse_summary("not json", source_items=[])

    with pytest.raises(ContextSummaryValidationError):
        compressor.parse_summary(
            json.dumps(
                {
                    "goal": "g",
                    "current_state": "s",
                    "completed_actions": [],
                    "pending_actions": [],
                    "verified_facts": [{"fact": "claim without ref", "reference_ids": []}],
                    "failed_attempts": [],
                    "policy_constraints": [],
                    "workspace_changes": [],
                    "verification_status": "unknown",
                    "open_questions": [],
                    "reference_ids": [],
                    "omitted_item_ids": [],
                    "confidence": 0.5,
                }
            ),
            source_items=[],
        )


def test_compressor_drift_check_preserves_prior_policy_constraints() -> None:
    compressor = ContextCompressor()
    old = compressor.parse_summary(
        json.dumps(
            {
                "goal": "g",
                "current_state": "s",
                "completed_actions": [],
                "pending_actions": [],
                "verified_facts": [],
                "failed_attempts": [],
                "policy_constraints": ["no shell"],
                "workspace_changes": [],
                "verification_status": "unknown",
                "open_questions": [],
                "reference_ids": [],
                "omitted_item_ids": [],
                "confidence": 0.7,
            }
        ),
        source_items=[],
    )

    with pytest.raises(ContextSummaryValidationError):
        compressor.parse_summary(
            json.dumps(
                {
                    "goal": "g",
                    "current_state": "s2",
                    "completed_actions": [],
                    "pending_actions": [],
                    "verified_facts": [],
                    "failed_attempts": [],
                    "policy_constraints": [],
                    "workspace_changes": [],
                    "verification_status": "unknown",
                    "open_questions": [],
                    "reference_ids": [],
                    "omitted_item_ids": [],
                    "confidence": 0.7,
                }
            ),
            source_items=[],
            previous_summary=old,
        )


def test_compressor_drift_check_preserves_facts_workspace_and_verification() -> None:
    compressor = ContextCompressor()
    old = compressor.parse_summary(
        json.dumps(
            {
                "goal": "g",
                "current_state": "s",
                "completed_actions": [],
                "pending_actions": [],
                "verified_facts": [{"fact": "tests passed", "reference_ids": ["ref_test"]}],
                "failed_attempts": [],
                "policy_constraints": [],
                "workspace_changes": [{"changed_files": ["app.py"], "patch_digest": "abc"}],
                "verification_status": "passed",
                "open_questions": [],
                "reference_ids": ["ref_test"],
                "omitted_item_ids": [],
                "confidence": 0.7,
            }
        ),
        source_items=[],
    )

    with pytest.raises(ContextSummaryValidationError):
        compressor.parse_summary(
            json.dumps(
                {
                    "goal": "g",
                    "current_state": "s2",
                    "completed_actions": [],
                    "pending_actions": [],
                    "verified_facts": [],
                    "failed_attempts": [],
                    "policy_constraints": [],
                    "workspace_changes": [],
                    "verification_status": "unknown",
                    "open_questions": [],
                    "reference_ids": [],
                    "omitted_item_ids": [],
                    "confidence": 0.7,
                }
            ),
            source_items=[],
            previous_summary=old,
        )


def test_context_manager_records_invalid_compression_response_and_continues(tmp_path) -> None:
    class BadCompressionProvider:
        def chat(self, *, messages, tools, tool_choice):
            return {"choices": [{"message": {"role": "assistant", "content": "not json"}}]}

    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        provider=BadCompressionProvider(),
        db_path=tmp_path / "context.sqlite3",
        model_context_window=100,
        output_token_reserve=20,
    )
    context.add_assistant_message({"role": "assistant", "content": "history " * 200})

    messages = context.messages(persist=True)

    assert messages[:2] == [
        {"role": "system", "content": "system"},
        {"role": "user", "content": "inspect"},
    ]
    assert context.store.latest_snapshot(context.run_id) is None
    assert any(
        event["event_type"] == "context.compaction_failed"
        for event in context.store.events_for_run(context.run_id)
    )


def test_context_manager_marks_omitted_items_stale_and_avoids_repeat_compaction(tmp_path) -> None:
    class GoodCompressionProvider:
        def __init__(self) -> None:
            self.calls = 0

        def chat(self, *, messages, tools, tool_choice):
            self.calls += 1
            return {
                "choices": [
                    {
                        "message": {
                            "content": json.dumps(
                                {
                                    "goal": "inspect",
                                    "current_state": "old dialogue compacted",
                                    "completed_actions": [],
                                    "pending_actions": [],
                                    "verified_facts": [
                                        {"fact": "history considered", "reference_ids": ["ref_history"]}
                                    ],
                                    "failed_attempts": [],
                                    "policy_constraints": [],
                                    "workspace_changes": [],
                                    "verification_status": "unknown",
                                    "open_questions": [],
                                    "reference_ids": ["ref_history"],
                                    "omitted_item_ids": [],
                                    "confidence": 0.8,
                                }
                            )
                        }
                    }
                ]
            }

    provider = GoodCompressionProvider()
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        provider=provider,
        db_path=tmp_path / "context.sqlite3",
        model_context_window=100,
        output_token_reserve=20,
    )
    context.add_assistant_message({"role": "assistant", "content": "history " * 200})
    old_item_id = [
        item.item_id
        for item in context.store.query_items(run_id=context.run_id)
        if item.item_type == ContextItemType.ASSISTANT_MESSAGE
    ][0]

    context.messages(persist=True)
    context.messages(persist=True)

    assert provider.calls == 1
    assert context.store.load_item(old_item_id).freshness == ContextFreshness.STALE


def test_context_continues_tool_edit_and_verification_after_compaction(tmp_path) -> None:
    class GoodCompressionProvider:
        def chat(self, *, messages, tools, tool_choice):
            return {
                "choices": [
                    {
                        "message": {
                            "content": json.dumps(
                                {
                                    "goal": "inspect",
                                    "current_state": "ready",
                                    "completed_actions": [],
                                    "pending_actions": [],
                                    "verified_facts": [
                                        {"fact": "history compacted", "reference_ids": ["ref_history"]}
                                    ],
                                    "failed_attempts": [],
                                    "policy_constraints": [],
                                    "workspace_changes": [],
                                    "verification_status": "unknown",
                                    "open_questions": [],
                                    "reference_ids": ["ref_history"],
                                    "omitted_item_ids": [],
                                    "confidence": 0.8,
                                }
                            )
                        }
                    }
                ]
            }

    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        provider=GoodCompressionProvider(),
        db_path=tmp_path / "context.sqlite3",
        model_context_window=500,
        output_token_reserve=20,
    )
    context.add_assistant_message({"role": "assistant", "content": "history " * 1000})
    context.messages(persist=True)

    call = {"id": "call_read", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}
    context.add_tool_result(tool_call=call, result={"ok": True, "content": "new result"})
    context.add_mutation_evidence(
        MutationEvidence(
            transaction_id="tx_1",
            files_changed=["app.py"],
            diff_summary="updated app.py",
            rollback_ref="rollback_1",
            status="applied",
        )
    )
    context.add_verification_evidence(
        VerificationEvidence(
            check_id="pytest",
            command="pytest",
            status="passed",
            failure_summary=None,
            parsed_failures=[],
            repair_hints=[],
            logs_ref="log_pytest",
            confidence=0.9,
        )
    )

    rendered = "\n".join(str(message.get("content")) for message in context.messages())

    assert "new result" in rendered
    assert "app.py" in rendered
    assert "pytest" in rendered


def test_forced_compaction_generates_unique_versioned_summary_ids(tmp_path) -> None:
    class GoodCompressionProvider:
        def chat(self, *, messages, tools, tool_choice):
            return {
                "choices": [
                    {
                        "message": {
                            "content": json.dumps(
                                {
                                    "goal": "inspect",
                                    "current_state": "steady",
                                    "completed_actions": [],
                                    "pending_actions": [],
                                    "verified_facts": [],
                                    "failed_attempts": [],
                                    "policy_constraints": [],
                                    "workspace_changes": [],
                                    "verification_status": "unknown",
                                    "open_questions": [],
                                    "reference_ids": [],
                                    "omitted_item_ids": [],
                                    "confidence": 0.8,
                                }
                            )
                        }
                    }
                ]
            }

    db_path = tmp_path / "context.sqlite3"
    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        provider=GoodCompressionProvider(),
        db_path=db_path,
        model_context_window=100,
        output_token_reserve=20,
    )
    context.add_assistant_message({"role": "assistant", "content": "history " * 200})

    assert context.compact_context() is True
    assert context.compact_context() is True

    with sqlite3.connect(db_path) as connection:
        summary_rows = connection.execute(
            "select summary_id from context_summaries order by created_at, summary_id"
        ).fetchall()

    summary_ids = [row[0] for row in summary_rows]
    assert len(summary_ids) == 2
    assert summary_ids[0] != summary_ids[1]


def test_partial_compact_requires_explicit_turn_range_and_preserves_out_of_range_items(tmp_path) -> None:
    class GoodCompressionProvider:
        def chat(self, *, messages, tools, tool_choice):
            return {
                "choices": [
                    {
                        "message": {
                            "content": json.dumps(
                                {
                                    "goal": "inspect",
                                    "current_state": "partial history compacted",
                                    "completed_actions": [],
                                    "pending_actions": [],
                                    "verified_facts": [],
                                    "failed_attempts": [],
                                    "policy_constraints": [],
                                    "workspace_changes": [],
                                    "verification_status": "unknown",
                                    "open_questions": [],
                                    "reference_ids": [],
                                    "omitted_item_ids": [],
                                    "confidence": 0.8,
                                }
                            )
                        }
                    }
                ]
            }

    context = ContextManager(
        system_prompt="system",
        user_goal="inspect",
        provider=GoodCompressionProvider(),
        db_path=tmp_path / "context.sqlite3",
        model_context_window=1000,
        output_token_reserve=20,
    )
    context.add_assistant_message(
        {"role": "assistant", "content": "turn one " * 80, "metadata": {"turn": 1}}
    )
    context.add_assistant_message(
        {"role": "assistant", "content": "turn two " * 80, "metadata": {"turn": 2}}
    )
    one_id = [
        item.item_id
        for item in context.store.query_items(run_id=context.run_id)
        if item.item_type == ContextItemType.ASSISTANT_MESSAGE
        and item.metadata.get("turn") == 1
    ][0]
    two_id = [
        item.item_id
        for item in context.store.query_items(run_id=context.run_id)
        if item.item_type == ContextItemType.ASSISTANT_MESSAGE
        and item.metadata.get("turn") == 2
    ][0]

    assert context.partial_compact(PartialCompactionRange(start_turn=1, end_turn=1)) is True

    assert context.store.load_item(one_id).freshness == ContextFreshness.STALE
    assert context.store.load_item(two_id).freshness == ContextFreshness.CURRENT
    snapshot = context.store.latest_snapshot(context.run_id)
    assert snapshot is not None
    assert snapshot.metadata["compaction_plan"]["partial_range"] == {
        "start_turn": 1,
        "end_turn": 1,
        "checkpoint_id": None,
    }
