from pathlib import Path

from singularity.context import ContextManager
from singularity.context.models import (
    CommandObservation,
    MutationEvidence,
    PlannerState,
    PolicyObservation,
    VerificationEvidence,
)
from singularity.context.tokens import TokenCounter


def test_context_manager_records_structured_component_observations(
    tmp_path: Path,
) -> None:
    context = ContextManager(
        system_prompt="system",
        user_goal="fix tests",
        db_path=tmp_path / "context.sqlite3",
        run_id="run_1",
        token_counter=TokenCounter(model="gpt-4o-mini"),
    )

    context.add_planner_state(
        PlannerState(
            task_id="task_1",
            current_phase="verify",
            status="in_progress",
            current_plan=["run tests"],
            completion_criteria={"tests": "pass"},
            open_actions=["run pytest"],
            blocked_actions=[],
            risk_escalations=[],
            evidence_refs=[],
        )
    )
    context.add_policy_observation(
        PolicyObservation(
            decision_id="decision_1",
            request_id="request_1",
            outcome="deny",
            risk_level="high",
            reason="git mutation blocked",
            constraints_summary=["no git mutation"],
            user_decision=None,
            approval_grant_id=None,
            component="policy",
            operation="git push",
            resource="repo",
            reference="ref_policy",
        )
    )
    context.add_mutation_evidence(
        MutationEvidence(
            transaction_id="tx_1",
            files_changed=["src/singularity/context/manager.py"],
            diff_summary="updated context manager",
            rollback_ref="rollback_1",
            status="applied",
        )
    )
    context.add_edit_result(
        {
            "edit_result_id": "edit_result_1",
            "edit_plan_id": "edit_plan_1",
            "intent_id": "intent_1",
            "strategy": "targeted_patch",
            "status": "applied",
            "ok": True,
            "patch_candidate_id": "patch_1",
            "patch_digest": "digest_1",
            "changed_files": ["src/singularity/context/manager.py"],
            "changeset_id": "change_1",
            "transaction_id": "tx_1",
            "validation": {"ok": True, "requires_review": False, "issues": []},
        }
    )
    context.add_command_observation(
        CommandObservation(
            command_id="cmd_1",
            command_preview="pytest tests/test_context.py",
            exit_code=1,
            status="failed",
            stdout_preview="",
            stderr_preview="AssertionError",
            output_ref="artifact_1",
            resource_limits={"timeout": 60},
            policy_decision_id="decision_1",
        )
    )
    context.add_verification_evidence(
        VerificationEvidence(
            check_id="check_1",
            command="pytest",
            status="failed",
            failure_summary="one failing test",
            parsed_failures=["AssertionError"],
            repair_hints=["fix context rendering"],
            logs_ref="artifact_1",
            confidence=0.9,
        )
    )
    context.add_workspace_state(
        {"status": "dirty", "changed_files": ["src/singularity/context/manager.py"]}
    )

    rendered = "\n".join(str(message.get("content")) for message in context.messages())
    item_types = {item.item_type.value for item in context.store.query_items(run_id="run_1")}

    assert "planner_state" in item_types
    assert "policy_observation" in item_types
    assert "mutation_evidence" in item_types
    assert "edit_evidence" in item_types
    assert "command_observation" in item_types
    assert "verification_evidence" in item_types
    assert "workspace_state" in item_types
    assert "git mutation blocked" in rendered
    assert "one failing test" in rendered
    assert "updated context manager" in rendered
    assert "pytest tests/test_context.py" in rendered

