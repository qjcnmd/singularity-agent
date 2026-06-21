from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from pathlib import Path


class ApprovalMode(str, Enum):
    INTERACTIVE = "interactive"
    REVIEW_ALL = "review_all"
    AUTO_SAFE = "auto_safe"
    READ_ONLY = "read_only"
    NON_INTERACTIVE = "non_interactive"


class SecurityMode(str, Enum):
    STRICT = "strict"
    COMPAT = "compat"


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
    security_mode: SecurityMode | str = SecurityMode.STRICT

    def __post_init__(self) -> None:
        object.__setattr__(self, "approval_mode", _mode(self.approval_mode))
        object.__setattr__(self, "security_mode", _security_mode(self.security_mode))
        root = Path(self.workspace_root).expanduser().resolve(strict=False)
        object.__setattr__(self, "workspace_root", root)
        if self.audit_log_path is None:
            policy_dir = root / ".miniharness" / "policy"
            object.__setattr__(
                self,
                "audit_log_path",
                policy_dir / "audit.jsonl",
            )
        else:
            object.__setattr__(self, "audit_log_path", Path(self.audit_log_path))
        if self.approval_grants_path is None:
            object.__setattr__(
                self,
                "approval_grants_path",
                root / ".miniharness" / "policy" / "approval_grants.jsonl",
            )
        else:
            object.__setattr__(self, "approval_grants_path", Path(self.approval_grants_path))

    @classmethod
    def runtime_default(cls, workspace_root: Path | str) -> "PolicyConfig":
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
