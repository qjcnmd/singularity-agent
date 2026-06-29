from singularity.error_codes import ERROR_CODE_VALUES, ErrorCode


def test_error_code_registry_has_unique_core_runtime_codes() -> None:
    expected = {
        "completion_rejected",
        "final_review_rejected",
        "max_turns_exceeded",
        "model_runner_failed",
        "approval_required",
        "policy_denied",
        "policy_ask_user_required",
        "action_not_allowed",
        "snapshot_mismatch",
        "semantic_failure",
        "timeout",
        "sandbox_unavailable",
        "verification_runner_required",
        "repair_budget_exceeded",
    }

    assert len(ERROR_CODE_VALUES) == len(set(ERROR_CODE_VALUES))
    assert expected <= ERROR_CODE_VALUES
    assert ErrorCode.COMPLETION_REJECTED == "completion_rejected"
