from __future__ import annotations

import hashlib
import json
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.observability.protocols import TraceEmitterProtocol
from singularity.observability.redaction import shared_trace_redactor
from singularity.policy import (
    ApprovalGate,
)
from singularity.tools.cache import ToolResultCache
from singularity.tools.execution_cache import ToolExecutionCache
from singularity.tools.execution_dispatch import ToolExecutionDispatcher
from singularity.tools.execution_pipeline import (
    PLANNER_ACTION_ID_METADATA_KEY,
    PLANNER_UPDATE_DEFERRED_METADATA_KEY,
    ToolExecutionPipelineState,
)
from singularity.tools.execution_policy import ToolExecutionPolicyGate, ToolPolicyEngineProtocol
from singularity.tools.execution_resources import is_read_only_side_effect
from singularity.tools.execution_trace import ToolExecutionTraceRecorder
from singularity.tools.idempotency import IdempotencyLedger
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutionRequest,
    ToolResult,
    ToolSpec,
)
from singularity.tools.policy import ToolPolicy
from singularity.tools.registry import ToolRegistry

_ExecutionPipelineState = ToolExecutionPipelineState


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
        self._redactor = shared_trace_redactor()
        self.execution_policy = ToolExecutionPolicyGate(
            policy_engine=self.policy_engine,
            approval_gate=self.approval_gate,
            workspace_root=self.workspace_root,
            trace=self.trace,
            planner=self.planner,
            redactor=self._redactor,
            argument_summary=self._argument_trace_summary,
        )
        self.execution_cache = ToolExecutionCache(
            cache=self._cache,
            workspace_root=self.workspace_root,
            redactor=self._redactor,
            result_digest=self._result_digest,
            output_text=self._output_text,
            throw_if_cancelled=self._throw_if_cancelled,
            standalone_can_execute=self.standalone_can_execute,
        )
        self.execution_dispatch = ToolExecutionDispatcher(
            workspace_root=self.workspace_root,
            redactor=self._redactor,
            cache=self.execution_cache,
            emit_trace=self._emit_trace,
            request_trace_ids=self._request_trace_ids,
            argument_summary=self._argument_trace_summary,
            result_digest=self._result_digest,
            limit_output=self._limit_output,
            throw_if_cancelled=self._throw_if_cancelled,
            update_planner=self._update_planner,
        )
        self.execution_trace = ToolExecutionTraceRecorder(
            trace=self.trace,
            planner=self.planner,
            redactor=self._redactor,
            annotate_result_metadata=self._annotate_result_metadata,
            argument_summary=self._argument_trace_summary,
            request_trace_ids=self._request_trace_ids,
            emit_trace=self._emit_trace,
            safe_update_planner=self._safe_update_planner,
        )
        self._pipeline_state_type = ToolExecutionPipelineState
        self.cancellation_token: Any | None = None

    def execute_tool_call(self, tool_call: dict[str, Any]) -> ToolResult:
        return self.execute_request(ToolExecutionRequest.from_provider_tool_call(tool_call))

    def execute_request(self, request: ToolExecutionRequest | dict[str, Any]) -> ToolResult:
        request = self._normalize_execution_request(request)
        state = self._pipeline_state_type(
            request=request,
            started_at=datetime.now(UTC).isoformat(),
            started=time.perf_counter(),
            tool_call_id=request.tool_call_id,
            tool_name=request.tool_name or "<unknown>",
        )

        try:
            for stage in (
                self._stage_load_spec,
                self._stage_validate_arguments,
                self._stage_replay_precheck,
                self._stage_pre_policy_preflight,
                self._stage_enforce_policy,
                self._stage_authorize_with_planner,
                self._stage_cache_precheck,
            ):
                result = stage(state)
                if result is not None:
                    return self._finish_pipeline_result(state, result)
            return self._finish_pipeline_result(state, self._stage_dispatch(state))
        except Exception as exc:
            if _is_cancellation_error(exc):
                raise
            result = ToolResult.failure(
                code="internal_error",
                message=self._redactor.redact_text(str(exc)),
                details={"type": type(exc).__name__},
            )
            return self._finish_pipeline_result(state, result)
        finally:
            self._finalize_pipeline_state(state)

    def _stage_load_spec(self, state: _ExecutionPipelineState) -> ToolResult | None:
        self._throw_if_cancelled()
        state.spec = self.registry.get(state.tool_name)
        if state.spec is None or not state.spec.enabled:
            return ToolResult.failure(
                code="tool_not_found",
                message=f"Unknown tool: {state.tool_name}",
            )
        return None

    def _stage_validate_arguments(self, state: _ExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        try:
            state.parsed_args = self._arguments_for_execution_validation(state.request)
        except json.JSONDecodeError as exc:
            self._emit_trace(
                TraceEventType.TOOL_VALIDATION_FAILED,
                summary=f"Tool {state.tool_name} arguments were invalid JSON.",
                payload={
                    "tool_name": state.tool_name,
                    "tool_call_id": state.tool_call_id,
                    "batch_id": state.request.batch_id,
                    "argument_digest": state.request.argument_digest,
                    "validation_scope": "execution_validation",
                },
                ids=self._request_trace_ids(state.request, action_id=state.tool_call_id),
                severity=TraceSeverity.ERROR,
            )
            return ToolResult.failure(
                code="bad_arguments_json",
                message=f"Invalid JSON arguments: {exc}",
            )

        self._emit_trace(
            TraceEventType.TOOL_VALIDATION_STARTED,
            summary=f"Validating tool {state.tool_name}.",
            payload={
                "tool_name": state.tool_name,
                "tool_call_id": state.tool_call_id,
                "batch_id": state.request.batch_id,
                "argument_digest": state.request.argument_digest,
                "validation_scope": "execution_validation",
                "arguments": self._argument_trace_summary(state.parsed_args),
            },
            ids=self._request_trace_ids(state.request, action_id=state.tool_call_id),
        )
        try:
            state.validated = state.spec.input_model.model_validate(state.parsed_args)
        except ValidationError as exc:
            result = ToolResult.failure(
                code="validation_error",
                message="Tool arguments failed validation.",
                details=self._redactor.redact_value(exc.errors()),
            )
            self._emit_trace(
                TraceEventType.TOOL_VALIDATION_FAILED,
                summary=f"Tool {state.tool_name} arguments failed validation.",
                payload={
                    "tool_name": state.tool_name,
                    "tool_call_id": state.tool_call_id,
                    "batch_id": state.request.batch_id,
                    "argument_digest": state.request.argument_digest,
                    "validation_scope": "execution_validation",
                    "errors": result.error.details if result.error else None,
                },
                ids=self._request_trace_ids(state.request, action_id=state.tool_call_id),
                severity=TraceSeverity.ERROR,
            )
            return result
        state.validated_args = state.validated.model_dump(mode="json")
        state.args_fingerprint = self._digest(
            json.dumps(state.validated_args, ensure_ascii=False, sort_keys=True, default=str)
        )
        return None

    def _stage_replay_precheck(self, state: _ExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.args_fingerprint is not None
        replay = self._ledger.check(
            state.tool_call_id,
            state.args_fingerprint,
            replay_allowed=state.spec.idempotency_policy.replay_returns_previous
            if state.spec.idempotency_policy
            else state.spec.idempotent,
        )
        if replay is None:
            return None
        state.output_digest = replay.metadata.get("output_digest") or self._result_digest(replay)
        return replay

    def _stage_pre_policy_preflight(self, state: _ExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.validated is not None
        result = self._check_execution_boundary(state.spec)
        if result is not None:
            state.remember_replay = True
            return result
        result = self._dry_run_error(state.spec)
        if result is not None:
            state.remember_replay = True
            return result
        result = self.execution_dispatch.preflight_delegated_handler(state.spec, state.validated)
        if result is not None:
            state.remember_replay = True
            return result
        return None

    def _stage_enforce_policy(self, state: _ExecutionPipelineState) -> ToolResult | None:
        return self.execution_policy.enforce(state)

    def _stage_authorize_with_planner(self, state: _ExecutionPipelineState) -> ToolResult | None:
        assert state.spec is not None
        assert state.validated_args is not None
        planner_decision = self._authorize_with_planner(
            tool_name=state.tool_name,
            tool_call_id=state.tool_call_id,
            spec=state.spec,
            validated_args=state.validated_args,
        )
        if planner_decision is not None and not planner_decision.allowed:
            self._record_planner_denial(state.tool_name, planner_decision)
            state.planner_updated = True
            state.remember_replay = True
            return ToolResult.failure(
                code=planner_decision.error_code or "action_not_allowed",
                message="Planner denied tool execution.",
                details={
                    "planner_reason": planner_decision.reason,
                    "risk_decision": planner_decision.risk_decision.value,
                },
            )
        if planner_decision is not None and planner_decision.action is not None:
            state.planner_action_id = planner_decision.action.action_id
        return None

    def _stage_cache_precheck(self, state: _ExecutionPipelineState) -> ToolResult | None:
        return self.execution_cache.precheck(state)

    def _stage_dispatch(self, state: _ExecutionPipelineState) -> ToolResult:
        return self.execution_dispatch.dispatch(state)

    def _finish_pipeline_result(
        self,
        state: _ExecutionPipelineState,
        result: ToolResult,
    ) -> ToolResult:
        state.result = result
        if state.planner_action_id:
            result.metadata[PLANNER_ACTION_ID_METADATA_KEY] = state.planner_action_id
        if state.defer_planner_update and not state.planner_updated:
            result.metadata[PLANNER_UPDATE_DEFERRED_METADATA_KEY] = True
        if state.output_digest is None:
            state.output_digest = self._result_digest(result)
        if (
            state.remember_replay
            and state.spec is not None
            and state.args_fingerprint is not None
        ):
            self._remember_replay(
                state.tool_call_id,
                state.args_fingerprint,
                state.spec,
                result,
            )
        return result

    def _finalize_pipeline_state(self, state: _ExecutionPipelineState) -> None:
        self.execution_trace.finalize(state)

    def invalidate_paths(self, paths: list[str]) -> None:
        self.execution_cache.invalidate_paths(paths)

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
                severity=TraceSeverity.ERROR,
            )
            raise RuntimeError("planner observation update failed") from exc

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
        if spec.execution_backend == ToolExecutionBackendKind.DELEGATED_EDIT_EXECUTOR and (
            not spec.uses_edit_executor or not spec.uses_mutation_manager
        ):
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
        if (
            spec.permission_level == PermissionLevel.READ_ONLY
            and is_read_only_side_effect(spec.side_effects)
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

    @staticmethod
    def _parse_arguments(raw_arguments: Any) -> Any:
        if isinstance(raw_arguments, dict):
            return raw_arguments
        return json.loads(raw_arguments)

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


def _is_cancellation_error(exc: BaseException) -> bool:
    return (
        getattr(exc, "code", None) == "cancelled"
        or exc.__class__.__name__ == "CancellationError"
    )
