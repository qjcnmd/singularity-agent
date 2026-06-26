from __future__ import annotations

import os
from dataclasses import dataclass
from enum import Enum
from pathlib import Path

from singularity.policy.operator_key import default_operator_key_path


class ApprovalMode(str, Enum):
    INTERACTIVE = "interactive"
    REVIEW_ALL = "review_all"
    AUTO_SAFE = "auto_safe"
    READ_ONLY = "read_only"
    NON_INTERACTIVE = "non_interactive"


class SecurityMode(str, Enum):
    STRICT = "strict"
    COMPAT = "compat"


def _default_policy_home() -> Path:
    """Return the base directory for default policy artifacts.

    Trust boundary: default grant/audit paths must live outside the
    model-writable workspace. ``SINGULARITY_POLICY_HOME`` allows operators
    and tests to redirect the default location without affecting
    ``Path.home()``.
    """
    env_home = os.environ.get("SINGULARITY_POLICY_HOME")
    if env_home:
        return Path(env_home).expanduser()
    return Path.home()


@dataclass(frozen=True)
class PolicyConfig:
    approval_mode: ApprovalMode | str = ApprovalMode.INTERACTIVE
    workspace_root: Path | str = "."
    allow_workspace_reads: bool = True
    allow_workspace_mutation_with_review: bool = True
    allow_command_with_review: bool = True
    allow_network_with_review: bool = True
    allow_package_install_with_review: bool = True
    deny_secret_access_by_default: bool = True
    deny_outside_workspace_write: bool = True
    max_auto_read_bytes: int = 200_000
    default_command_timeout_seconds: int = 30
    audit_log_path: Path | str | None = None
    approval_grants_path: Path | str | None = None
    consumption_ledger_path: Path | str | None = None
    security_mode: SecurityMode | str = SecurityMode.STRICT
    operator_key_path: Path | str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "approval_mode", _mode(self.approval_mode))
        object.__setattr__(self, "security_mode", _security_mode(self.security_mode))
        root = Path(self.workspace_root).expanduser().resolve(strict=False)
        object.__setattr__(self, "workspace_root", root)
        # Trust boundary: audit log and approval grants must default to a
        # location outside the model-writable workspace so the model cannot
        # forge audit entries or approval grants via shell writes. Explicit
        # configuration still overrides the default for backward compatibility.
        home_policy_dir = _default_policy_home() / ".singularity" / "policy"
        if self.audit_log_path is None:
            object.__setattr__(
                self,
                "audit_log_path",
                home_policy_dir / "audit.jsonl",
            )
        else:
            object.__setattr__(self, "audit_log_path", Path(self.audit_log_path))
        if self.approval_grants_path is None:
            object.__setattr__(
                self,
                "approval_grants_path",
                home_policy_dir / "approval_grants.jsonl",
            )
        else:
            object.__setattr__(self, "approval_grants_path", Path(self.approval_grants_path))
        # Trust boundary: consumption ledger defaults to the same
        # outside-workspace policy dir as the grant store. The ledger is the
        # single source of truth for grant consumption (HMAC-chained,
        # append-only) and must not be model-writable.
        if self.consumption_ledger_path is None:
            object.__setattr__(
                self,
                "consumption_ledger_path",
                home_policy_dir / "grant_consumption_ledger.jsonl",
            )
        else:
            object.__setattr__(
                self,
                "consumption_ledger_path",
                Path(self.consumption_ledger_path),
            )
        # Operator key: binds remote approval grant signatures to a secret
        # held outside the workspace. Defaults to
        # ``<policy_home>/.singularity/policy/operator.pem`` so test isolation
        # via ``SINGULARITY_POLICY_HOME`` is respected.
        if self.operator_key_path is None:
            object.__setattr__(
                self,
                "operator_key_path",
                default_operator_key_path(),
            )
        else:
            object.__setattr__(self, "operator_key_path", Path(self.operator_key_path))

    @classmethod
    def default_for_workspace(cls, workspace_root: Path | str) -> "PolicyConfig":
        return cls(workspace_root=workspace_root, approval_mode=ApprovalMode.AUTO_SAFE)


def _mode(value: ApprovalMode | str) -> ApprovalMode:
    if isinstance(value, ApprovalMode):
        return value
    try:
        return ApprovalMode[str(value).upper()]
    except KeyError:
        return ApprovalMode(str(value))


def _security_mode(value: SecurityMode | str) -> SecurityMode:
    if isinstance(value, SecurityMode):
        return value
    try:
        return SecurityMode[str(value).upper()]
    except KeyError:
        return SecurityMode(str(value))
