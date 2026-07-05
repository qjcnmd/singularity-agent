from __future__ import annotations

from singularity.agent_loop_failure_recovery import FailureRecoveryCoordinator


def test_failure_recovery_detects_repairable_planner_failure() -> None:
    class _State:
        pass

    class _Planner:
        state = _State()
        evidence = type(
            "Evidence",
            (),
            {
                "verification_results": [],
                "unresolved_failures": [
                    {
                        "error_code": "schema_mismatch",
                    }
                ]
            },
        )()

    assert FailureRecoveryCoordinator.has_repairable_planner_failure(_Planner()) is True
