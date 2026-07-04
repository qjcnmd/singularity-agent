from __future__ import annotations

import json
import os
import socket
import time
from contextlib import contextmanager, suppress
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from singularity.kernel.exceptions import WorkspaceLockError

GUARD_ACQUIRE_TIMEOUT_SECONDS = 5.0
GUARD_RETRY_INTERVAL_SECONDS = 0.01
GUARD_RELEASE_TIMEOUT_SECONDS = 1.0
STALE_GUARD_AGE_SECONDS = 30


@dataclass(frozen=True)
class WorkspaceLockHandle:
    run_id: str
    read_only: bool
    lock_path: Path
    pid: int
    hostname: str


class WorkspaceLockManager:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        lock_path: Path | str | None = None,
        stale_after_seconds: int = 3600,
    ) -> None:
        self.workspace_root = Path(workspace_root).expanduser().resolve(strict=False)
        self.lock_path = Path(lock_path) if lock_path else self.workspace_root / ".singularity" / "locks" / "workspace.lock"
        self.guard_path = self.lock_path.with_suffix(self.lock_path.suffix + ".guard")
        self.stale_after_seconds = stale_after_seconds
        self._handle: WorkspaceLockHandle | None = None
        self.last_stale_lock_detected = False

    @property
    def acquired(self) -> bool:
        return self._handle is not None

    def acquire_lock(self, *, run_id: str, read_only: bool = False) -> WorkspaceLockHandle:
        self.lock_path.parent.mkdir(parents=True, exist_ok=True)
        with self._guard():
            stale_payload = self._read_payload() if self.lock_path.exists() else None
            self.last_stale_lock_detected = self._payload_stale(stale_payload)
            if self.lock_path.exists() and self.last_stale_lock_detected:
                self.lock_path.unlink(missing_ok=True)
            payload = self._read_payload()
            holders = list(payload.get("holders") or []) if payload else []
            existing_write = bool(payload and payload.get("mode") == "write")
            if holders and (existing_write or not read_only):
                raise WorkspaceLockError(
                    "Workspace is already locked.",
                    code="workspace_locked",
                    details={"lock_path": str(self.lock_path), "holders": holders},
                )
            holder = {
                "run_id": run_id,
                "pid": os.getpid(),
                "hostname": socket.gethostname(),
                "acquired_at": _now(),
                "updated_at": _now(),
                "read_only": read_only,
            }
            holder_pid = os.getpid()
            holder_hostname = socket.gethostname()
            holders.append(holder)
            self._write_payload(
                {
                    "version": 1,
                    "mode": "read" if read_only else "write",
                    "holders": holders,
                }
            )
            self._handle = WorkspaceLockHandle(
                run_id=run_id,
                read_only=read_only,
                lock_path=self.lock_path,
                pid=holder_pid,
                hostname=holder_hostname,
            )
            return self._handle

    def release_lock(self) -> None:
        if self._handle is None:
            return
        with self._guard():
            payload = self._read_payload()
            if payload is None:
                self._handle = None
                return
            holders = [
                holder
                for holder in payload.get("holders") or []
                if holder.get("run_id") != self._handle.run_id
            ]
            if holders:
                payload["holders"] = holders
                payload["mode"] = "write" if any(not holder.get("read_only") for holder in holders) else "read"
                self._write_payload(payload)
            else:
                self.lock_path.unlink(missing_ok=True)
            self._handle = None

    def refresh_lock(self) -> None:
        if self._handle is None:
            return
        with self._guard():
            payload = self._read_payload()
            if payload is None:
                return
            for holder in payload.get("holders") or []:
                if holder.get("run_id") == self._handle.run_id:
                    holder["updated_at"] = _now()
            self._write_payload(payload)

    def detect_stale_lock(self) -> bool:
        payload = self._read_payload()
        stale = self._payload_stale(payload)
        self.last_stale_lock_detected = self.last_stale_lock_detected or stale
        return stale

    def _payload_stale(self, payload: dict[str, Any] | None) -> bool:
        if payload is None:
            return False
        holders = list(payload.get("holders") or [])
        if not holders:
            return True
        return all(self._holder_stale(holder) for holder in holders)

    def _holder_stale(self, holder: dict[str, Any]) -> bool:
        timestamp = holder.get("updated_at") or holder.get("acquired_at")
        try:
            age = datetime.now(UTC) - datetime.fromisoformat(str(timestamp))
        except (TypeError, ValueError):
            return True
        if age.total_seconds() <= self.stale_after_seconds:
            return False
        pid = holder.get("pid")
        return not _pid_exists(pid)

    def _read_payload(self) -> dict[str, Any] | None:
        if not self.lock_path.exists():
            return None
        try:
            return json.loads(self.lock_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise WorkspaceLockError(
                "Workspace lock is unreadable.",
                code="workspace_lock_corrupt",
                details={"lock_path": str(self.lock_path), "error": str(exc)},
            ) from exc

    def _write_payload(self, payload: dict[str, Any]) -> None:
        tmp_path = self.lock_path.with_suffix(".tmp")
        tmp_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2, sort_keys=True), encoding="utf-8")
        os.replace(tmp_path, self.lock_path)

    @contextmanager
    def _guard(self):
        self.lock_path.parent.mkdir(parents=True, exist_ok=True)
        deadline = time.monotonic() + GUARD_ACQUIRE_TIMEOUT_SECONDS
        fd: int | None = None
        while fd is None:
            try:
                fd = os.open(
                    self.guard_path,
                    os.O_CREAT | os.O_EXCL | os.O_WRONLY,
                )
            except FileExistsError:
                if self._guard_is_stale():
                    with suppress(PermissionError):
                        self.guard_path.unlink(missing_ok=True)
                if time.monotonic() >= deadline:
                    raise WorkspaceLockError(
                        "Workspace lock guard is busy.",
                        code="workspace_lock_busy",
                        details={"guard_path": str(self.guard_path)},
                    ) from None
                time.sleep(GUARD_RETRY_INTERVAL_SECONDS)
        try:
            os.write(
                fd,
                json.dumps(
                    {"pid": os.getpid(), "hostname": socket.gethostname(), "created_at": _now()},
                    ensure_ascii=False,
                    sort_keys=True,
                ).encode("utf-8"),
            )
            yield
        finally:
            if fd is not None:
                os.close(fd)
            self._release_guard()

    def _guard_is_stale(self) -> bool:
        try:
            payload = json.loads(self.guard_path.read_text(encoding="utf-8"))
        except PermissionError:
            return False
        except json.JSONDecodeError:
            return True
        except OSError:
            return False
        pid = payload.get("pid")
        created_at = payload.get("created_at")
        try:
            age = datetime.now(UTC) - datetime.fromisoformat(str(created_at))
        except (TypeError, ValueError):
            return True
        return age.total_seconds() > STALE_GUARD_AGE_SECONDS and not _pid_exists(pid)

    def _release_guard(self) -> None:
        deadline = time.monotonic() + GUARD_RELEASE_TIMEOUT_SECONDS
        while True:
            try:
                self.guard_path.unlink(missing_ok=True)
                return
            except PermissionError:
                if time.monotonic() >= deadline:
                    return
                time.sleep(GUARD_RETRY_INTERVAL_SECONDS)


def _pid_exists(pid: Any) -> bool:
    if not isinstance(pid, int) or pid <= 0:
        return False
    if pid == os.getpid():
        return True
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def _now() -> str:
    return datetime.now(UTC).isoformat()
