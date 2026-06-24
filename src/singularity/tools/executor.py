from __future__ import annotations

import hashlib
import json
import multiprocessing
import pickle
import time
from collections import OrderedDict
from dataclasses import dataclass
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeout
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath
from typing import Any, Protocol

from pydantic import ValidationError

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.redaction import TraceRedactor
from singularity.policy import (
    ApprovalGate,
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    PolicyComponent,
)
from singularity.policy.audit import redact_resource_identifier
from singularity.policy.config import PolicyConfig
from singularity.policy.exceptions import (
    ApprovalDenied,
    ApprovalRequired,
    PolicyAskUserRequired,
    PolicyDenied,
    PolicyEscalationRequired,
    SandboxRequired,
)
from singularity.tools.models import (
    PermissionLevel,
    ToolError,
    ToolExecutionBackendKind,
    ToolExecutionFailure,
    ToolExecutionRequest,
    ToolResult,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)
from singularity.tools.policy import ToolPolicy
from singularity.tools.registry import ToolRegistry
from singularity.observability.protocols import TraceEmitterProtocol


@dataclass
class _CacheEntry:
    result: ToolResult
    created_at: float
    touched_paths: tuple[str, ...]


@dataclass
class _ReplayEntry:
    args_fingerprint: str
    result: ToolResult
    replay_allowed: bool


def _tool_process_entrypoint(handler: Any, validated_args: Any, conn: Any) -> None:
    try:
        output = handler(validated_args)
        _send_tool_process_payload(conn, {"status": "success", "output": output})
    except ToolExecutionFailure as exc:
        _send_tool_process_payload(
            conn,
            {
                "status": "tool_failure",
                "code": exc.code,
                "message": exc.message,
                "details": _json_safe_process_value(exc.details),
            },
        )
    except Exception as exc:
        _send_tool_process_payload(
            conn,
            {
                "status": "exception",
                "message": str(exc),
                "type": type(exc).__name__,
            },
        )
    finally:
        conn.close()


def _send_tool_process_payload(conn: Any, payload: dict[str, Any]) -> None:
    try:
        conn.send(payload)
    except Exception as exc:
        try:
            conn.send(
                {
                    "status": "exception",
                    "message": f"Tool process could not return result: {exc}",
                    "type": type(exc).__name__,
                }
            )
        except Exception:
            pass


def _json_safe_process_value(value: Any) -> Any:
    return json.loads(json.dumps(value, ensure_ascii=False, default=str))


class ToolResultCache:
    def __init__(self) -> None:
        self._entries: OrderedDict[str, _CacheEntry] = OrderedDict()

    def get(self, key: str, *, ttl_seconds: float | None) -> ToolResult | None:
        entry = self._entries.get(key)
        if entry is None:
            return None
        if ttl_seconds is not None and time.time() - entry.created_at > ttl_seconds:
            self._entries.pop(key, None)
            return None
        self._entries.move_to_end(key)
        return entry.result.model_copy(deep=True)

    def set(
        self,
        key: str,
        result: ToolResult,
        *,
        max_entries: int,
        touched_paths: tuple[str, ...],
    ) -> None:
        self._entries[key] = _CacheEntry(
            result=result.model_copy(deep=True),
            created_at=time.time(),
            touched_paths=touched_paths,
        )
        self._entries.move_to_end(key)
        while len(self._entries) > max_entries:
            self._entries.popitem(last=False)

    def invalidate_paths(self, paths: list[str]) -> None:
        normalized = {Path(path).as_posix() for path in paths}
        for key, entry in list(self._entries.items()):
            if any(
                _paths_overlap(changed, touched)
                for changed in normalized
                for touched in entry.touched_paths
            ):
                self._entries.pop(key, None)

    def clear(self) -> None:
        self._entries.clear()


class IdempotencyLedger:
    def __init__(self) -> None:
        self._entries: dict[str, _ReplayEntry] = {}

    def check(
        self,
        tool_call_id: str | None,
        args_fingerprint: str,
        *,
        replay_allowed: bool,
    ) -> ToolResult | None:
        if not tool_call_id:
            return None
        existing = self._entries.get(tool_call_id)
        if existing is None:
            return None
        if existing.args_fingerprint != args_fingerprint:
            return ToolResult.failure(
                code="conflicting_replay",
                message="Duplicate tool_call_id was reused with different arguments.",
            )
        if not existing.replay_allowed:
            return ToolResult.failure(
                code="replay_not_allowed",
                message="Duplicate tool_call_id replay is not allowed for this tool.",
            )
        replay = existing.result.model_copy(deep=True)
        replay.metadata["replay"] = True
        return replay

    def remember(
        self,
        tool_call_id: str | None,
        args_fingerprint: str,
        result: ToolResult,
        *,
        replay_allowed: bool,
    ) -> None:
        if not tool_call_id:
            return
        self._entries[tool_call_id] = _ReplayEntry(
            args_fingerprint=args_fingerprint,
            result=result.model_copy(deep=True),
            replay_allowed=replay_allowed,
        )


class ToolPolicyEngineProtocol(Protocol):
    config: PolicyConfig

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        ...


class ToolExecutor:
    def __init__(
        self,
        *,
        registry: ToolRegistry,
        policy: ToolPolicy,
        trace: TraceEmitterProtocol | None,
        workspace_root: Path,
        planner: Any | None = None,
        policy_engine: ToolPolicyEngineProtocol | None = None,
        approval_gate: ApprovalGate | Any | None = None,
        standalone_can_execute: bool = True,
        dry_run: bool = False,
    ) -> None:
        self.registry = registry
        self.policy = policy
        self.trace = trace
        self.workspace_root = workspace_root.resolve()
        self.planner = planner
        if policy_engine is None:
            raise ValueError(
                "policy_engine is required; ToolExecutor must use the session PolicyEngine."
            )
        self.policy_engine = policy_engine
        self.approval_gate = approval_gate
        self.standalone_can_execute = standalone_can_execute
        self.dry_run = dry_run
        self._cache = ToolResultCache()
        self._ledger = IdempotencyLedger()
        self._redactor = TraceRedactor()
        self.cancellation_token: Any | None = None

    def execute_tool_call(self, tool_call: dict[str, Any]) -> ToolResult:
        return self.execute_request(ToolExecutionRequest.from_provider_tool_call(tool_call))

    def execute_request(self, request: ToolExecutionRequest | dict[str, Any]) -> ToolResult:
        request = self._normalize_execution_request(request)
        self._throw_if_cancelled()
        started_at = datetime.now(UTC).isoformat()
        started = time.perf_counter()
        tool_call_id = request.tool_call_id
        tool_name = request.tool_name or "<unknown>"
        spec: ToolSpec | None = None
        validated_args: dict[str, Any] | None = None
        planner_action_id: str | None = None
        cache_hit = False
        output_digest: str | None = None
        result: ToolResult
        planner_updated = False

        try:
            self._throw_if_cancelled()
            spec = self.registry.get(tool_name)
            if spec is None or not spec.enabled:
                result = ToolResult.failure(
                    code="tool_not_found",
                    message=f"Unknown tool: {tool_name}",
                )
                output_digest = self._result_digest(result)
                return result

            try:
                arguments = self._arguments_for_execution_validation(request)
            except json.JSONDecodeError as exc:
                self._emit_trace(
                    TraceEventType.TOOL_VALIDATION_FAILED,
                    summary=f"Tool {tool_name} arguments were invalid JSON.",
                    payload={
                        "tool_name": tool_name,
                        "tool_call_id": tool_call_id,
                        "batch_id": request.batch_id,
                        "argument_digest": request.argument_digest,
                        "validation_scope": "execution_validation",
                    },
                    ids=self._request_trace_ids(request, action_id=tool_call_id),
                    severity=TraceSeverity.ERROR,
                )
                result = ToolResult.failure(
                    code="bad_arguments_json",
                    message=f"Invalid JSON arguments: {exc}",
                )
                output_digest = self._result_digest(result)
                return result

            self._emit_trace(
                TraceEventType.TOOL_VALIDATION_STARTED,
                summary=f"Validating tool {tool_name}.",
                payload={
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "batch_id": request.batch_id,
                    "argument_digest": request.argument_digest,
                    "validation_scope": "execution_validation",
                    "arguments": self._argument_trace_summary(arguments),
                },
                ids=self._request_trace_ids(request, action_id=tool_call_id),
            )
            try:
                validated = spec.input_model.model_validate(arguments)
            except ValidationError as exc:
                result = ToolResult.failure(
                    code="validation_error",
                    message="Tool arguments failed validation.",
                    details=self._redactor.redact_value(exc.errors()),
                )
                output_digest = self._result_digest(result)
                self._emit_trace(
                    TraceEventType.TOOL_VALIDATION_FAILED,
                    summary=f"Tool {tool_name} arguments failed validation.",
                    payload={
                        "tool_name": tool_name,
                        "tool_call_id": tool_call_id,
                        "batch_id": request.batch_id,
                        "argument_digest": request.argument_digest,
                        "validation_scope": "execution_validation",
                        "errors": result.error.details if result.error else None,
                    },
                    ids=self._request_trace_ids(request, action_id=tool_call_id),
                    severity=TraceSeverity.ERROR,
                )
                return result
            validated_args = validated.model_dump(mode="json")
            args_fingerprint = self._digest(
                json.dumps(validated_args, ensure_ascii=False, sort_keys=True, default=str)
            )

            replay = self._ledger.check(
                tool_call_id,
                args_fingerprint,
                replay_allowed=spec.idempotency_policy.replay_returns_previous
                if spec.idempotency_policy
                else spec.idempotent,
            )
            if replay is not None:
                result = replay
                output_digest = result.metadata.get("output_digest") or self._result_digest(result)
                return result

            boundary_error = self._check_execution_boundary(spec)
            if boundary_error is not None:
                result = boundary_error
                output_digest = self._result_digest(result)
                self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                return result

            dry_run_error = self._dry_run_error(spec)
            if dry_run_error is not None:
                result = dry_run_error
                output_digest = self._result_digest(result)
                self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                return result

            delegated_preflight_error = self._preflight_delegated_handler(spec, validated)
            if delegated_preflight_error is not None:
                result = delegated_preflight_error
                output_digest = self._result_digest(result)
                self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                return result

            policy_result, approval_grant_id, policy_decision_id = self._enforce_policy(
                tool_name=tool_name,
                spec=spec,
                validated_args=validated_args,
                tool_call_id=tool_call_id,
            )
            if (
                policy_result is not None
                and self._delegates_policy_decision(spec, policy_result)
            ):
                policy_result = None
            if policy_result is not None:
                result = policy_result
                output_digest = self._result_digest(result)
                self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                return result

            planner_decision = self._authorize_with_planner(
                tool_name=tool_name,
                tool_call_id=tool_call_id,
                spec=spec,
                validated_args=validated_args,
            )
            if planner_decision is not None and not planner_decision.allowed:
                self._record_planner_denial(tool_name, planner_decision)
                result = ToolResult.failure(
                    code=planner_decision.error_code or "action_not_allowed",
                    message="Planner denied tool execution.",
                    details={
                        "planner_reason": planner_decision.reason,
                        "risk_decision": planner_decision.risk_decision.value,
                    },
                )
                output_digest = self._result_digest(result)
                self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                return result
            if planner_decision is not None and planner_decision.action is not None:
                planner_action_id = planner_decision.action.action_id

            self._throw_if_cancelled()
            cache_key = self._cache_key(spec, validated_args)
            cache_policy = spec.cache_policy
            if self._should_cache(spec):
                cached = self._cache.get(
                    cache_key,
                    ttl_seconds=cache_policy.ttl_seconds if cache_policy else None,
                )
                if cached is not None:
                    cache_hit = True
                    cached.metadata["cache_hit"] = True
                    if approval_grant_id:
                        cached.metadata["approval_grant_id"] = approval_grant_id
                    if policy_decision_id:
                        cached.metadata["policy_decision_id"] = policy_decision_id
                    result = cached
                    output_digest = result.metadata.get("output_digest") or self._result_digest(result)
                    self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                    return result

            delegated_error = self._delegated_backend_error(spec)
            if delegated_error is not None:
                result = delegated_error
                output_digest = self._result_digest(result)
                self._remember_replay(tool_call_id, args_fingerprint, spec, result)
                return result

            self._emit_trace(
                TraceEventType.TOOL_DISPATCH_STARTED,
                summary=f"Dispatching tool {tool_name}.",
                payload={
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "permission_level": spec.permission_level.value,
                    "risk_tags": list(spec.risk_tags),
                    "backend": spec.execution_backend.value,
                    "batch_id": request.batch_id,
                    "argument_digest": request.argument_digest,
                    "arguments": self._argument_trace_summary(validated_args),
                },
                ids=self._request_trace_ids(request, action_id=planner_action_id or tool_call_id),
            )
            self._throw_if_cancelled()
            result, output_digest = self._execute_handler(spec, validated)
            self._throw_if_cancelled()
            if approval_grant_id:
                result.metadata["approval_grant_id"] = approval_grant_id
            if policy_decision_id:
                result.metadata["policy_decision_id"] = policy_decision_id
            self._update_planner(
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                result=result,
                action_id=planner_action_id,
            )
            planner_updated = True
            if self._should_cache(spec) and result.ok and not self._is_sensitive_result(spec, result):
                touched_paths = self._touched_paths(spec, validated_args)
                self._cache.set(
                    cache_key,
                    result,
                    max_entries=spec.cache_policy.max_entries if spec.cache_policy else 128,
                    touched_paths=touched_paths,
                )
            if spec.permission_level != PermissionLevel.READ_ONLY:
                self._cache.clear()
            self._remember_replay(tool_call_id, args_fingerprint, spec, result)
            return result
        except Exception as exc:
            if _is_cancellation_error(exc):
                raise
            result = ToolResult.failure(
                code="internal_error",
                message=self._redactor.redact_text(str(exc)),
                details={"type": type(exc).__name__},
            )
            output_digest = self._result_digest(result)
            return result
        finally:
            ended_at = datetime.now(UTC).isoformat()
            duration_seconds = time.perf_counter() - started
            if "result" in locals():
                result.metadata.setdefault("cache_hit", cache_hit)
                result.metadata.setdefault("duration_seconds", duration_seconds)
                result.metadata.setdefault("output_digest", output_digest)
                self._annotate_result_metadata(result, request)
                if spec is not None:
                    result.metadata.setdefault("backend", spec.execution_backend.value)
                if not planner_updated:
                    self._safe_update_planner(
                        tool_call_id=tool_call_id,
                        tool_name=tool_name,
                        result=result,
                        action_id=planner_action_id,
                    )
                self._record_trace(
                    request=request,
                    tool_call_id=tool_call_id,
                    tool_name=tool_name,
                    spec=spec,
                    validated_args=validated_args,
                    started_at=started_at,
                    ended_at=ended_at,
                    duration_seconds=duration_seconds,
                    result=result,
                    output_digest=output_digest,
                    cache_hit=cache_hit,
                )

    def invalidate_paths(self, paths: list[str]) -> None:
        self._cache.invalidate_paths(paths)

    @staticmethod
    def _normalize_execution_request(
        request: ToolExecutionRequest | dict[str, Any],
    ) -> ToolExecutionRequest:
        if isinstance(request, ToolExecutionRequest):
            return request
        return ToolExecutionRequest.from_provider_tool_call(request)

    def _arguments_for_execution_validation(self, request: ToolExecutionRequest) -> Any:
        parsed = self._parse_arguments(request.raw_arguments)
        if request.normalized_arguments and parsed != request.normalized_arguments:
            return request.normalized_arguments
        return parsed

    @staticmethod
    def _request_trace_ids(
        request: ToolExecutionRequest,
        *,
        action_id: str | None,
    ) -> dict[str, Any]:
        return {
            "run_id": request.run_id,
            "session_id": request.session_id,
            "task_id": request.task_id,
            "phase_id": request.phase_id,
            "action_id": action_id,
        }

    @staticmethod
    def _annotate_result_metadata(
        result: ToolResult,
        request: ToolExecutionRequest,
    ) -> None:
        for key in (
            "batch_id",
            "run_id",
            "session_id",
            "task_id",
            "phase_id",
            "model_request_id",
            "model_response_id",
            "argument_digest",
        ):
            value = getattr(request, key)
            if value:
                result.metadata.setdefault(key, value)

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()

    def _authorize_with_planner(
        self,
        *,
        tool_name: str,
        tool_call_id: str | None,
        spec: ToolSpec,
        validated_args: dict[str, Any],
    ) -> Any | None:
        if self.planner is None:
            return None
        return self.planner.authorize_tool_call(
            tool_name=tool_name,
            tool_call_id=tool_call_id,
            spec=spec,
            arguments=validated_args,
        )

    def _update_planner(
        self,
        *,
        tool_call_id: str | None,
        tool_name: str,
        result: ToolResult,
        action_id: str | None,
    ) -> None:
        if self.planner is None:
            return
        self.planner.update_from_tool_result(
            tool_call_id=tool_call_id,
            tool_name=tool_name,
            result=result,
            action_id=action_id,
        )

    def _safe_update_planner(
        self,
        *,
        tool_call_id: str | None,
        tool_name: str,
        result: ToolResult,
        action_id: str | None,
    ) -> None:
        try:
            self._update_planner(
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                result=result,
                action_id=action_id,
            )
        except Exception as exc:
            self._emit_trace(
                TraceEventType.TOOL_DISPATCH_FAILED,
                summary=f"Planner observation update failed for tool {tool_name}.",
                payload={"tool_name": tool_name, "error_type": type(exc).__name__},
                ids={"action_id": action_id or tool_call_id},
                severity=TraceSeverity.WARNING,
            )

    def _record_planner_denial(self, tool_name: str, planner_decision: Any) -> None:
        self._emit_trace(
            TraceEventType.TOOL_DISPATCH_FAILED,
            summary=f"Tool {tool_name} was denied by planner.",
            payload={
                "tool_name": tool_name,
                "planner_reason": planner_decision.reason,
                "risk_decision": planner_decision.risk_decision.value,
            },
            ids={},
            severity=TraceSeverity.WARNING,
        )
        if self.planner is not None and hasattr(self.planner, "record_policy_observation"):
            self.planner.record_policy_observation(
                {
                    "outcome": "deny",
                    "component": "tool",
                    "operation": "planner_authorization",
                    "reason": planner_decision.reason,
                    "risk_level": "low",
                    "resource": tool_name,
                    "decision_id": planner_decision.error_code or "planner_denied",
                }
            )

    def _enforce_policy(
        self,
        *,
        tool_name: str,
        spec: ToolSpec,
        validated_args: dict[str, Any],
        tool_call_id: str | None,
    ) -> tuple[ToolResult | None, str | None, str | None]:
        request = self._policy_request(
            tool_name=tool_name,
            spec=spec,
            validated_args=validated_args,
            tool_call_id=tool_call_id,
        )
        decision = self.policy_engine.enforce(request)
        if decision.outcome == DecisionOutcome.ALLOW:
            return None, decision.approval_grant_id, decision.decision_id
        if decision.outcome == DecisionOutcome.REQUIRE_REVIEW and self.approval_gate is not None:
            if hasattr(self.approval_gate, "consume_matching_grant"):
                existing_grant = self.approval_gate.consume_matching_grant(request)
                if existing_grant is not None:
                    allowed = _decision_allowed_by_grant(decision, existing_grant.grant_id)
                    return None, existing_grant.grant_id, allowed.decision_id
            try:
                grant = self.approval_gate.resolve(request, decision)
            except (
                ApprovalDenied,
                ApprovalRequired,
                PolicyAskUserRequired,
                PolicyDenied,
                PolicyEscalationRequired,
                SandboxRequired,
            ):
                self._record_policy_observation(request, decision)
                return self._policy_failure(request, decision), None, decision.decision_id
            if grant is None:
                self._record_policy_observation(request, decision)
                return self._policy_failure(request, decision), None, decision.decision_id
            if hasattr(self.approval_gate, "register_grant"):
                self.approval_gate.register_grant(grant)
            consumed_grant = (
                self.approval_gate.consume_grant(grant)
                if hasattr(self.approval_gate, "consume_grant")
                else grant
            )
            if consumed_grant is None:
                self._record_policy_observation(request, decision)
                return self._policy_failure(request, decision), None, decision.decision_id
            allowed = _decision_allowed_by_grant(decision, consumed_grant.grant_id)
            return None, consumed_grant.grant_id, allowed.decision_id
        self._record_policy_observation(request, decision)
        return self._policy_failure(request, decision), None, decision.decision_id

    def _policy_failure(self, request: PolicyRequest, decision: PolicyDecision) -> ToolResult:
        return ToolResult.failure(
            code=_policy_error_code(decision.outcome),
            message=self._redactor.redact_text(decision.reason),
            details={
                "policy": self._redactor.redact_value(decision.to_dict()),
                "request": self._safe_request_details(request),
            },
            metadata={"policy_decision_id": decision.decision_id},
        )

    def _policy_request(
        self,
        *,
        tool_name: str,
        spec: ToolSpec,
        validated_args: dict[str, Any],
        tool_call_id: str | None,
    ) -> PolicyRequest:
        resources = self._resources_for(spec, validated_args)
        resource = resources[0] if resources else ResourceRef("workspace", tool_name, workspace_relative=True)
        resource_details = [item.to_dict() for item in resources] or [resource.to_dict()]
        related_resources = resource_details[1:]
        if related_resources and not resource.metadata.get("related_resources"):
            resource = ResourceRef(
                resource.resource_type,
                resource.identifier,
                normalized_identifier=resource.normalized_identifier,
                workspace_relative=resource.workspace_relative,
                sensitive=resource.sensitive,
                metadata={**resource.metadata, "related_resources": related_resources},
            )
        operation = spec.operation or OperationKind.READ_FILE
        capability = spec.capabilities[0] if spec.capabilities else Capability.READ_WORKSPACE
        return PolicyRequest(
            session_id=getattr(self.planner, "session_id", self.trace.run_id if self.trace else "tool_session"),
            task_id=getattr(self.planner, "task_id", self.trace.run_id if self.trace else "tool_task"),
            phase_id=getattr(getattr(self.planner, "state", None), "current_phase", "tool_dispatch"),
            action_id=tool_call_id or "tool_dispatch",
            component=PolicyComponent.TOOL,
            operation=operation,
            capability=capability,
            subject=PolicySubject(subject_type="component", name="ToolExecutor"),
            resource=resource,
            reason=f"Dispatch tool {tool_name}",
            proposed_by_model=True,
            risk_tags=list(spec.risk_tags),
            metadata={
                "tool_name": tool_name,
                "argument_fingerprint": self._argument_trace_summary(validated_args)["hash"],
                "permission_level": spec.permission_level.value,
                "risk_tags": list(spec.risk_tags),
                "delegated_executor": spec.uses_mutation_manager or spec.uses_command_executor,
                "timeout": spec.timeout_seconds,
                "max_output_chars": spec.max_output_chars,
                "backend": spec.execution_backend.value,
                "resources": resource_details,
                **self._policy_argument_metadata(validated_args),
            },
            touches_workspace=capability
            in {
                Capability.READ_WORKSPACE,
                Capability.LIST_DIRECTORY,
                Capability.MUTATE_WORKSPACE,
                Capability.CREATE_FILE,
                Capability.DELETE_FILE,
                Capability.MOVE_FILE,
            },
            touches_secrets=any(item.sensitive for item in resources)
            or resource.sensitive
            or capability in {Capability.READ_SECRET, Capability.READ_ENV},
            destructive=capability == Capability.DELETE_FILE,
            long_running=operation == OperationKind.START_LONG_PROCESS,
            workspace_root=str(self.workspace_root),
        )

    def _resources_for(self, spec: ToolSpec, args: dict[str, Any]) -> list[ResourceRef]:
        if spec.resource_resolver is not None:
            return spec.resource_resolver(args, self.workspace_root)
        return [_default_resource(spec, args)]

    @staticmethod
    def _policy_argument_metadata(args: dict[str, Any]) -> dict[str, Any]:
        metadata: dict[str, Any] = {}
        for key in (
            "argv",
            "shell",
            "filesystem_mode",
            "network_mode",
            "purpose",
            "risk_acceptance_reason",
        ):
            if key in args:
                metadata[key] = args[key]
        if args.get("argv"):
            metadata["command"] = " ".join(str(part) for part in args["argv"])
        elif args.get("shell"):
            metadata["command"] = str(args["shell"])
        return metadata

    def _record_policy_observation(self, request: PolicyRequest, decision: PolicyDecision) -> None:
        if self.planner is None or not hasattr(self.planner, "record_policy_observation"):
            return
        self.planner.record_policy_observation(
            {
                "outcome": decision.outcome.value,
                "component": request.component.value,
                "operation": request.operation.value,
                "reason": decision.reason,
                "risk_level": decision.risk_level.value,
                "resource": redact_resource_identifier(request.resource.identifier),
                "resources": _redacted_resource_details(
                    request.metadata.get("resources")
                ),
                "decision_id": decision.decision_id,
            }
        )

    def _delegates_policy_decision(self, spec: ToolSpec, result: ToolResult) -> bool:
        if result.error_code != "sandbox_required":
            return False
        if not spec.delegates_policy_constraints:
            return False
        return spec.execution_backend == ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNNER

    def _preflight_delegated_handler(
        self,
        spec: ToolSpec,
        validated_args: Any,
    ) -> ToolResult | None:
        if spec.execution_backend != ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR:
            return None
        owner = getattr(spec.handler, "__self__", None)
        validator = getattr(owner, "validate_direct_command", None)
        request_builder = getattr(owner, "_request", None)
        if not callable(validator) or not callable(request_builder):
            return None
        try:
            validator(request_builder(validated_args))
        except ToolExecutionFailure as exc:
            return ToolResult.failure(
                code=exc.code,
                message=exc.message,
                details=exc.details,
            )
        return None

    def _execute_handler(self, spec: ToolSpec, validated_args: Any) -> tuple[ToolResult, str]:
        if (
            spec.execution_backend == ToolExecutionBackendKind.IN_PROCESS
            and self._handler_can_run_in_process(spec.handler, validated_args)
        ):
            return self._execute_handler_in_process(spec, validated_args)
        return self._execute_handler_in_thread(spec, validated_args)

    @staticmethod
    def _handler_can_run_in_process(handler: Any, validated_args: Any) -> bool:
        try:
            pickle.dumps((handler, validated_args))
        except Exception:
            return False
        return True

    def _execute_handler_in_process(
        self, spec: ToolSpec, validated_args: Any
    ) -> tuple[ToolResult, str]:
        context = multiprocessing.get_context("spawn")
        parent_conn, child_conn = context.Pipe(duplex=False)
        process = context.Process(
            target=_tool_process_entrypoint,
            args=(spec.handler, validated_args, child_conn),
        )
        try:
            process.start()
        except Exception as exc:
            parent_conn.close()
            child_conn.close()
            result = ToolResult.failure(
                code="execution_error",
                message=self._redactor.redact_text(f"Tool process failed to start: {exc}"),
                details={"type": type(exc).__name__},
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "process",
                },
            )
            return result, self._result_digest(result)
        child_conn.close()

        try:
            process.join(spec.timeout_seconds)
            if process.is_alive():
                process.terminate()
                process.join(1)
                killed = False
                if process.is_alive():
                    kill = getattr(process, "kill", None)
                    if kill is not None:
                        kill()
                        killed = True
                        process.join(1)
                still_alive = process.is_alive()
                result = ToolResult.failure(
                    code="timeout",
                    message=f"Tool timed out after {spec.timeout_seconds} seconds.",
                    metadata={
                        "backend": spec.execution_backend.value,
                        "handler_isolation": "process",
                        "timeout_type": "execution",
                        "timeout_terminated": not still_alive,
                        "timeout_killed": killed,
                        "timeout_untrusted_state": still_alive,
                        "process_exitcode": process.exitcode,
                    },
                )
                return result, self._result_digest(result)

            if not parent_conn.poll():
                result = ToolResult.failure(
                    code="execution_error",
                    message="Tool process exited without returning a result.",
                    details={"process_exitcode": process.exitcode},
                    metadata={
                        "backend": spec.execution_backend.value,
                        "handler_isolation": "process",
                    },
                )
                return result, self._result_digest(result)

            payload = parent_conn.recv()
        finally:
            parent_conn.close()
            try:
                process.close()
            except ValueError:
                pass

        return self._process_payload_to_result(spec, payload)

    def _execute_handler_in_thread(
        self, spec: ToolSpec, validated_args: Any
    ) -> tuple[ToolResult, str]:
        executor = ThreadPoolExecutor(max_workers=1)
        future = executor.submit(spec.handler, validated_args)
        shutdown_wait = True
        try:
            output = future.result(timeout=spec.timeout_seconds)
        except FutureTimeout:
            future.cancel()
            executor.shutdown(wait=False, cancel_futures=True)
            shutdown_wait = False
            result = ToolResult.failure(
                code="timeout",
                message=f"Tool timed out after {spec.timeout_seconds} seconds.",
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "thread",
                    "timeout_type": "execution",
                    "timeout_untrusted_state": True,
                },
            )
            return result, self._result_digest(result)
        except ToolExecutionFailure as exc:
            result = ToolResult.failure(
                code=exc.code,
                message=self._redactor.redact_text(exc.message),
                details=self._redactor.redact_value(exc.details),
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "thread",
                },
            )
            return result, self._result_digest(result)
        except Exception as exc:
            result = ToolResult.failure(
                code="execution_error",
                message=self._redactor.redact_text(str(exc)),
                details={"type": type(exc).__name__},
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "thread",
                },
            )
            return result, self._result_digest(result)
        finally:
            if shutdown_wait:
                executor.shutdown(wait=True, cancel_futures=True)

        return self._handler_output_to_result(spec, output, handler_isolation="thread")

    def _process_payload_to_result(
        self, spec: ToolSpec, payload: dict[str, Any]
    ) -> tuple[ToolResult, str]:
        status = payload.get("status")
        if status == "success":
            return self._handler_output_to_result(
                spec,
                payload.get("output"),
                handler_isolation="process",
            )
        if status == "tool_failure":
            result = ToolResult.failure(
                code=str(payload.get("code") or "execution_error"),
                message=self._redactor.redact_text(str(payload.get("message") or "")),
                details=self._redactor.redact_value(payload.get("details")),
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "process",
                },
            )
            return result, self._result_digest(result)

        result = ToolResult.failure(
            code="execution_error",
            message=self._redactor.redact_text(str(payload.get("message") or "Tool failed.")),
            details={"type": payload.get("type") or "Exception"},
            metadata={
                "backend": spec.execution_backend.value,
                "handler_isolation": "process",
            },
        )
        return result, self._result_digest(result)

    def _handler_output_to_result(
        self,
        spec: ToolSpec,
        output: Any,
        *,
        handler_isolation: str,
    ) -> tuple[ToolResult, str]:
        if spec.output_model is not None:
            try:
                output = spec.output_model.model_validate(output).model_dump(mode="json")
            except ValidationError as exc:
                result = ToolResult.failure(
                    code="output_validation_error",
                    message="Tool output failed validation.",
                    details=self._redactor.redact_value(exc.errors()),
                    metadata={
                        "backend": spec.execution_backend.value,
                        "handler_isolation": handler_isolation,
                    },
                )
                return result, self._result_digest(result)
        content, truncated, metadata, digest = self._limit_output(output, spec.max_output_chars)
        result = ToolResult.success(
            content=self._redactor.redact_value(content),
            truncated=truncated,
            metadata={
                **metadata,
                "backend": spec.execution_backend.value,
                "handler_isolation": handler_isolation,
            },
        )
        result.metadata["output_digest"] = digest
        return result, digest

    def _record_trace(
        self,
        *,
        request: ToolExecutionRequest,
        tool_call_id: str | None,
        tool_name: str,
        spec: ToolSpec | None,
        validated_args: dict[str, Any] | None,
        started_at: str,
        ended_at: str,
        duration_seconds: float,
        result: ToolResult,
        output_digest: str | None,
        cache_hit: bool,
    ) -> None:
        if self.trace is None:
            return
        args_summary = (
            self._argument_trace_summary(validated_args)
            if validated_args is not None
            else None
        )
        payload = {
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "batch_id": request.batch_id,
            "run_id": request.run_id,
            "session_id": request.session_id,
            "task_id": request.task_id,
            "phase_id": request.phase_id,
            "model_request_id": request.model_request_id,
            "model_response_id": request.model_response_id,
            "argument_digest": request.argument_digest,
            "policy_decision_id": result.metadata.get("policy_decision_id"),
            "argument_summary": args_summary,
            "permission_level": spec.permission_level.value if spec is not None else None,
            "risk_tags": list(spec.risk_tags) if spec is not None else [],
            "start": started_at,
            "end": ended_at,
            "duration_seconds": duration_seconds,
            "status": "ok" if result.ok else "error",
            "error_code": result.error_code,
            "truncated": result.truncated,
            "output_digest": output_digest,
            "cache_hit": cache_hit,
            "backend": spec.execution_backend.value if spec is not None else None,
        }
        if not hasattr(self.trace, "emit"):
            self.trace.record("tool_call", payload)
            return
        self._emit_trace(
            TraceEventType.TOOL_DISPATCH_COMPLETED if result.ok else TraceEventType.TOOL_DISPATCH_FAILED,
            summary=f"Tool {tool_name} {'completed' if result.ok else 'failed'}.",
            payload=payload,
            ids=self._request_trace_ids(request, action_id=tool_call_id),
            severity=TraceSeverity.INFO if result.ok else TraceSeverity.ERROR,
        )

    def _emit_trace(
        self,
        event_type: TraceEventType,
        *,
        summary: str,
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> None:
        if self.trace is None or not hasattr(self.trace, "emit"):
            return
        resolved_ids = {
            "session_id": getattr(self.planner, "session_id", None),
            "task_id": getattr(self.planner, "task_id", None),
            "phase_id": getattr(getattr(self.planner, "state", None), "current_phase", None),
        }
        resolved_ids.update(ids or {})
        self.trace.emit(
            event_type,
            component="tool",
            summary=summary,
            payload=payload or {},
            ids=resolved_ids,
            severity=severity,
        )

    def _check_execution_boundary(self, spec: ToolSpec) -> ToolResult | None:
        if (
            spec.permission_level == PermissionLevel.WRITE
            and not spec.uses_mutation_manager
            and spec.execution_backend
            not in {
                ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER,
                ToolExecutionBackendKind.DELEGATED_EDIT_EXECUTOR,
            }
        ):
            return ToolResult.failure(
                code="invalid_operation",
                message="Write tools must execute through WorkspaceMutationManager.",
                details={"tool_name": spec.name},
            )
        if spec.execution_backend == ToolExecutionBackendKind.DELEGATED_EDIT_EXECUTOR:
            if not spec.uses_edit_executor or not spec.uses_mutation_manager:
                return ToolResult.failure(
                    code="invalid_operation",
                    message="EditExecutor tools must declare edit executor usage and mutation delegation.",
                    details={"tool_name": spec.name},
                )
        if (
            spec.permission_level == PermissionLevel.SHELL
            and not spec.uses_command_executor
            and spec.execution_backend
            not in {
                ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR,
                ToolExecutionBackendKind.DELEGATED_VERIFICATION_RUNNER,
            }
        ):
            return ToolResult.failure(
                code="invalid_operation",
                message="Shell tools must execute through CommandExecutor.",
                details={"tool_name": spec.name},
            )
        return None

    def _dry_run_error(self, spec: ToolSpec) -> ToolResult | None:
        if not self.dry_run:
            return None
        read_only_side_effects = {
            ToolSideEffectKind.NONE,
            ToolSideEffectKind.READ_WORKSPACE,
        }
        if (
            spec.permission_level == PermissionLevel.READ_ONLY
            and spec.side_effects in read_only_side_effects
        ):
            return None
        return ToolResult.failure(
            code="dry_run_blocked",
            message="Dry-run mode blocks mutation, command, verification, and other side-effect tools.",
            details={
                "tool_name": spec.name,
                "permission_level": spec.permission_level.value,
                "side_effects": spec.side_effects.value if spec.side_effects else None,
                "backend": spec.execution_backend.value,
            },
        )

    def _delegated_backend_error(self, spec: ToolSpec) -> ToolResult | None:
        if spec.execution_backend == ToolExecutionBackendKind.IN_PROCESS:
            return None
        if self.standalone_can_execute:
            return None
        return ToolResult.failure(
            code="delegated_backend_unavailable",
            message=f"Delegated backend is unavailable: {spec.execution_backend.value}",
            metadata={"backend": spec.execution_backend.value},
        )

    def _failure_from_tool_error(self, error: ToolError) -> ToolResult:
        return ToolResult.failure(
            code=error.code,
            message=self._redactor.redact_text(error.message),
            details=self._redactor.redact_value(error.details),
        )

    @staticmethod
    def _parse_arguments(raw_arguments: Any) -> Any:
        if isinstance(raw_arguments, dict):
            return raw_arguments
        return json.loads(raw_arguments)

    @staticmethod
    def _should_cache(spec: ToolSpec) -> bool:
        return (
            bool(spec.cache_policy and spec.cache_policy.cacheable)
            and spec.permission_level == PermissionLevel.READ_ONLY
            and bool(spec.idempotency_policy and spec.idempotency_policy.idempotent)
            and spec.sensitivity not in {
                ToolSensitivityLevel.SENSITIVE,
                ToolSensitivityLevel.SECRET,
            }
        )

    def _cache_key(self, spec: ToolSpec, validated_args: dict[str, Any]) -> str:
        payload = {
            "tool_name": spec.name,
            "version": spec.version,
            "schema": self._model_schema_fingerprint(spec),
            "arguments": validated_args,
            "workspace_root": str(self.workspace_root),
            "paths": self._file_snapshots(spec, validated_args),
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    def _file_snapshots(self, spec: ToolSpec, args: dict[str, Any]) -> dict[str, Any]:
        snapshots: dict[str, Any] = {}
        for path in self._touched_paths(spec, args):
            full = (self.workspace_root / path).resolve(strict=False)
            if full.exists() and full.is_file():
                try:
                    snapshots[path] = {
                        "sha256": hashlib.sha256(full.read_bytes()).hexdigest(),
                        "size": full.stat().st_size,
                    }
                except OSError:
                    snapshots[path] = {"error": "unreadable"}
            elif full.exists() and full.is_dir():
                snapshots[path] = self._directory_snapshot(full)
            else:
                snapshots[path] = {"exists": False}
        return snapshots

    def _directory_snapshot(self, root: Path) -> dict[str, Any]:
        entries: list[dict[str, Any]] = []
        for child in sorted(root.rglob("*")):
            if ".git" in child.parts or ".singularity" in child.parts:
                continue
            if self._is_sensitive_path(child):
                continue
            try:
                relative = child.relative_to(root).as_posix()
            except ValueError:
                continue
            if child.is_file():
                try:
                    entries.append(
                        {
                            "path": relative,
                            "sha256": hashlib.sha256(child.read_bytes()).hexdigest(),
                            "size": child.stat().st_size,
                        }
                    )
                except OSError:
                    entries.append({"path": relative, "error": "unreadable"})
        digest = hashlib.sha256(
            json.dumps(entries, ensure_ascii=False, sort_keys=True).encode("utf-8")
        ).hexdigest()
        return {"digest": digest, "file_count": len(entries)}

    def _is_sensitive_path(self, path: Path) -> bool:
        parts = [part.lower() for part in path.parts]
        name = path.name.lower()
        if name == ".env" or name.startswith(".env."):
            return True
        if any(part in {".ssh", ".gnupg", ".aws", ".azure"} for part in parts):
            return True
        if any(marker in name for marker in ("token", "secret", "credential", "password", "api_key")):
            return True
        if any(name.endswith(ext) for ext in (".pem", ".key", ".p12", ".pfx")):
            return True
        return False

    def _touched_paths(self, spec: ToolSpec, args: dict[str, Any]) -> tuple[str, ...]:
        paths: list[str] = []
        for resource in self._resources_for(spec, args):
            if resource.resource_type in {"file", "directory"} and resource.workspace_relative:
                paths.append(Path(resource.identifier).as_posix())
        return tuple(sorted(set(paths)))

    @staticmethod
    def _model_schema_fingerprint(spec: ToolSpec) -> str:
        schema = spec.input_model.model_json_schema()
        text = json.dumps(schema, ensure_ascii=False, sort_keys=True, default=str)
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def _is_sensitive_result(self, spec: ToolSpec, result: ToolResult) -> bool:
        if spec.sensitivity in {ToolSensitivityLevel.SENSITIVE, ToolSensitivityLevel.SECRET}:
            return True
        text = self._output_text(result.content)
        return self._redactor.redact_text(text) != text

    def _limit_output(
        self, output: Any, max_output_chars: int
    ) -> tuple[Any, bool, dict[str, Any], str]:
        text = self._output_text(output)
        digest = self._digest(text)
        original_chars = len(text)
        if original_chars <= max_output_chars:
            return output, False, {
                "original_chars": original_chars,
                "returned_chars": original_chars,
                "cache_hit": False,
            }, digest
        truncated = self._truncate_head_tail(text, max_output_chars)
        return truncated, True, {
            "original_chars": original_chars,
            "returned_chars": len(truncated),
            "cache_hit": False,
        }, digest

    @staticmethod
    def _truncate_head_tail(text: str, max_chars: int) -> str:
        marker = "\n...[truncated]...\n"
        if max_chars <= len(marker) + 2:
            return text[:max_chars]
        head_chars = (max_chars - len(marker)) // 2
        tail_chars = max_chars - len(marker) - head_chars
        return f"{text[:head_chars]}{marker}{text[-tail_chars:]}"

    @staticmethod
    def _output_text(output: Any) -> str:
        if isinstance(output, str):
            return output
        return json.dumps(output, ensure_ascii=False, sort_keys=True, default=str)

    @staticmethod
    def _digest(text: str) -> str:
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def _argument_trace_summary(self, arguments: Any) -> dict[str, Any]:
        text = json.dumps(
            self._redactor.redact_value(arguments),
            ensure_ascii=False,
            sort_keys=True,
            default=str,
        )
        if isinstance(arguments, dict):
            keys = sorted(str(key) for key in arguments)
            count = len(arguments)
            shape = "object"
        elif isinstance(arguments, list):
            keys = []
            count = len(arguments)
            shape = "array"
        else:
            keys = []
            count = 1 if arguments is not None else 0
            shape = type(arguments).__name__
        return {
            "shape": shape,
            "keys": keys,
            "count": count,
            "hash": self._digest(text),
        }

    def _result_digest(self, result: ToolResult) -> str:
        dumped = self._redactor.redact_value(result.model_dump(mode="json"))
        return self._digest(json.dumps(dumped, ensure_ascii=False, sort_keys=True, default=str))

    def _safe_request_details(self, request: PolicyRequest) -> dict[str, Any]:
        payload = request.to_dict()
        payload["resource"]["identifier"] = redact_resource_identifier(
            request.resource.identifier
        )
        payload["resource"]["normalized_identifier"] = (
            redact_resource_identifier(request.resource.normalized_identifier)
            if request.resource.normalized_identifier
            else None
        )
        payload["metadata"] = {
            key: (
                _redacted_resource_details(value)
                if key == "resources"
                else value
            )
            for key, value in payload.get("metadata", {}).items()
            if key != "arguments"
        }
        return self._redactor.redact_value(payload)

    def _remember_replay(
        self,
        tool_call_id: str | None,
        args_fingerprint: str,
        spec: ToolSpec,
        result: ToolResult,
    ) -> None:
        self._ledger.remember(
            tool_call_id,
            args_fingerprint,
            result,
            replay_allowed=bool(
                spec.idempotency_policy
                and spec.idempotency_policy.idempotent
                and spec.idempotency_policy.replay_returns_previous
            ),
        )


def _redacted_resource_details(value: Any) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        return []
    resources: list[dict[str, Any]] = []
    for item in value:
        if not isinstance(item, dict):
            continue
        identifier = item.get("identifier")
        normalized_identifier = item.get("normalized_identifier")
        resources.append(
            {
                **item,
                "identifier": redact_resource_identifier(str(identifier))
                if identifier is not None
                else "",
                "normalized_identifier": (
                    redact_resource_identifier(str(normalized_identifier))
                    if normalized_identifier is not None
                    else None
                ),
            }
        )
    return resources


def _default_resource(spec: ToolSpec, args: dict[str, Any]) -> ResourceRef:
    name = spec.name
    if name in {"edit_plan", "edit_preview", "edit_apply"}:
        operations = args.get("operations") or []
        if isinstance(operations, list):
            for operation in operations:
                if isinstance(operation, dict) and operation.get("path"):
                    return ResourceRef("file", str(operation.get("path")), workspace_relative=True)
        return ResourceRef("workspace", "edit", workspace_relative=True)
    if name in {"read_file", "workspace_create_file", "workspace_delete_file", "workspace_replace_text"}:
        return ResourceRef("file", str(args.get("path") or "."), workspace_relative=True)
    if name == "workspace_move_file":
        return ResourceRef("file", str(args.get("path") or "."), workspace_relative=True)
    if name in {"list_files", "search_text"}:
        return ResourceRef("directory", str(args.get("path") or "."), workspace_relative=True)
    if name == "start_process":
        return ResourceRef("command", _command_identifier(args))
    if name == "stop_process":
        return ResourceRef("process", str(args.get("process_id") or ""))
    if name == "run_command":
        return ResourceRef("command", _command_identifier(args))
    if name in {"plan_verification", "get_verification_result"}:
        return ResourceRef("workspace", name, workspace_relative=True)
    if name in {"run_verification", "rerun_check"}:
        return ResourceRef("workspace", name, workspace_relative=True)
    if spec.permission_level == PermissionLevel.SHELL:
        return ResourceRef("command", _command_identifier(args) or name)
    return ResourceRef("tool", name)


def _command_identifier(args: dict[str, Any]) -> str:
    if args.get("shell"):
        return str(args["shell"])
    if args.get("argv"):
        return " ".join(str(part) for part in args["argv"])
    return ""


def _is_cancellation_error(exc: BaseException) -> bool:
    return (
        getattr(exc, "code", None) == "cancelled"
        or exc.__class__.__name__ == "CancellationError"
    )


def _paths_overlap(left: str, right: str) -> bool:
    left_parts = PurePosixPath(Path(left).as_posix()).parts
    right_parts = PurePosixPath(Path(right).as_posix()).parts
    return (
        left_parts == right_parts
        or left_parts[: len(right_parts)] == right_parts
        or right_parts[: len(left_parts)] == left_parts
    )


def _policy_error_code(outcome: DecisionOutcome) -> str:
    mapping = {
        DecisionOutcome.DENY: "policy_denied",
        DecisionOutcome.REQUIRE_REVIEW: "approval_required",
        DecisionOutcome.SANDBOX_REQUIRED: "sandbox_required",
        DecisionOutcome.ASK_USER: "policy_ask_user_required",
        DecisionOutcome.ESCALATE: "policy_escalation_required",
    }
    return mapping.get(outcome, "policy_denied")


def _decision_allowed_by_grant(decision: PolicyDecision, grant_id: str) -> PolicyDecision:
    return decision.model_copy_with(
        outcome=DecisionOutcome.ALLOW,
        reason="Action allowed by matching ApprovalGrant.",
        approval_grant_id=grant_id,
        required_approval=None,
    )
