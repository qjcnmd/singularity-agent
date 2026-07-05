from __future__ import annotations

import os
import shlex
import signal
import subprocess
import time
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from singularity.kernel.cancellation import is_cancellation_error, throw_if_cancelled
from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.protocols import TraceEmitterProtocol
from singularity.observability.redaction import shared_trace_redactor
from singularity.policy import PermissionProfile, PermissionProfileName
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
from singularity.utils.serialization import utc_iso_timestamp

_RELAXED_BACKEND_NAME = "local_process"
_WINDOWS_ELEVATED_BACKEND_NAME = "windows_elevated"
_WINDOWS_UNELEVATED_BACKEND_NAME = "windows_unelevated"
_DANGER_FULL_ACCESS_FALLBACK_REASON = "danger-full-access sandbox mode"
_PROCESS_TERMINATE_TIMEOUT_SECONDS = 2.0


@dataclass(frozen=True)
class _SelectedSandboxBackend:
    backend: SandboxBackend
    capabilities: Any
    metadata: dict[str, Any]


@dataclass(frozen=True)
class _FallbackRun:
    result: SandboxResult
    selected: _SelectedSandboxBackend
    capabilities: Any
    prepared: Any | None


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
        throw_if_cancelled(self)
        started = time.perf_counter()
        backend: SandboxBackend | None = None
        backend_capabilities = None
        selected: _SelectedSandboxBackend | None = None
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
            selected = self._select_backend(request)
            backend = selected.backend if selected is not None else None
            backend_capabilities = selected.capabilities if selected is not None else None
            if backend is None:
                reason = self._backend_unavailable_reason()
                if self._allows_relaxed_local_process():
                    result = self._run_relaxed_local_process(request, started, reason)
                else:
                    result = self._unavailable(
                        request,
                        reason,
                        started,
                        backend_name="unavailable",
                    )
                self._record_trace(
                    prepared=None,
                    result=result,
                    capabilities=None,
                    request=request,
                )
                return result
            self.ensure_capabilities(
                request,
                backend,
                capabilities=backend_capabilities,
                reduced_enforcement_allowed=backend.name()
                == _WINDOWS_UNELEVATED_BACKEND_NAME,
            )
            throw_if_cancelled(self)
            prepared = backend.prepare(request)
            throw_if_cancelled(self)
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
            throw_if_cancelled(self)
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
            if (
                selected is not None
                and result.status == SandboxStatus.BACKEND_UNAVAILABLE
                and backend.name() == _WINDOWS_ELEVATED_BACKEND_NAME
            ):
                fallback = self._run_unelevated_fallback(
                    request,
                    started,
                    elevated_result=result,
                )
                if fallback is not None:
                    self._record_trace(
                        prepared=fallback.prepared,
                        result=fallback.result,
                        capabilities=fallback.capabilities,
                    )
                    return fallback.result
            if selected is not None:
                self._apply_selection_metadata(result, selected)
            self._record_trace(
                prepared=prepared,
                result=result,
                capabilities=backend_capabilities,
            )
            return result
        except SandboxCapabilityError as exc:
            if self._allows_relaxed_local_process():
                result = self._run_relaxed_local_process(request, started, str(exc))
            else:
                result = self._unavailable(
                    request,
                    str(exc),
                    started,
                    backend_name=backend.name() if backend is not None else "unavailable",
                )
            if selected is not None:
                self._apply_selection_metadata(result, selected)
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
                capabilities=self._capabilities_for_trace(backend, backend_capabilities),
                request=request,
            )
            return result
        except Exception as exc:
            if is_cancellation_error(exc):
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
            if selected is not None:
                self._apply_selection_metadata(result, selected)
            self._record_trace(
                prepared=prepared,
                result=result,
                capabilities=self._capabilities_for_trace(backend, backend_capabilities),
                request=request,
            )
            return result

    def ensure_capabilities(
        self,
        request: SandboxRequest,
        backend: SandboxBackend,
        *,
        capabilities: Any | None = None,
        reduced_enforcement_allowed: bool = False,
    ) -> None:
        capabilities = capabilities if capabilities is not None else backend.capabilities()
        if (
            request.profile.network.require_hard_isolation
            and not capabilities.network_isolation
            and not reduced_enforcement_allowed
        ):
            raise SandboxCapabilityError("Backend cannot enforce required network isolation.")
        if (
            request.profile.network.mode == SandboxNetworkMode.DENIED
            and not capabilities.network_isolation
            and not reduced_enforcement_allowed
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

    def _select_backend(self, request: SandboxRequest) -> _SelectedSandboxBackend | None:
        mode = self._sandbox_mode()
        elevated_backend = self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME)
        elevated_available = (
            self._backend_available(elevated_backend)
            if elevated_backend is not None
            else False
        )
        elevated_blocker_summary = (
            ""
            if elevated_available
            else self._backend_reason(elevated_backend)
            if elevated_backend is not None
            else ""
        )
        fallback_reason = elevated_blocker_summary
        ordered = self._ordered_backends()
        first_capability_error: SandboxCapabilityError | None = None
        for backend in ordered:
            backend_name = backend.name()
            if not self._backend_allowed_for_mode(backend_name, mode):
                continue
            if not self._backend_available(backend):
                continue
            capabilities = backend.capabilities()
            reduced = backend_name == _WINDOWS_UNELEVATED_BACKEND_NAME
            try:
                self.ensure_capabilities(
                    request,
                    backend,
                    capabilities=capabilities,
                    reduced_enforcement_allowed=reduced,
                )
            except SandboxCapabilityError as exc:
                first_capability_error = first_capability_error or exc
                continue
            return _SelectedSandboxBackend(
                backend=backend,
                capabilities=capabilities,
                metadata=self._selection_metadata(
                    backend_name,
                    elevated_available=elevated_available,
                    elevated_blocker_summary=elevated_blocker_summary,
                    fallback_reason=fallback_reason,
                ),
            )
        if first_capability_error is not None:
            raise first_capability_error
        return None

    def _ordered_backends(self) -> list[SandboxBackend]:
        by_name = {backend.name(): backend for backend in self.backends}
        ordered: list[SandboxBackend] = []
        for name in (_WINDOWS_ELEVATED_BACKEND_NAME, _WINDOWS_UNELEVATED_BACKEND_NAME):
            backend = by_name.get(name)
            if backend is not None:
                ordered.append(backend)
        ordered.extend(
            backend
            for backend in self.backends
            if backend.name()
            not in {_WINDOWS_ELEVATED_BACKEND_NAME, _WINDOWS_UNELEVATED_BACKEND_NAME}
        )
        return ordered

    def _backend_by_name(self, name: str) -> SandboxBackend | None:
        return next((backend for backend in self.backends if backend.name() == name), None)

    def _backend_allowed_for_mode(self, backend_name: str, mode: str) -> bool:
        if backend_name == _RELAXED_BACKEND_NAME:
            return mode == PermissionProfileName.DANGER_FULL_ACCESS.value
        if mode in {
            PermissionProfileName.READ_ONLY.value,
            PermissionProfileName.WORKSPACE_WRITE.value,
        }:
            return backend_name in {
                _WINDOWS_ELEVATED_BACKEND_NAME,
                _WINDOWS_UNELEVATED_BACKEND_NAME,
            } or not backend_name.startswith("windows_")
        return True

    def _selection_metadata(
        self,
        backend_name: str,
        *,
        elevated_available: bool,
        elevated_blocker_summary: str,
        fallback_reason: str,
    ) -> dict[str, Any]:
        mode = self._sandbox_mode()
        reduced = backend_name == _WINDOWS_UNELEVATED_BACKEND_NAME
        strict = backend_name == _WINDOWS_ELEVATED_BACKEND_NAME or not reduced
        metadata = {
            "sandbox_mode": mode,
            "sandbox_backend": backend_name,
            "sandbox_enforcement": "reduced" if reduced else "strict",
            "enforcement_status": "degraded" if reduced else "available",
            "fallback_used": reduced,
            "fallback_reason": fallback_reason if reduced else "",
            "elevated_available": elevated_available,
            "elevated_blocker_summary": elevated_blocker_summary,
        }
        if strict and backend_name == _WINDOWS_ELEVATED_BACKEND_NAME:
            metadata.setdefault("execution_backend", "account_restricted_token")
        elif reduced:
            metadata.setdefault("execution_backend", "current_user_process")
        return metadata

    @staticmethod
    def _apply_selection_metadata(
        result: SandboxResult,
        selected: _SelectedSandboxBackend,
    ) -> None:
        for key, value in selected.metadata.items():
            if value == "":
                result.metadata.setdefault(key, value)
            else:
                result.metadata[key] = value
        result.metadata.setdefault("sandbox_backend", result.backend_name)
        result.metadata.setdefault("sandbox_mode", selected.metadata["sandbox_mode"])
        result.metadata.setdefault(
            "backend_is_local_process",
            result.backend_name == _RELAXED_BACKEND_NAME,
        )

    def _run_unelevated_fallback(
        self,
        request: SandboxRequest,
        started: float,
        *,
        elevated_result: SandboxResult,
    ) -> _FallbackRun | None:
        backend = self._backend_by_name(_WINDOWS_UNELEVATED_BACKEND_NAME)
        if backend is None or not self._backend_available(backend):
            return None
        capabilities = backend.capabilities()
        try:
            self.ensure_capabilities(
                request,
                backend,
                capabilities=capabilities,
                reduced_enforcement_allowed=True,
            )
        except SandboxCapabilityError:
            return None
        reason = str(
            elevated_result.metadata.get("reason")
            or elevated_result.stderr
            or self._backend_reason(self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME))
            or "native_windows_elevated_sandbox_unavailable"
        )
        selected = _SelectedSandboxBackend(
            backend=backend,
            capabilities=capabilities,
            metadata=self._selection_metadata(
                backend.name(),
                elevated_available=False,
                elevated_blocker_summary=_safe_reason(reason),
                fallback_reason=_safe_reason(reason),
            ),
        )
        prepared = None
        try:
            prepared = backend.prepare(request)
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
            self._apply_selection_metadata(result, selected)
            return _FallbackRun(
                result=result,
                selected=selected,
                capabilities=capabilities,
                prepared=prepared,
            )
        except Exception as exc:
            if is_cancellation_error(exc):
                if prepared is not None:
                    with suppress(Exception):
                        backend.cleanup(prepared)
                raise
            result = SandboxResult(
                sandbox_id=request.sandbox_id,
                backend_name=backend.name(),
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
            self._apply_selection_metadata(result, selected)
            return _FallbackRun(
                result=result,
                selected=selected,
                capabilities=capabilities,
                prepared=prepared,
            )

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
            reason = self._backend_reason(backend)
            if reason:
                reasons.append(reason)
        if reasons:
            return "; ".join(reasons)
        return "backend_unavailable: no available OS sandbox backend is registered."

    @staticmethod
    def _backend_reason(backend: SandboxBackend | None) -> str:
        if backend is None:
            return ""
        doctor = getattr(backend, "doctor", None)
        if callable(doctor):
            try:
                report = doctor()
                reason = getattr(report, "reason", None)
                if reason:
                    return _safe_reason(str(reason))
                diagnostics = getattr(report, "diagnostics", None) or ()
                summary = _diagnostic_summary(diagnostics)
                if summary:
                    return summary
            except Exception as exc:
                return _safe_reason(f"{backend.name()}: capability probe failed: {exc}")
        return ""

    def _allows_relaxed_local_process(self) -> bool:
        return self.permission_profile.profile == PermissionProfileName.DANGER_FULL_ACCESS

    def _sandbox_mode(self) -> str:
        return self.permission_profile.profile.value

    def _run_relaxed_local_process(
        self,
        request: SandboxRequest,
        started: float,
        unavailable_reason: str,
    ) -> SandboxResult:
        self._emit_trace(
            TraceEventType.SANDBOX_STARTED,
            request=request,
            result=None,
            summary=(
                "Sandbox command started with relaxed local process execution "
                "for danger-full-access mode."
            ),
            payload={
                "sandbox_id": request.sandbox_id,
                "backend": _RELAXED_BACKEND_NAME,
                "sandbox_mode": self.permission_profile.profile.value,
                "sandbox_enforcement": "relaxed",
                "local_process_fallback_reason": _DANGER_FULL_ACCESS_FALLBACK_REASON,
            },
            severity=TraceSeverity.WARNING,
        )
        command, shell = _subprocess_command(request.command)
        env = _relaxed_env(request)
        timeout = request.profile.resources.timeout_seconds
        output_limit = request.profile.resources.max_output_chars
        result_started = _now()
        try:
            process = subprocess.Popen(
                command,
                cwd=str(request.cwd),
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                shell=shell,
                text=True,
                encoding="utf-8",
                errors="replace",
                creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
                start_new_session=os.name != "nt",
            )
        except FileNotFoundError as exc:
            return self._relaxed_process_result(
                request,
                started,
                started_at=result_started,
                status=SandboxStatus.FAILED,
                exit_code=None,
                stdout="",
                stderr=str(exc),
                unavailable_reason=unavailable_reason,
                error_code="command_not_found",
            )
        except PermissionError as exc:
            return self._relaxed_process_result(
                request,
                started,
                started_at=result_started,
                status=SandboxStatus.FAILED,
                exit_code=None,
                stdout="",
                stderr=str(exc),
                unavailable_reason=unavailable_reason,
                error_code="permission_error",
            )
        except Exception as exc:
            return self._relaxed_process_result(
                request,
                started,
                started_at=result_started,
                status=SandboxStatus.SETUP_FAILED,
                exit_code=None,
                stdout="",
                stderr=str(exc),
                unavailable_reason=unavailable_reason,
                error_code="spawn_failed",
            )
        timed_out = False
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            _kill_process_tree(process)
            try:
                stdout, stderr = process.communicate(
                    timeout=_PROCESS_TERMINATE_TIMEOUT_SECONDS
                )
            except subprocess.TimeoutExpired:
                with suppress(Exception):
                    process.kill()
                stdout, stderr = "", "Process timed out and could not be terminated cleanly."
        stdout, stderr, output_truncated = _limit_output(
            shared_trace_redactor().redact_text(stdout or ""),
            shared_trace_redactor().redact_text(stderr or ""),
            output_limit,
        )
        status = (
            SandboxStatus.TIMEOUT
            if timed_out
            else SandboxStatus.SUCCESS
            if process.returncode == 0
            else SandboxStatus.FAILED
        )
        return self._relaxed_process_result(
            request,
            started,
            started_at=result_started,
            status=status,
            exit_code=process.returncode,
            stdout=stdout,
            stderr=stderr,
            unavailable_reason=unavailable_reason,
            error_code="timeout" if timed_out else None,
            timed_out=timed_out,
            output_truncated=output_truncated,
        )

    def _relaxed_process_result(
        self,
        request: SandboxRequest,
        started: float,
        *,
        started_at: str,
        status: SandboxStatus,
        exit_code: int | None,
        stdout: str,
        stderr: str,
        unavailable_reason: str,
        error_code: str | None,
        timed_out: bool = False,
        output_truncated: bool = False,
    ) -> SandboxResult:
        metadata = {
            "error_code": error_code,
            "reason": unavailable_reason,
            "sandbox_mode": self.permission_profile.profile.value,
            "sandbox_backend": _RELAXED_BACKEND_NAME,
            "sandbox_enforcement": "relaxed",
            "enforcement_status": "relaxed",
            "execution_backend": _RELAXED_BACKEND_NAME,
            "backend_is_local_process": True,
            "fallback_used": True,
            "fallback_reason": _DANGER_FULL_ACCESS_FALLBACK_REASON,
            "elevated_available": False,
            "elevated_blocker_summary": _safe_reason(unavailable_reason),
            "used_local_process_fallback": True,
            "local_process_fallback_reason": _DANGER_FULL_ACCESS_FALLBACK_REASON,
            "network_denied_verified": False,
            "process_tree_kill": True,
            "timeout_enforced": True,
            "timed_out": timed_out,
            "output_truncated": output_truncated,
        }
        metadata["timing"] = {
            "relaxed_local_process_time_seconds": time.perf_counter() - started
        }
        return SandboxResult(
            sandbox_id=request.sandbox_id,
            backend_name=_RELAXED_BACKEND_NAME,
            status=status,
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            started_at=started_at,
            ended_at=_now(),
            duration_ms=int((time.perf_counter() - started) * 1000),
            cleanup_status="not_required",
            metadata=metadata,
        )

    @staticmethod
    def _capabilities_for_trace(
        backend: SandboxBackend | None,
        capabilities: Any | None,
    ) -> Any | None:
        if capabilities is not None:
            return capabilities
        if backend is None:
            return None
        return backend.capabilities()

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
            metadata={
                "error_code": "backend_unavailable",
                "reason": reason,
                "sandbox_mode": self.permission_profile.profile.value,
                "sandbox_backend": "unavailable",
                "sandbox_enforcement": "strict",
                "enforcement_status": "blocked",
                "execution_backend": "unavailable",
                "backend_is_local_process": False,
                "network_isolation": "blocked",
                "filesystem_isolation": "blocked",
                "fallback_used": False,
                "fallback_reason": "",
                "elevated_available": self._backend_available(
                    self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME)
                )
                if self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME)
                else False,
                "elevated_blocker_summary": _safe_reason(
                    self._backend_reason(
                        self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME)
                    )
                    or reason
                ),
            },
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
            metadata={
                "error_code": "protected_path_denied",
                "sandbox_mode": self.permission_profile.profile.value,
                "sandbox_backend": "policy",
                "sandbox_enforcement": "strict",
                "enforcement_status": "blocked",
                "execution_backend": "policy",
                "backend_is_local_process": False,
                "network_isolation": "blocked",
                "filesystem_isolation": "blocked",
                "fallback_used": False,
                "fallback_reason": "",
                "elevated_available": self._backend_available(
                    self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME)
                )
                if self._backend_by_name(_WINDOWS_ELEVATED_BACKEND_NAME)
                else False,
                "elevated_blocker_summary": "",
            },
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
        for token in command:
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

_now = utc_iso_timestamp


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


def _subprocess_command(command: list[str] | str) -> tuple[list[str] | str, bool]:
    if isinstance(command, list):
        return [str(part) for part in command], False
    return command, True


def _relaxed_env(request: SandboxRequest) -> dict[str, str]:
    env = dict(request.profile.env.extra_env)
    for name in _runtime_env_names():
        value = os.environ.get(name)
        if value is not None and name not in env:
            env[name] = value
    env.setdefault("PYTHONIOENCODING", "utf-8")
    return env


def _runtime_env_names() -> tuple[str, ...]:
    if os.name == "nt":
        return (
            "COMSPEC",
            "PATH",
            "PATHEXT",
            "SYSTEMDRIVE",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "WINDIR",
        )
    return ("HOME", "LANG", "LC_ALL", "PATH", "TMPDIR")


def _kill_process_tree(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        with suppress(Exception):
            subprocess.run(
                ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=_PROCESS_TERMINATE_TIMEOUT_SECONDS,
            )
        if process.poll() is None:
            with suppress(Exception):
                process.kill()
        return
    killpg = getattr(os, "killpg", None)
    getpgid = getattr(os, "getpgid", None)
    if callable(killpg) and callable(getpgid):
        with suppress(Exception):
            killpg(getpgid(process.pid), signal.SIGTERM)
        with suppress(subprocess.TimeoutExpired):
            process.wait(timeout=1)
    if process.poll() is None:
        with suppress(Exception):
            if callable(killpg) and callable(getpgid):
                killpg(getpgid(process.pid), getattr(signal, "SIGKILL", signal.SIGTERM))
            else:
                process.kill()


def _limit_output(
    stdout: str,
    stderr: str,
    max_output_chars: int | None,
) -> tuple[str, str, bool]:
    if max_output_chars is None or max_output_chars <= 0:
        return stdout, stderr, False
    remaining = max_output_chars
    limited_stdout = stdout[:remaining]
    remaining -= len(limited_stdout)
    limited_stderr = stderr[: max(0, remaining)]
    truncated = len(limited_stdout) < len(stdout) or len(limited_stderr) < len(stderr)
    return limited_stdout, limited_stderr, truncated


def _safe_reason(reason: str) -> str:
    return shared_trace_redactor().redact_text(reason)[:500]


def _diagnostic_summary(diagnostics: Any) -> str:
    if not isinstance(diagnostics, list | tuple):
        return ""
    summaries: list[str] = []
    for item in diagnostics:
        if not isinstance(item, dict):
            continue
        failure_type = item.get("failure_type")
        kind = item.get("kind")
        reason = item.get("reason")
        if failure_type:
            summaries.append(str(failure_type))
        elif kind:
            summaries.append(str(kind))
        elif reason:
            summaries.append(str(reason))
    return _safe_reason("; ".join(summaries))
