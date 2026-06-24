from __future__ import annotations

import json
import os
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any


CONFIG_SCHEMA_VERSION = 1
MEMORY_SCHEMA_VERSION = 1
TRACE_SCHEMA_VERSION = 1
EVAL_SCHEMA_VERSION = 1
CURRENT_MIGRATION_VERSION = "001-installation-layout"


@dataclass(frozen=True)
class InstallationManifest:
    app_version: str
    config_schema_version: int = CONFIG_SCHEMA_VERSION
    memory_schema_version: int = MEMORY_SCHEMA_VERSION
    trace_schema_version: int = TRACE_SCHEMA_VERSION
    eval_schema_version: int = EVAL_SCHEMA_VERSION
    last_migration: str = CURRENT_MIGRATION_VERSION
    mode: str = "user"
    created_at: str | None = None
    updated_at: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, payload: dict[str, Any]) -> "InstallationManifest":
        return cls(
            app_version=str(payload.get("app_version") or "0.0.0"),
            config_schema_version=int(payload.get("config_schema_version") or 0),
            memory_schema_version=int(payload.get("memory_schema_version") or 0),
            trace_schema_version=int(payload.get("trace_schema_version") or 0),
            eval_schema_version=int(payload.get("eval_schema_version") or 0),
            last_migration=str(payload.get("last_migration") or "000"),
            mode=str(payload.get("mode") or "user"),
            created_at=payload.get("created_at"),
            updated_at=payload.get("updated_at"),
        )


@dataclass(frozen=True)
class ReleaseCheck:
    name: str
    status: str
    message: str
    suggestion: str | None = None
    details: dict[str, Any] = field(default_factory=dict)

    @property
    def ok(self) -> bool:
        return self.status == "ok"

    def to_dict(self) -> dict[str, Any]:
        payload = asdict(self)
        if self.suggestion is None:
            payload.pop("suggestion")
        if not self.details:
            payload.pop("details")
        return payload


@dataclass(frozen=True)
class ReleaseDoctorReport:
    ok: bool
    checks: list[ReleaseCheck]

    def to_dict(self) -> dict[str, Any]:
        return {"ok": self.ok, "checks": [check.to_dict() for check in self.checks]}

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), ensure_ascii=False, indent=2, sort_keys=True) + "\n"


def atomic_write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with tmp.open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    last_error: PermissionError | None = None
    for attempt in range(8):
        try:
            os.replace(tmp, path)
            break
        except PermissionError as exc:
            last_error = exc
            time.sleep(0.05 * (attempt + 1))
    else:
        if last_error is not None:
            raise last_error
    try:
        directory_fd = os.open(str(path.parent), os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(directory_fd)
    except OSError:
        pass
    finally:
        os.close(directory_fd)


def atomic_write_json(path: Path, payload: dict[str, Any]) -> None:
    atomic_write_text(
        path,
        json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
    )


def read_json(path: Path) -> dict[str, Any]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError(f"Expected JSON object in {path}")
    return payload
