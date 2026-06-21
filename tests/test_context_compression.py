import json

import pytest

from singularity.context.compression import (
    ContextCompressor,
    ContextSummaryValidationError,
)
from singularity.context import ContextManager
from singularity.context.models import (
    ContextAuthority,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextRuntime,
    ContextSensitivity,
)


def evidence_item(item_id: str) -> ContextItem:
    return ContextItem(
        item_id=item_id,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspect",
        layer=ContextLayer.EVIDENCE,
        source_runtime=ContextRuntime.TOOL,
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


def test_context_manager_rejects_invalid_compression_response(tmp_path) -> None:
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

    with pytest.raises(ContextSummaryValidationError):
        context.messages(persist=True)
