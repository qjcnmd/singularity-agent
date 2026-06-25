from __future__ import annotations

import hashlib
from pathlib import Path
from uuid import uuid4

from singularity.context.redaction import ContextRedactor
from singularity.sandbox.models import (
    SandboxArtifact,
    SandboxResourceLimits,
)


class SandboxArtifactCollector:
    def __init__(self, redactor: ContextRedactor | None = None) -> None:
        self.redactor = redactor or ContextRedactor()

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
                self._write_file_artifact(
                    sandbox_id=sandbox_id,
                    root=artifact_root,
                    workspace_root=workspace_root,
                    source=candidate,
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
            redacted=True,
        )

    def _write_file_artifact(
        self,
        *,
        sandbox_id: str,
        root: Path,
        workspace_root: Path,
        source: Path,
    ) -> SandboxArtifact:
        raw_bytes = source.read_bytes()
        relative_path = source.relative_to(workspace_root).as_posix()
        try:
            text = raw_bytes.decode("utf-8")
        except UnicodeDecodeError:
            stored_bytes = raw_bytes
            redacted = False
        else:
            stored_bytes = self.redactor.redact_text(text).encode("utf-8")
            redacted = True
        target_dir = root / "files"
        target_dir.mkdir(parents=True, exist_ok=True)
        target_path = target_dir / source.name
        counter = 0
        while target_path.exists():
            counter += 1
            target_path = (
                target_dir / f"{source.stem}_{counter}{source.suffix}"
            )
        target_path.write_bytes(stored_bytes)
        return SandboxArtifact(
            artifact_id=f"artifact_{uuid4().hex[:12]}",
            sandbox_id=sandbox_id,
            path=target_path,
            relative_path=relative_path,
            size_bytes=len(stored_bytes),
            kind=self._kind_for(source),
            sha256=hashlib.sha256(stored_bytes).hexdigest(),
            metadata={},
            redacted=redacted,
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
