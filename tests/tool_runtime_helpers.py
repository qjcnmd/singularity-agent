from __future__ import annotations

from pathlib import Path

from singularity.policy import ApprovalMode, PolicyConfig, PolicyRuntime


def make_test_policy_runtime(workspace_root: Path) -> PolicyRuntime:
    return PolicyRuntime(
        PolicyConfig(
            workspace_root=workspace_root,
            approval_mode=ApprovalMode.NON_INTERACTIVE,
        )
    )


def runtime_default_policy_runtime(workspace_root: Path) -> PolicyRuntime:
    return PolicyRuntime(PolicyConfig.runtime_default(workspace_root))

