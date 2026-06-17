from __future__ import annotations

from pathlib import Path

from miniharness.instructions.config import InstructionRuntimeConfig
from miniharness.instructions.exceptions import InstructionSourceError
from miniharness.instructions.models import (
    InstructionPriority,
    InstructionScope,
    InstructionSource,
    InstructionSourceType,
    TrustLevel,
    _new_id,
)


class ProjectInstructionLoader:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        config: InstructionRuntimeConfig | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.config = config or InstructionRuntimeConfig()

    def load(self) -> list[InstructionSource]:
        if not self.config.enable_project_instructions:
            return []
        sources: list[InstructionSource] = []
        for filename in self.config.project_instruction_filenames:
            path = self._resolve_inside_workspace(filename)
            if not path.exists() or not path.is_file():
                continue
            content, truncated = self._read_limited(path)
            sources.append(
                InstructionSource(
                    source_id=_new_id("project_instruction"),
                    source_type=InstructionSourceType.PROJECT_INSTRUCTION_FILE,
                    origin=str(path),
                    priority=InstructionPriority.PROJECT_INSTRUCTION,
                    trust_level=TrustLevel.PROJECT_DECLARED,
                    scope=InstructionScope(applies_to_runtime=["model", "planner"]),
                    content=content,
                    metadata={
                        "path": str(path.relative_to(self.workspace_root)),
                        "truncated": truncated,
                        "max_bytes": self.config.max_project_instruction_bytes,
                    },
                )
            )
        return sources

    def _resolve_inside_workspace(self, filename: str) -> Path:
        candidate = (self.workspace_root / filename).resolve(strict=False)
        try:
            candidate.relative_to(self.workspace_root)
        except ValueError as exc:
            raise InstructionSourceError(
                f"Project instruction path escapes workspace: {filename}"
            ) from exc
        return candidate

    def _read_limited(self, path: Path) -> tuple[str, bool]:
        limit = self.config.max_project_instruction_bytes
        data = path.read_bytes()
        truncated = len(data) > limit
        if truncated:
            data = data[:limit]
        return data.decode("utf-8", errors="replace"), truncated
