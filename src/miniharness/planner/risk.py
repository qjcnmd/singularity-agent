from __future__ import annotations

from typing import Any

from miniharness.planner.models import RiskDecisionKind, RiskEscalation, RiskLevel


HIGH_RISK_PATH_PARTS = {
    ".env",
    "pyproject.toml",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "uv.lock",
}


class RiskEscalator:
    def evaluate_action(
        self,
        *,
        tool_name: str,
        arguments: dict[str, Any],
        changed_files: list[str],
    ) -> RiskEscalation:
        reasons: list[str] = []
        candidate_paths = list(changed_files)
        path = arguments.get("path")
        if isinstance(path, str):
            candidate_paths.append(path)
        new_path = arguments.get("new_path")
        if isinstance(new_path, str):
            candidate_paths.append(new_path)

        for candidate in candidate_paths:
            normalized = candidate.replace("\\", "/")
            name = normalized.rsplit("/", 1)[-1]
            if normalized.startswith(".github/workflows/") or name in HIGH_RISK_PATH_PARTS:
                reasons.append(f"high-risk file: {candidate}")

        if tool_name in {"workspace_delete_file", "workspace_move_file"}:
            reasons.append(f"high-risk mutation tool: {tool_name}")
        if len(set(candidate_paths)) > 20:
            reasons.append("large changed-file scope")

        if reasons:
            return RiskEscalation(
                decision=RiskDecisionKind.REQUIRE_REVIEW,
                risk_level=RiskLevel.HIGH,
                reasons=reasons,
            )
        return RiskEscalation(
            decision=RiskDecisionKind.CONTINUE,
            risk_level=RiskLevel.LOW,
            reasons=[],
        )
