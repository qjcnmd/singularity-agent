from __future__ import annotations

from pathlib import Path

from singularity.policy import ApprovalMode, PolicyConfig, PolicyEngine


def make_test_policy_engine(workspace_root: Path) -> PolicyEngine:
    return PolicyEngine(
        PolicyConfig(
            workspace_root=workspace_root,
            approval_mode=ApprovalMode.NON_INTERACTIVE,
        )
    )


def default_policy_engine(workspace_root: Path) -> PolicyEngine:
    return PolicyEngine(PolicyConfig.default_for_workspace(workspace_root))

