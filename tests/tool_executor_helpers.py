from __future__ import annotations

from pathlib import Path

from singularity.policy import ApprovalMode, PolicyConfig, PolicyEngine
from singularity.policy.operator_key import generate_operator_key


def make_test_policy_engine(workspace_root: Path) -> PolicyEngine:
    return PolicyEngine(
        PolicyConfig(
            workspace_root=workspace_root,
            approval_mode=ApprovalMode.NON_INTERACTIVE,
        )
    )


def default_policy_engine(workspace_root: Path) -> PolicyEngine:
    return PolicyEngine(PolicyConfig.default_for_workspace(workspace_root))


def make_ledger_test_config(
    workspace_root: Path,
    *,
    grants_path: Path | None = None,
    ledger_path: Path | None = None,
    **overrides: object,
) -> PolicyConfig:
    """Build a PolicyConfig wired for grant-consumption ledger tests.

    Generates a fresh operator key inside ``workspace_root`` so each test
    is fully isolated from the host's ``~/.singularity`` policy home, and
    points both the grant store and the consumption ledger at files inside
    ``workspace_root`` (overridable via ``grants_path`` / ``ledger_path``).
    """
    operator_key_path = workspace_root / "operator.pem"
    if not operator_key_path.exists():
        generate_operator_key(operator_key_path)
    return PolicyConfig(
        workspace_root=workspace_root,
        approval_grants_path=grants_path or (workspace_root / "grants.jsonl"),
        consumption_ledger_path=ledger_path or (workspace_root / "ledger.jsonl"),
        operator_key_path=operator_key_path,
        **overrides,  # type: ignore[arg-type]
    )
