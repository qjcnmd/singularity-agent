from __future__ import annotations

import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Protocol

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.protocols import TraceEmitterProtocol
from singularity.policy.config import SecurityMode
from singularity.policy.models import PolicyDecision
from singularity.sandbox.backends import SandboxBackend, default_sandbox_backends
from singularity.sandbox.exceptions import SandboxCapabilityError
from singularity.sandbox.models import (
    SandboxFilesystemMode,
    SandboxNetworkMode,
    SandboxProfileName,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
    SandboxViolation,
    default_sandbox_profile,
    new_sandbox_id,
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
        security_mode: SecurityMode | str = SecurityMode.COMPAT,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.backends = backends if backends is not None else default_sandbox_backends()
        self.trace = trace or SandboxJsonlTraceRecorder.create(self.workspace_root)
        self.security_mode = _security_mode(security_mode)

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
        try:
            self._apply_policy_constraints(request)
            backend = self._select_backend(request)
            if backend is None:
                result = self._unavailable(request, "No sandbox backend is registered.", started)
                self._record_trace(prepared=prepared, result=result, capabilities=None, request=request)
                return result
            capability_violations = self.ensure_capabilities(request, backend)
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
                    "sandbox_handle": _relative_handle(
                        prepared.sandbox_root,
                        self.workspace_root,
                    ),
                },
            )
            self._emit_trace(
                TraceEventType.SANDBOX_STARTED,
                request=request,
                result=None,
                summary=f"Sandbox command started in {prepared.sandbox_id}.",
            )
            result = backend.run(prepared)
            if capability_violations:
                result.violations.extend(capability_violations)
            self._throw_if_cancelled()
            try:
                backend.cleanup(prepared)
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
                    try:
                        backend.cleanup(prepared)
                    except Exception:
                        pass
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

    def ensure_capabilities(
        self,
        request: SandboxRequest,
        backend: SandboxBackend,
    ) -> list[SandboxViolation]:
        capabilities = backend.capabilities()
        violations: list[SandboxViolation] = []
        if request.profile.network.require_hard_isolation and not capabilities.network_isolation:
            raise SandboxCapabilityError("Backend cannot enforce required network isolation.")
        if (
            request.profile.network.mode == SandboxNetworkMode.DENIED
            and not capabilities.network_isolation
        ):
            # Fail-closed: the profile denies network access but the selected
            # backend cannot enforce network isolation. Refuse to run rather
            # than silently executing with an unenforced denial.
            raise SandboxCapabilityError(
                f"Backend {backend.name()} cannot enforce denied network mode; "
                "network access is not isolated."
            )
        if (
            request.profile.filesystem.mode == SandboxFilesystemMode.READ_ONLY_WORKSPACE
            and not capabilities.readonly_mount
        ):
            raise SandboxCapabilityError(
                "Backend cannot enforce read-only workspace; "
                "write operations would not be blocked."
            )
        if request.profile.resources.max_memory_mb is not None and not capabilities.memory_limit:
            raise SandboxCapabilityError("Backend cannot enforce memory limits.")
        if request.profile.resources.max_processes is not None and not capabilities.process_limit:
            raise SandboxCapabilityError("Backend cannot enforce process limits.")
        request_checker = getattr(backend, "ensure_request_supported", None)
        if callable(request_checker):
            request_checker(request)
        return violations

    def capability_summary(self, *, approval_mode: str | None = None) -> dict[str, Any]:
        backends: dict[str, dict[str, Any]] = {}
        selected_capabilities: dict[str, Any] | None = None
        for backend in self.backends:
            if not self._backend_available(backend):
                continue
            name = backend.name()
            capabilities_dict = backend.capabilities().to_dict()
            backends[name] = capabilities_dict
            if selected_capabilities is None:
                # The first available backend is what `_select_backend` would
                # pick for a default request; report isolation capabilities
                # based on this selected backend rather than the union of all
                # available backends.
                selected_capabilities = capabilities_dict
        if selected_capabilities is None:
            hard_isolation = False
            soft_workspace_isolation = False
        else:
            hard_isolation = selected_capabilities.get("network_isolation") is True
            soft_workspace_isolation = (
                selected_capabilities.get("copy_on_write") is True
                or selected_capabilities.get("filesystem_isolation") is True
            )
        default_profile = default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION,
            workspace_root=self.workspace_root,
        )
        return {
            "hard_isolation": hard_isolation,
            "soft_workspace_isolation": soft_workspace_isolation,
            "no_isolation": not hard_isolation and not soft_workspace_isolation,
            "network_blocked": hard_isolation,
            "write_scope": default_profile.filesystem.mode.value,
            "approval_mode": approval_mode,
            "security_mode": self.security_mode.value,
            "available_backends": sorted(backends),
            "capabilities": backends,
        }

    @staticmethod
    def _apply_policy_constraints(request: SandboxRequest) -> None:
        constraints = request.policy_constraints
        if constraints is None:
            return
        if getattr(constraints, "hard_isolation_required", False):
            request.profile.network.require_hard_isolation = True

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
        if not hasattr(backend, "is_available"):
            return True
        try:
            return bool(backend.is_available())
        except Exception:
            return False

    def shutdown(self) -> None:
        return None

    def build_request_from_policy(
        self,
        command_request: Any,
        policy_decision: PolicyDecision,
        *,
        session_id: str,
        task_id: str,
        action_id: str,
        profile_name: SandboxProfileName | str | None = None,
        cwd: Path,
    ) -> SandboxRequest:
        profile = default_sandbox_profile(
            profile_name or self._profile_name(command_request),
            workspace_root=self.workspace_root,
        )
        constraints = policy_decision.constraints
        if constraints.max_duration_seconds is not None:
            profile.resources.timeout_seconds = constraints.max_duration_seconds
        if constraints.max_output_chars is not None:
            profile.resources.max_output_chars = constraints.max_output_chars
        filesystem_mode = str(constraints.filesystem_mode or "")
        if filesystem_mode in SandboxFilesystemMode._value2member_map_:
            profile.filesystem.mode = SandboxFilesystemMode(filesystem_mode)
        elif filesystem_mode in {"copy_on_write", "workspace_copy", "copy_on_write_workspace"}:
            profile.filesystem.mode = SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE
        elif filesystem_mode in {"read_only", "readonly", "read_only_workspace"}:
            profile.filesystem.mode = SandboxFilesystemMode.READ_ONLY_WORKSPACE
        elif filesystem_mode in {"empty", "empty_temp_workspace"}:
            profile.filesystem.mode = SandboxFilesystemMode.EMPTY_TEMP_WORKSPACE
        if profile.filesystem.mode == SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE:
            profile.filesystem.detect_changes = True
        if constraints.network_allowed:
            profile.network.mode = SandboxNetworkMode.ALLOWED
        elif getattr(constraints, "hard_isolation_required", False):
            profile.network.require_hard_isolation = True
        elif (
            self.security_mode == SecurityMode.STRICT
            and constraints.sandbox_required
        ):
            profile.network.require_hard_isolation = True
        if constraints.allowed_hosts:
            profile.network.allowed_hosts = constraints.allowed_hosts
        if "hard-network-required" in constraints.allowed_hosts:
            profile.network.require_hard_isolation = True
        metadata = {
            "command_id": getattr(command_request, "command_id", action_id),
            "purpose": getattr(getattr(command_request, "purpose", None), "value", None),
        }
        return SandboxRequest(
            sandbox_id=new_sandbox_id(),
            session_id=session_id,
            task_id=task_id,
            action_id=action_id,
            command=command_request.argv if command_request.argv is not None else command_request.shell or "",
            cwd=cwd,
            workspace_root=self.workspace_root,
            profile=profile,
            policy_decision_id=policy_decision.decision_id,
            policy_constraints=constraints,
            reason=policy_decision.reason,
            metadata=metadata,
        )

    def _unavailable(
        self,
        request: SandboxRequest,
        reason: str,
        started: float,
        *,
        backend_name: str = "unavailable",
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
            metadata={"error_code": "sandbox_unavailable", "reason": reason},
        )

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
            severity=(
                TraceSeverity.INFO
                if result.status == SandboxStatus.SUCCESS
                else TraceSeverity.WARNING
            ),
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
                "changed_files": (
                    result.filesystem_changes.to_dict() if result else {}
                ),
                "violations": [item.to_dict() for item in result.violations]
                if result
                else [],
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
            artifact_refs=[
                artifact.artifact_id for artifact in result.artifacts
            ]
            if result
            else [],
        )

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()

    @staticmethod
    def _profile_name(command_request: Any) -> SandboxProfileName:
        purpose = getattr(getattr(command_request, "purpose", None), "value", "")
        if purpose in {"PROJECT_VERIFICATION", "LINT", "TYPECHECK", "FORMAT_CHECK", "BUILD"}:
            return SandboxProfileName.ISOLATED_VERIFICATION
        if purpose == "CODE_GENERATION":
            return SandboxProfileName.GENERATED_CODE
        if purpose == "PACKAGE_MANAGER":
            return SandboxProfileName.PACKAGE_OPERATION
        if purpose == "LONG_RUNNING":
            return SandboxProfileName.LONG_RUNNING_SERVICE
        return SandboxProfileName.READONLY_ANALYSIS


def _now() -> str:
    return datetime.now(UTC).isoformat()


def _is_cancellation_error(exc: BaseException) -> bool:
    return type(exc).__name__ == "CancellationError" and getattr(exc, "code", None) == "cancelled"


def _security_mode(value: SecurityMode | str) -> SecurityMode:
    if isinstance(value, SecurityMode):
        return value
    try:
        return SecurityMode[str(value).upper()]
    except KeyError:
        return SecurityMode(str(value))


def _relative_handle(path: Path, root: Path) -> str:
    try:
        return path.resolve(strict=False).relative_to(root.resolve(strict=False)).as_posix() or "."
    except ValueError:
        return path.name
