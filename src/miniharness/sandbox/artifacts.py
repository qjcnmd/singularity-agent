from __future__ import annotations

import hashlib
from pathlib import Path
from uuid import uuid4

from miniharness.sandbox.models import (
    SandboxArtifact,
    SandboxResourceLimits,
)


class SandboxArtifactCollector:
    def collect(
        self,
        *,
        sandbox_id: str,
        workspace_root: Path,
        artifact_root: Path,
        artifact_paths: list[str],
        limits: SandboxResourceLimits,
        stdout: str = "",
        stderr: str = "",
    ) -> list[SandboxArtifact]:
        artifacts: list[SandboxArtifact] = []
        budget = limits.max_artifact_bytes
        used = 0
        if stdout:
            artifacts.append(
                self._write_text_artifact(
                    sandbox_id=sandbox_id,
                    root=artifact_root,
                    relative_path="stdout.log",
                    text=stdout,
                    kind="log",
                )
            )
        if stderr:
            artifacts.append(
                self._write_text_artifact(
                    sandbox_id=sandbox_id,
                    root=artifact_root,
                    relative_path="stderr.log",
                    text=stderr,
                    kind="log",
                )
            )
        used = sum(artifact.size_bytes for artifact in artifacts)
        for raw_path in artifact_paths:
            candidate = (workspace_root / raw_path).resolve(strict=False)
            if not self._inside(candidate, workspace_root):
                continue
            if not candidate.exists() or not candidate.is_file():
                continue
            size = candidate.stat().st_size
            if budget is not None and used + size > budget:
                continue
            artifacts.append(
                SandboxArtifact(
                    artifact_id=f"artifact_{uuid4().hex[:12]}",
                    sandbox_id=sandbox_id,
                    path=candidate,
                    relative_path=candidate.relative_to(workspace_root).as_posix(),
                    size_bytes=size,
                    kind=self._kind_for(candidate),
                    sha256=hashlib.sha256(candidate.read_bytes()).hexdigest(),
                    metadata={},
                )
            )
            used += size
        return artifacts

    @staticmethod
    def _write_text_artifact(
        *,
        sandbox_id: str,
        root: Path,
        relative_path: str,
        text: str,
        kind: str,
    ) -> SandboxArtifact:
        root.mkdir(parents=True, exist_ok=True)
        path = root / relative_path
        data = text.encode("utf-8")
        path.write_bytes(data)
        return SandboxArtifact(
            artifact_id=f"artifact_{uuid4().hex[:12]}",
            sandbox_id=sandbox_id,
            path=path,
            relative_path=f"artifacts/{relative_path}",
            size_bytes=len(data),
            kind=kind,
            sha256=hashlib.sha256(data).hexdigest(),
            metadata={},
        )

    @staticmethod
    def _kind_for(path: Path) -> str:
        lowered = path.name.lower()
        if "coverage" in lowered:
            return "coverage"
        if lowered.endswith((".log", ".txt")):
            return "log"
        if lowered.endswith((".xml", ".json", ".html")):
            return "report"
        return "generic"

    @staticmethod
    def _inside(child: Path, parent: Path) -> bool:
        try:
            child.relative_to(parent.resolve(strict=False))
            return True
        except ValueError:
            return False
