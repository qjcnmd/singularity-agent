from __future__ import annotations

import shlex
import time
from contextlib import suppress
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Protocol

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.protocols import TraceEmitterProtocol
from singularity.policy import PermissionProfile
from singularity.sandbox.backends import SandboxBackend, default_sandbox_backends
from singularity.sandbox.exceptions import SandboxCapabilityError
from singularity.sandbox.models import (
    SandboxFilesystemMode,
    SandboxNetworkMode,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
)
from singularity.sandbox.trace_recorder import SandboxJsonlTraceRecorder


class SandboxAppendTraceRecorderProtocol(Protocol):
    def append(
        self,
        *,
        prepared: Any | None,
        result: SandboxResult,
        capabilities: Any | None = None,
        request: SandboxRequest | None = None,
    ) -> None:
        ...


class SandboxManager:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        backends: list[SandboxBackend] | None = None,
        trace: SandboxAppendTraceRecorderProtocol | TraceEmitterProtocol | None = None,
        permission_profile: PermissionProfile | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.backends = backends if backends is not None else default_sandbox_backends()
        self.trace = trace or SandboxJsonlTraceRecorder.create(self.workspace_root)
        self.permission_profile = permission_profile or PermissionProfile.default_for_workspace(
            self.workspace_root
        )

    def run(self, request: SandboxRequest) -> SandboxResult:
        self._throw_if_cancelled()
        started = time.perf_counter()
        backend: SandboxBackend | None = None
        prepared = None
        self._emit_trace(
            TraceEventType.SANDBOX_REQUESTED,
            request=request,
            summary=f"Sandbox requested for {request.sandbox_id}.",
        )
        protected_reason = self._protected_path_violation(request)
        if protected_reason is not None:
            result = self._blocked(request, protected_reason, started)
            self._record_trace(
                prepared=None,
                result=result,
                capabilities=None,
                request=request,
            )
            return result
        try:
            backend = self._select_backend(request)
            if backend is None:
                result = self._unavailable(
                    request,
                    self._backend_unavailable_reason(),
                    started,
                    backend_name=self.backends[0].name() if self.backends else "unavailable",
                )
                self._record_trace(
                    prepared=None,
                    result=result,
                    capabilities=None,
                    request=request,
                )
                return result
            self.ensure_capabilities(request, backend)
            self._throw_if_cancelled()
            prepared = backend.prepare(request)
            self._throw_if_cancelled()
            self._emit_trace(
                TraceEventType.SANDBOX_PREPARED,
                request=request,
                result=None,
                summary=f"Sandbox prepared with {backend.name()}.",
                payload={
                    "sandbox_id": prepared.sandbox_id,
                    "backend": prepared.backend_name,
                    "sandbox_handle": _relative_handle(prepared.sandbox_root, self.workspace_root),
                    "timing": dict(prepared.baseline.get("timing") or {}),
                },
            )
            self._emit_trace(
                TraceEventType.SANDBOX_STARTED,
                request=request,
                result=None,
                summary=f"Sandbox command started in {prepared.sandbox_id}.",
            )
            result = backend.run(prepared)
            self._throw_if_cancelled()
            try:
                cleanup_started = time.perf_counter()
                backend.cleanup(prepared)
                result.metadata.setdefault("timing", {})["run_root_cleanup_time_seconds"] = (
                    time.perf_counter() - cleanup_started
                )
                result.cleanup_status = "cleaned"
                self._emit_trace(
                    TraceEventType.SANDBOX_CLEANED,
                    request=request,
                    result=result,
                    summary=f"Sandbox cleaned for {result.sandbox_id}.",
                )
            except Exception as exc:
                result.cleanup_status = "cleanup_failed"
                result.metadata["cleanup_error"] = str(exc)
                if result.status == SandboxStatus.SUCCESS:
                    result.status = SandboxStatus.CLEANUP_FAILED
            self._record_trace(
                prepared=prepared,
                result=result,
                capabilities=backend.capabilities(),
            )
            return result
        except SandboxCapabilityError as exc:
            result = self._unavailable(
                request,
                str(exc),
                started,
                backend_name=backend.name() if backend is not None else "unavailable",
            )
            self._emit_trace(
                TraceEventType.SANDBOX_CAPABILITY_FAILED,
                request=request,
                result=result,
                summary=str(exc),
                severity=TraceSeverity.WARNING,
            )
            self._record_trace(
                prepared=prepared,
                result=result,
                capabilities=backend.capabilities() if backend is not None else None,
                request=request,
            )
            return result
        except Exception as exc:
            if _is_cancellation_error(exc):
                if prepared is not None and backend is not None:
                    with suppress(Exception):
                        backend.cleanup(prepared)
                raise
            result = SandboxResult(
                sandbox_id=request.sandbox_id,
                backend_name=backend.name() if backend is not None else "unavailable",
                status=SandboxStatus.SETUP_FAILED,
                exit_code=None,
                stdout="",
                stderr=str(exc),
                started_at=_now(),
                ended_at=_now(),
                duration_ms=int((time.perf_counter() - started) * 1000),
                cleanup_status="not_started",
                metadata={"error_code": "sandbox_setup_failed"},
            )
            self._record_trace(
                prepared=prepared,
                result=result,
                capabilities=backend.capabilities() if backend is not None else None,
                request=request,
            )
            return result

    def ensure_capabilities(self, request: SandboxRequest, backend: SandboxBackend) -> None:
        capabilities = backend.capabilities()
        if request.profile.network.require_hard_isolation and not capabilities.network_isolation:
            raise SandboxCapabilityError("Backend cannot enforce required network isolation.")
        if (
            request.profile.network.mode == SandboxNetworkMode.DENIED
            and not capabilities.network_isolation
        ):
            raise SandboxCapabilityError(
                f"Backend {backend.name()} cannot enforce denied network mode."
            )
        if (
            request.profile.filesystem.mode == SandboxFilesystemMode.READ_ONLY_WORKSPACE
            and not capabilities.readonly_mount
        ):
            raise SandboxCapabilityError("Backend cannot enforce read-only workspace.")
        resources = request.profile.resources
        if (
            resources.max_memory_mb is not None or resources.memory_limit is not None
        ) and not capabilities.memory_limit:
            raise SandboxCapabilityError("Backend cannot enforce memory limits.")
        if (
            resources.max_processes is not None or resources.pids_limit is not None
        ) and not capabilities.process_limit:
            raise SandboxCapabilityError("Backend cannot enforce process limits.")

    def capability_summary(self) -> dict[str, Any]:
        backends: dict[str, dict[str, Any]] = {}
        for backend in self.backends:
            if self._backend_available(backend):
                backends[backend.name()] = backend.capabilities().to_dict()
        return {
            "backend_status": "available" if backends else "backend_unavailable",
            "available_backends": sorted(backends),
            "capabilities": backends,
        }

    def _select_backend(self, request: SandboxRequest) -> SandboxBackend | None:
        first_capability_error: SandboxCapabilityError | None = None
        for backend in self.backends:
            if not self._backend_available(backend):
                continue
            try:
                self.ensure_capabilities(request, backend)
            except SandboxCapabilityError as exc:
                first_capability_error = first_capability_error or exc
                continue
            return backend
        if first_capability_error is not None:
            raise first_capability_error
        return None

    @staticmethod
    def _backend_available(backend: SandboxBackend) -> bool:
        probe = getattr(backend, "is_available", None)
        if not callable(probe):
            return True
        try:
            return bool(probe())
        except Exception:
            return False

    def _backend_unavailable_reason(self) -> str:
        reasons: list[str] = []
        for backend in self.backends:
            doctor = getattr(backend, "doctor", None)
            if callable(doctor):
                try:
                    report = doctor()
                    reason = getattr(report, "reason", None)
                    if reason:
                        reasons.append(str(reason))
                except Exception as exc:
                    reasons.append(f"{backend.name()}: capability probe failed: {exc}")
        if reasons:
            return "; ".join(reasons)
        return "backend_unavailable: no available OS sandbox backend is registered."

    def shutdown(self) -> None:
        return None

    def _unavailable(
        self,
        request: SandboxRequest,
        reason: str,
        started: float,
        *,
        backend_name: str,
    ) -> SandboxResult:
        return SandboxResult(
            sandbox_id=request.sandbox_id,
            backend_name=backend_name,
            status=SandboxStatus.BACKEND_UNAVAILABLE,
            exit_code=None,
            stdout="",
            stderr=reason,
            started_at=_now(),
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            cleanup_status="not_started",
            metadata={"error_code": "backend_unavailable", "reason": reason},
        )

    def _blocked(
        self,
        request: SandboxRequest,
        reason: str,
        started: float,
    ) -> SandboxResult:
        return SandboxResult(
            sandbox_id=request.sandbox_id,
            backend_name="policy",
            status=SandboxStatus.POLICY_BLOCKED,
            exit_code=None,
            stdout="",
            stderr=reason,
            started_at=_now(),
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            cleanup_status="not_started",
            metadata={"error_code": "protected_path_denied"},
        )

    def _protected_path_violation(self, request: SandboxRequest) -> str | None:
        candidates: list[tuple[Path, str]] = [(request.cwd, "execute")]
        for value in (
            *request.profile.filesystem.writable_paths,
            *request.profile.filesystem.readonly_paths,
        ):
            raw = Path(value).expanduser()
            path = raw if raw.is_absolute() else request.workspace_root / raw
            access = "write" if value in request.profile.filesystem.writable_paths else "read"
            candidates.append((path, access))
        resources = request.metadata.get("resources")
        if isinstance(resources, list):
            for value in resources:
                if not isinstance(value, str):
                    continue
                raw = Path(value).expanduser()
                candidates.append(
                    (raw if raw.is_absolute() else request.workspace_root / raw, "execute")
                )
        command = request.command if isinstance(request.command, list) else _split_command(request.command)
        for token in command[1:]:
            if not _looks_like_path(token):
                continue
            raw = Path(token).expanduser()
            candidates.append(
                (raw if raw.is_absolute() else request.cwd / raw, "execute")
            )
        for path, access in candidates:
            rule = self.permission_profile.matching_protected_rule(path, access=access)
            if rule is not None and rule.hard_deny:
                return "Protected path access denied before sandbox execution."
        return None

    def _record_trace(
        self,
        *,
        prepared: Any | None,
        result: SandboxResult,
        capabilities: Any | None,
        request: SandboxRequest | None = None,
    ) -> None:
        append = getattr(self.trace, "append", None)
        if callable(append):
            append(
                prepared=prepared,
                result=result,
                capabilities=capabilities,
                request=request,
            )
        request = prepared.request if prepared is not None else request
        if result.violations or result.status == SandboxStatus.VIOLATION:
            self._emit_trace(
                TraceEventType.SANDBOX_VIOLATION,
                request=request,
                result=result,
                summary=f"Sandbox violation in {result.sandbox_id}.",
                severity=TraceSeverity.ERROR,
            )
        self._emit_trace(
            TraceEventType.SANDBOX_COMPLETED,
            request=request,
            result=result,
            summary=f"Sandbox completed with status {result.status.value}.",
            severity=(TraceSeverity.INFO if result.status == SandboxStatus.SUCCESS else TraceSeverity.WARNING),
        )

    def _emit_trace(
        self,
        event_type: TraceEventType,
        *,
        request: SandboxRequest | None,
        summary: str,
        result: SandboxResult | None = None,
        payload: dict[str, Any] | None = None,
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> None:
        emit = getattr(self.trace, "emit", None)
        if not callable(emit):
            return
        emit(
            event_type,
            component="sandbox",
            summary=summary,
            payload=payload
            or {
                "sandbox_id": result.sandbox_id if result else request.sandbox_id if request else None,
                "backend": result.backend_name if result else None,
                "status": result.status.value if result else None,
                "exit_code": result.exit_code if result else None,
                "duration_ms": result.duration_ms if result else None,
                "artifact_count": len(result.artifacts) if result else 0,
                "changed_files": result.filesystem_changes.to_dict() if result else {},
                "violations": [item.to_dict() for item in result.violations] if result else [],
                "timing": dict(result.metadata.get("timing") or {}) if result else {},
            },
            ids={
                "session_id": request.session_id if request else None,
                "task_id": request.task_id if request else None,
                "action_id": request.action_id if request else None,
                "sandbox_id": result.sandbox_id if result else request.sandbox_id if request else None,
                "policy_decision_id": request.policy_decision_id if request else None,
                "command_id": (request.metadata or {}).get("command_id") if request else None,
            },
            severity=severity,
            artifact_refs=[artifact.artifact_id for artifact in result.artifacts] if result else [],
        )

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _is_cancellation_error(exc: BaseException) -> bool:
    return type(exc).__name__ == "CancellationError" and getattr(exc, "code", None) == "cancelled"


def _relative_handle(path: Path, root: Path) -> str:
    try:
        return path.resolve(strict=False).relative_to(root.resolve(strict=False)).as_posix() or "."
    except ValueError:
        return path.name


def _split_command(command: str) -> list[str]:
    try:
        return shlex.split(command, posix=False)
    except ValueError:
        return command.split()


def _looks_like_path(value: str) -> bool:
    token = value.strip("'\";,()[]{}")
    if not token or token.startswith("-") or "://" in token:
        return False
    normalized = token.replace("\\", "/")
    return (
        "/" in normalized
        or normalized.startswith(".")
        or Path(token).suffix.lower()
        in {".env", ".json", ".pem", ".key", ".pfx", ".p12"}
    )
