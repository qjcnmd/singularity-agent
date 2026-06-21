from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

from singularity.code_index.models import FileRecord, FreshnessStatus
from singularity.code_index.scanner import WorkspaceScanner


@dataclass(frozen=True)
class FreshnessCheck:
    path: str
    status: FreshnessStatus
    reasons: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, object]:
        return {
            "path": self.path,
            "status": self.status.value,
            "reasons": self.reasons,
        }


class FreshnessManager:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        schema_version: str,
        plugin_versions: dict[str, str] | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.schema_version = schema_version
        self.plugin_versions = plugin_versions or {}
        self.scanner = WorkspaceScanner(self.workspace_root)

    def check_file(self, record: FileRecord) -> FreshnessCheck:
        path = self.workspace_root / record.path
        if not path.exists():
            return FreshnessCheck(record.path, FreshnessStatus.INVALID, ["file_deleted"])
        try:
            current = self.scanner.scan_paths([record.path])[0]
        except Exception:
            return FreshnessCheck(record.path, FreshnessStatus.UNKNOWN, ["scan_failed"])
        reasons: list[str] = []
        if record.sha256 and current.sha256 and record.sha256 != current.sha256:
            reasons.append("sha256_changed")
        if record.mtime_ns != current.mtime_ns and record.sha256 == current.sha256:
            reasons.append("mtime_changed")
        if not record.sha256 or not current.sha256:
            reasons.append("hash_unavailable")
        if reasons:
            return FreshnessCheck(record.path, FreshnessStatus.STALE_CONTENT, reasons)
        return FreshnessCheck(record.path, FreshnessStatus.FRESH, [])

    def stale_paths_for_config_change(self, changed_files: list[str]) -> list[str]:
        config_names = {
            "package.json",
            "pyproject.toml",
            "requirements.txt",
            "setup.cfg",
            "setup.py",
            "tox.ini",
            "tsconfig.json",
            "Cargo.toml",
            "Cargo.lock",
        }
        if any(Path(path).name in config_names for path in changed_files):
            return ["*"]
        return []
