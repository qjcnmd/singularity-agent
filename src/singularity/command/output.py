from __future__ import annotations

import hashlib
import re
from dataclasses import dataclass
from pathlib import Path
from threading import Lock

from singularity.command.models import ResourceLimits
from singularity.command.policy import is_secret_env_name


SECRET_VALUE_RE = re.compile(
    r"(?i)\b([A-Z0-9_]*(?:TOKEN|KEY|SECRET|PASSWORD|DSN|CONN_STR|CONN_STRING|CONNECTION_STRING)|DATABASE_URL|AWS_[A-Z0-9_]+|GITHUB_TOKEN|OPENAI_API_KEY)=([^\s]+)"
)


class SecretRedactor:
    def __init__(self) -> None:
        self._literal_values: set[str] = set()
        self.redaction_count = 0

    def add_env_values(self, env: dict[str, str]) -> None:
        for name, value in env.items():
            if value and is_secret_env_name(name):
                self._literal_values.add(value)

    def add_literal(self, value: str | None) -> None:
        if value:
            self._literal_values.add(value)

    def redact(self, text: str) -> str:
        redacted = text
        for value in sorted(self._literal_values, key=len, reverse=True):
            if value and value in redacted:
                redacted = redacted.replace(value, "[REDACTED]")
                self.redaction_count += 1

        def replace_match(match: re.Match[str]) -> str:
            self.redaction_count += 1
            return f"{match.group(1)}=[REDACTED]"

        return SECRET_VALUE_RE.sub(replace_match, redacted)


@dataclass(frozen=True)
class OutputSnapshot:
    stdout_preview: str
    stderr_preview: str
    combined_output_preview: str
    stdout_bytes: int
    stderr_bytes: int
    output_truncated: bool
    output_digest: str
    artifact_path: str | None
    secret_redactions: int


class OutputCollector:
    def __init__(
        self,
        *,
        workspace_root: Path,
        command_id: str,
        limits: ResourceLimits,
        redactor: SecretRedactor,
    ) -> None:
        self.workspace_root = workspace_root
        self.command_id = command_id
        self.limits = limits
        self.redactor = redactor
        self._stdout = ""
        self._stderr = ""
        self._combined = ""
        self._stdout_bytes = 0
        self._stderr_bytes = 0
        self._truncated = False
        self._has_artifact = False
        self._artifact_last_stream: str | None = None
        self._lock = Lock()
        self._artifact_path = (
            workspace_root / ".singularity" / "artifacts" / "commands" / f"{command_id}.log"
        )
        self._artifact_relative = self._artifact_path.relative_to(workspace_root).as_posix()
        self._digest = hashlib.sha256()
        self._read_offsets = {"stdout": 0, "stderr": 0, "combined": 0}

    def add(self, stream: str, raw: bytes) -> None:
        if not raw:
            return
        text = raw.decode("utf-8", errors="replace")
        text = self.redactor.redact(text)
        encoded_len = len(text.encode("utf-8"))
        with self._lock:
            self._digest.update(f"{stream}:".encode("utf-8"))
            self._digest.update(text.encode("utf-8"))
            self._write_artifact(stream, text)
            if stream == "stdout":
                self._stdout_bytes += encoded_len
                self._stdout = self._append_limited(
                    self._stdout,
                    text,
                    self.limits.max_stdout_bytes,
                )
            else:
                self._stderr_bytes += encoded_len
                self._stderr = self._append_limited(
                    self._stderr,
                    text,
                    self.limits.max_stderr_bytes,
                )
            self._combined = self._append_limited(
                self._combined,
                text,
                self.limits.max_combined_output_bytes,
            )

    def snapshot(self) -> OutputSnapshot:
        with self._lock:
            stdout = self.redactor.redact(self._stdout)
            stderr = self.redactor.redact(self._stderr)
            combined = self.redactor.redact(self._combined)
            return OutputSnapshot(
                stdout_preview=stdout,
                stderr_preview=stderr,
                combined_output_preview=combined,
                stdout_bytes=self._stdout_bytes,
                stderr_bytes=self._stderr_bytes,
                output_truncated=self._truncated,
                output_digest=self._digest.hexdigest(),
                artifact_path=self._artifact_relative if self._has_artifact else None,
                secret_redactions=self.redactor.redaction_count,
            )

    def read_since_last(self) -> OutputSnapshot:
        with self._lock:
            stdout = self._stdout[self._read_offsets["stdout"] :]
            stderr = self._stderr[self._read_offsets["stderr"] :]
            combined = self._combined[self._read_offsets["combined"] :]
            self._read_offsets = {
                "stdout": len(self._stdout),
                "stderr": len(self._stderr),
                "combined": len(self._combined),
            }
            stdout = self.redactor.redact(stdout)
            stderr = self.redactor.redact(stderr)
            combined = self.redactor.redact(combined)
            return OutputSnapshot(
                stdout_preview=stdout,
                stderr_preview=stderr,
                combined_output_preview=combined,
                stdout_bytes=len(stdout.encode("utf-8")),
                stderr_bytes=len(stderr.encode("utf-8")),
                output_truncated=self._truncated,
                output_digest=self._digest.hexdigest(),
                artifact_path=self._artifact_relative if self._has_artifact else None,
                secret_redactions=self.redactor.redaction_count,
            )

    def _append_limited(self, current: str, text: str, limit: int) -> str:
        if len(current.encode("utf-8")) >= limit:
            self._truncated = True
            return current
        combined = current + text
        encoded = combined.encode("utf-8")
        if len(encoded) <= limit:
            return combined
        self._truncated = True
        return encoded[:limit].decode("utf-8", errors="replace")

    def _write_artifact(self, stream: str, text: str) -> None:
        if not self._has_artifact and not self._should_materialize_artifact(stream, text):
            return
        self._artifact_path.parent.mkdir(parents=True, exist_ok=True)
        if not self._has_artifact:
            with self._artifact_path.open("w", encoding="utf-8") as file:
                if self._stdout:
                    file.write(f"[stdout] {self._stdout}")
                    self._artifact_last_stream = "stdout"
                if self._stderr:
                    file.write(f"[stderr] {self._stderr}")
                    self._artifact_last_stream = "stderr"
        with self._artifact_path.open("a", encoding="utf-8") as file:
            if self._artifact_last_stream == stream:
                file.write(text)
            else:
                file.write(f"[{stream}] {text}")
                self._artifact_last_stream = stream
        self._has_artifact = True

    def _should_materialize_artifact(self, stream: str, text: str) -> bool:
        text_bytes = len(text.encode("utf-8"))
        current = self._stdout_bytes + self._stderr_bytes + text_bytes
        stream_bytes = self._stdout_bytes if stream == "stdout" else self._stderr_bytes
        stream_limit = (
            self.limits.max_stdout_bytes
            if stream == "stdout"
            else self.limits.max_stderr_bytes
        )
        return (
            current > self.limits.max_combined_output_bytes
            or stream_bytes + text_bytes > stream_limit
        )
