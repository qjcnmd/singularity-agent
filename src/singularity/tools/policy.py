from __future__ import annotations

from dataclasses import dataclass, field

from singularity.tools.models import PermissionLevel, ToolError, ToolSpec


@dataclass(frozen=True)
class ToolPolicy:
    allowed_permissions: frozenset[PermissionLevel] = field(
        default_factory=lambda: frozenset({PermissionLevel.READ_ONLY})
    )
    denied_risk_tags: frozenset[str] = field(
        default_factory=lambda: frozenset({"write", "shell", "git", "network"})
    )

    @classmethod
    def read_only(cls) -> "ToolPolicy":
        return cls()

    @classmethod
    def coding_agent(cls) -> "ToolPolicy":
        return cls(
            allowed_permissions=frozenset(
                {
                    PermissionLevel.READ_ONLY,
                    PermissionLevel.WRITE,
                    PermissionLevel.SHELL,
                }
            ),
            denied_risk_tags=frozenset({"raw_shell", "git", "network"}),
        )

    def check(self, spec: ToolSpec) -> ToolError | None:
        if spec.permission_level not in self.allowed_permissions:
            return ToolError(
                code="permission_denied",
                message=f"Tool permission is not allowed: {spec.permission_level.value}",
                details={"permission_level": spec.permission_level.value},
            )

        denied_tags = sorted(set(spec.risk_tags) & set(self.denied_risk_tags))
        if denied_tags:
            return ToolError(
                code="policy_denied",
                message=f"Tool risk tags are denied: {', '.join(denied_tags)}",
                details={"risk_tags": denied_tags},
            )
        return None
