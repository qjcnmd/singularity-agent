from __future__ import annotations

import hashlib
import mimetypes
import shutil
from pathlib import Path
from typing import Any
from uuid import uuid4

from singularity.observability.exceptions import TraceArtifactError
from singularity.observability.models import TraceArtifact, TraceArtifactKind
from singularity.observability.redaction import TraceRedactor


class TraceArtifactStore:
    def __init__(
        self,
        root: Path | str,
        *,
        run_id: str,
        session_id: str,
        redactor: TraceRedactor | None = None,
        max_artifact_bytes: int = 10 * 1024 * 1024,
        max_total_bytes: int = 100 * 1024 * 1024,
        run_dir: Path | str | None = None,
    ) -> None:
        self.root = Path(root)
        self.run_id = run_id
        self.session_id = session_id
        self.redactor = redactor or TraceRedactor()
        self.max_artifact_bytes = max_artifact_bytes
        self.max_total_bytes = max_total_bytes
        self.run_dir = Path(run_dir).expanduser() if run_dir is not None else (
            self.root / "work" / "traces" / "runs" / run_id
        )
        self.artifact_dir = self.run_dir / "artifacts"
        self.artifact_dir.mkdir(parents=True, exist_ok=True)

    def write_text_artifact(
        self,
        *,
        kind: TraceArtifactKind | str,
        text: str,
        task_id: str | None = None,
        summary: str = "",
        metadata: dict[str, Any] | None = None,
        sensitive: bool = False,
        content_type: str = "text/plain",
    ) -> TraceArtifact:
        output = self.redactor.redact_text(text)
        return self.write_bytes_artifact(
            kind=kind if isinstance(kind, TraceArtifactKind) else TraceArtifactKind(str(kind)),
            data=output.encode("utf-8"),
            task_id=task_id,
            summary=summary,
            metadata=metadata,
            sensitive=sensitive,
            content_type=content_type,
            extension=".txt",
            redacted=True,
        )

    def write_bytes_artifact(
        self,
        *,
        kind: TraceArtifactKind | str,
        data: bytes,
        task_id: str | None = None,
        summary: str = "",
        metadata: dict[str, Any] | None = None,
        sensitive: bool = False,
        content_type: str = "application/octet-stream",
        extension: str = ".bin",
        redacted: bool = True,
    ) -> TraceArtifact:
        self._check_limits(len(data))
        artifact_id = f"artifact_{uuid4().hex[:12]}"
        path = self.artifact_dir / f"{artifact_id}{extension}"
        path.write_bytes(data)
        return self._artifact(
            artifact_id=artifact_id,
            kind=kind if isinstance(kind, TraceArtifactKind) else TraceArtifactKind(str(kind)),
            path=path,
            task_id=task_id,
            content_type=content_type,
            redacted=redacted,
            sensitive=sensitive,
            summary=summary,
            metadata=metadata or {},
        )

    def register_file_artifact(
        self,
        *,
        kind: TraceArtifactKind | str,
        source_path: Path | str,
        task_id: str | None = None,
        summary: str = "",
        metadata: dict[str, Any] | None = None,
        sensitive: bool = False,
    ) -> TraceArtifact:
        source = Path(source_path)
        if not source.is_file():
            raise TraceArtifactError(f"Artifact source is not a file: {source}")
        size = source.stat().st_size
        self._check_limits(size)
        artifact_id = f"artifact_{uuid4().hex[:12]}"
        suffix = source.suffix or ".bin"
        path = self.artifact_dir / f"{artifact_id}{suffix}"
        if sensitive:
            try:
                path.write_text(
                    self.redactor.redact_text(source.read_text(encoding="utf-8")),
                    encoding="utf-8",
                )
            except UnicodeDecodeError as exc:
                raise TraceArtifactError(
                    "Sensitive file artifacts must be text-redactable."
                ) from exc
        else:
            shutil.copyfile(source, path)
        content_type = mimetypes.guess_type(str(source))[0] or "application/octet-stream"
        return self._artifact(
            artifact_id=artifact_id,
            kind=kind if isinstance(kind, TraceArtifactKind) else TraceArtifactKind(str(kind)),
            path=path,
            task_id=task_id,
            content_type=content_type,
            redacted=sensitive,
            sensitive=sensitive,
            summary=summary,
            metadata=metadata or {},
        )

    def read_artifact(self, artifact: TraceArtifact | str) -> bytes:
        artifact_id = artifact.artifact_id if isinstance(artifact, TraceArtifact) else artifact
        matches = list(self.artifact_dir.glob(f"{artifact_id}.*"))
        if not matches:
            raise TraceArtifactError(f"Unknown artifact: {artifact_id}")
        return matches[0].read_bytes()

    def _artifact(
        self,
        *,
        artifact_id: str,
        kind: TraceArtifactKind | str,
        path: Path,
        task_id: str | None,
        content_type: str,
        redacted: bool,
        sensitive: bool,
        summary: str,
        metadata: dict[str, Any],
    ) -> TraceArtifact:
        try:
            relative = path.relative_to(self.run_dir).as_posix()
        except ValueError:
            relative = path.name
        data = path.read_bytes()
        return TraceArtifact(
            artifact_id=artifact_id,
            run_id=self.run_id,
            session_id=self.session_id,
            task_id=task_id,
            kind=kind if isinstance(kind, TraceArtifactKind) else TraceArtifactKind(str(kind)),
            path=path,
            relative_path=relative,
            size_bytes=len(data),
            sha256=hashlib.sha256(data).hexdigest(),
            content_type=content_type,
            redacted=redacted,
            sensitive=sensitive,
            summary=self.redactor.redact_text(summary),
            metadata=self.redactor.redact_payload(metadata),
        )

    def _check_limits(self, size: int) -> None:
        if size > self.max_artifact_bytes:
            raise TraceArtifactError("Trace artifact exceeds max_artifact_bytes.")
        current = sum(path.stat().st_size for path in self.artifact_dir.glob("*") if path.is_file())
        if current + size > self.max_total_bytes:
            raise TraceArtifactError("Trace artifacts exceed max_total_bytes.")
