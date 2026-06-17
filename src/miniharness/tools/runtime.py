from __future__ import annotations

import hashlib
import json
import time
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeout
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from pydantic import ValidationError

from miniharness.tools.models import (
    PermissionLevel,
    ToolError,
    ToolExecutionFailure,
    ToolResult,
    ToolSpec,
)
from miniharness.tools.policy import ToolPolicy
from miniharness.tools.registry import ToolRegistry
from miniharness.trace import TraceWriter
from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyRequest,
    PolicyRuntime,
    PolicySubject,
    ResourceRef,
    RuntimeName,
)
from miniharness.policy.audit import redact


class ToolRuntime:
    def __init__(
        self,
        *,
        registry: ToolRegistry,
        policy: ToolPolicy,
        trace: TraceWriter | None,
        workspace_root: Path,
        planner: Any | None = None,
        policy_runtime: PolicyRuntime | None = None,
    ) -> None:
        self.registry = registry
        self.policy = policy
        self.trace = trace
        self.workspace_root = workspace_root.resolve()
        self.planner = planner
        self.policy_runtime = policy_runtime or PolicyRuntime(
            PolicyConfig.runtime_default(self.workspace_root)
        )
        self._cache: dict[str, ToolResult] = {}

    def execute_tool_call(self, tool_call: dict[str, Any]) -> ToolResult:
        started_at = datetime.now(UTC).isoformat()
        started = time.perf_counter()
        tool_call_id = tool_call.get("id")
        function = tool_call.get("function") or {}
        tool_name = function.get("name") or "<unknown>"
        spec: ToolSpec | None = None
        validated_args: dict[str, Any] | None = None
        planner_action_id: str | None = None
        cache_hit = False
        output_digest: str | None = None

        try:
            spec = self.registry.get(tool_name)
            if spec is None:
                result = ToolResult.failure(
                    code="tool_not_found",
                    message=f"Unknown tool: {tool_name}",
                )
                output_digest = self._result_digest(result)
                return result

            raw_arguments = function.get("arguments") or "{}"
            try:
                arguments = self._parse_arguments(raw_arguments)
            except json.JSONDecodeError as exc:
                self._emit_trace(
                    TraceEventType.TOOL_VALIDATION_FAILED,
                    summary=f"Tool {tool_name} arguments were invalid JSON.",
                    payload={"tool_name": tool_name, "tool_call_id": tool_call_id},
                    ids={"action_id": tool_call_id},
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
                    "arguments": self._argument_trace_summary(arguments),
                },
                ids={"action_id": tool_call_id},
            )
            try:
                validated = spec.input_model.model_validate(arguments)
            except ValidationError as exc:
                self._emit_trace(
                    TraceEventType.TOOL_VALIDATION_FAILED,
                    summary=f"Tool {tool_name} arguments failed validation.",
                    payload={
                        "tool_name": tool_name,
                        "tool_call_id": tool_call_id,
                        "errors": exc.errors(),
                    },
                    ids={"action_id": tool_call_id},
                    severity=TraceSeverity.ERROR,
                )
                result = ToolResult.failure(
                    code="validation_error",
                    message="Tool arguments failed validation.",
                    details=exc.errors(),
                )
                output_digest = self._result_digest(result)
                return result
            validated_args = validated.model_dump(mode="json")

            policy_error = self.policy.check(spec)
            if policy_error is not None:
                result = ToolResult.failure(
                    code=policy_error.code,
                    message=policy_error.message,
                    details=policy_error.details,
                )
                output_digest = self._result_digest(result)
                return result
            if (
                spec.permission_level == PermissionLevel.WRITE
                and not spec.uses_mutation_runtime
            ):
                self._emit_trace(
                    TraceEventType.TOOL_DISPATCH_FAILED,
                    summary=f"Tool {tool_name} failed runtime boundary validation.",
                    payload={"tool_name": tool_name, "reason": "write_without_mutation_runtime"},
                    ids={"action_id": tool_call_id},
                    severity=TraceSeverity.ERROR,
                )
                result = ToolResult.failure(
                    code="invalid_operation",
                    message=(
                        "Write tools must execute through Workspace Mutation Runtime."
                    ),
                    details={"tool_name": spec.name},
                )
                output_digest = self._result_digest(result)
                return result
            if (
                spec.permission_level == PermissionLevel.SHELL
                and not spec.uses_command_runtime
            ):
                self._emit_trace(
                    TraceEventType.TOOL_DISPATCH_FAILED,
                    summary=f"Tool {tool_name} failed runtime boundary validation.",
                    payload={"tool_name": tool_name, "reason": "shell_without_command_runtime"},
                    ids={"action_id": tool_call_id},
                    severity=TraceSeverity.ERROR,
                )
                result = ToolResult.failure(
                    code="invalid_operation",
                    message="Shell tools must execute through CommandRuntime.",
                    details={"tool_name": spec.name},
                )
                output_digest = self._result_digest(result)
                return result

            policy_decision = self._enforce_policy(
                tool_name=tool_name,
                spec=spec,
                validated_args=validated_args,
                tool_call_id=tool_call_id,
            )
            if policy_decision is not None:
                result = policy_decision
                output_digest = self._result_digest(result)
                return result

            planner_decision = self._authorize_with_planner(
                tool_name=tool_name,
                tool_call_id=tool_call_id,
                spec=spec,
                validated_args=validated_args,
            )
            if planner_decision is not None and not planner_decision.allowed:
                self._emit_trace(
                    TraceEventType.TOOL_DISPATCH_FAILED,
                    summary=f"Tool {tool_name} was denied by planner.",
                    payload={
                        "tool_name": tool_name,
                        "planner_reason": planner_decision.reason,
                        "risk_decision": planner_decision.risk_decision.value,
                    },
                    ids={"action_id": tool_call_id},
                    severity=TraceSeverity.WARNING,
                )
                result = ToolResult.failure(
                    code=planner_decision.error_code or "action_not_allowed",
                    message="Planner denied tool execution.",
                    details={
                        "planner_reason": planner_decision.reason,
                        "risk_decision": planner_decision.risk_decision.value,
                    },
                )
                output_digest = self._result_digest(result)
                return result
            if planner_decision is not None and planner_decision.action is not None:
                planner_action_id = planner_decision.action.action_id

            cache_key = self._cache_key(spec, validated_args)
            if self._should_cache(spec) and cache_key in self._cache:
                cache_hit = True
                result = self._cache[cache_key].model_copy(deep=True)
                result.metadata["cache_hit"] = True
                output_digest = result.metadata.get("output_digest")
                return result

            self._emit_trace(
                TraceEventType.TOOL_DISPATCH_STARTED,
                summary=f"Dispatching tool {tool_name}.",
                payload={
                    "tool_name": tool_name,
                    "tool_call_id": tool_call_id,
                    "permission_level": spec.permission_level.value,
                    "risk_tags": list(spec.risk_tags),
                    "arguments": self._argument_trace_summary(validated_args),
                },
                ids={"action_id": planner_action_id or tool_call_id},
            )
            result, output_digest = self._execute_handler(spec, validated)
            self._update_planner(
                tool_call_id=tool_call_id,
                tool_name=tool_name,
                result=result,
                action_id=planner_action_id,
            )
            if self._should_cache(spec) and result.ok:
                self._cache[cache_key] = result.model_copy(deep=True)
            return result
        except Exception as exc:
            result = ToolResult.failure(
                code="internal_error",
                message=str(exc),
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
                self._record_trace(
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

    def _enforce_policy(
        self,
        *,
        tool_name: str,
        spec: ToolSpec,
        validated_args: dict[str, Any],
        tool_call_id: str | None,
    ) -> ToolResult | None:
        request = self._policy_request(
            tool_name=tool_name,
            spec=spec,
            validated_args=validated_args,
            tool_call_id=tool_call_id,
        )
        decision = self.policy_runtime.enforce(request)
        self._record_policy_trace(request, decision)
        if decision.outcome == DecisionOutcome.ALLOW:
            return None
        if (
            decision.outcome == DecisionOutcome.SANDBOX_REQUIRED
            and spec.uses_command_runtime
        ):
            self._record_policy_observation(request, decision)
            return None
        self._record_policy_observation(request, decision)
        return ToolResult.failure(
            code=_policy_error_code(decision.outcome),
            message=decision.reason,
            details={"policy": decision.to_dict(), "request": request.to_dict()},
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
        operation, capability, resource = _tool_policy_shape(
            tool_name,
            spec,
            validated_args,
        )
        return PolicyRequest(
            session_id=getattr(self.planner, "session_id", self.trace.run_id if self.trace else "tool_session"),
            task_id=getattr(self.planner, "task_id", self.trace.run_id if self.trace else "tool_task"),
            phase_id=getattr(getattr(self.planner, "state", None), "current_phase", "tool_dispatch"),
            action_id=tool_call_id or "tool_dispatch",
            runtime=RuntimeName.TOOL,
            operation=operation,
            capability=capability,
            subject=PolicySubject(subject_type="runtime", name="ToolRuntime"),
            resource=resource,
            reason=f"Dispatch tool {tool_name}",
            proposed_by_model=True,
            metadata={
                "tool_name": tool_name,
                "arguments": validated_args,
                "permission_level": spec.permission_level.value,
                "risk_tags": list(spec.risk_tags),
                "delegated_runtime": spec.uses_mutation_runtime or spec.uses_command_runtime,
            },
            touches_workspace=capability
            in {
                Capability.READ_WORKSPACE,
                Capability.MUTATE_WORKSPACE,
                Capability.CREATE_FILE,
                Capability.DELETE_FILE,
                Capability.MOVE_FILE,
            },
            destructive=capability == Capability.DELETE_FILE,
            long_running=operation == OperationKind.START_LONG_PROCESS,
            workspace_root=str(self.workspace_root),
        )

    def _record_policy_observation(
        self,
        request: PolicyRequest,
        decision: Any,
    ) -> None:
        if self.planner is None or not hasattr(self.planner, "record_policy_observation"):
            return
        self.planner.record_policy_observation(
            {
                "outcome": decision.outcome.value,
                "runtime": request.runtime.value,
                "operation": request.operation.value,
                "reason": decision.reason,
                "risk_level": decision.risk_level.value,
                "resource": request.resource.identifier,
                "decision_id": decision.decision_id,
            }
        )

    def _record_policy_trace(self, request: PolicyRequest, decision: Any) -> None:
        if self.trace is None:
            return
        self.trace.record(
            "policy",
            redact(
                {
                    "request_id": request.request_id,
                    "decision_id": decision.decision_id,
                    "runtime": request.runtime.value,
                    "operation": request.operation.value,
                    "capability": request.capability.value,
                    "resource": request.resource.identifier,
                    "outcome": decision.outcome.value,
                    "risk_level": decision.risk_level.value,
                    "risk_tags": [
                        tag.value if hasattr(tag, "value") else str(tag)
                        for tag in decision.risk_tags
                    ],
                    "reason": decision.reason,
                    "rule_ids": decision.rule_ids,
                    "approval_required": decision.required_approval is not None,
                }
            ),
        )

    def _execute_handler(
        self, spec: ToolSpec, validated_args: Any
    ) -> tuple[ToolResult, str]:
        executor = ThreadPoolExecutor(max_workers=1)
        future = executor.submit(spec.handler, validated_args)
        try:
            output = future.result(timeout=spec.timeout_seconds)
        except FutureTimeout:
            future.cancel()
            result = ToolResult.failure(
                code="timeout",
                message=f"Tool timed out after {spec.timeout_seconds} seconds.",
            )
            return result, self._result_digest(result)
        except ToolExecutionFailure as exc:
            result = ToolResult.failure(
                code=exc.code,
                message=exc.message,
                details=exc.details,
            )
            return result, self._result_digest(result)
        except Exception as exc:
            result = ToolResult.failure(
                code="execution_error",
                message=str(exc),
                details={"type": type(exc).__name__},
            )
            return result, self._result_digest(result)
        finally:
            executor.shutdown(wait=False, cancel_futures=True)

        content, truncated, metadata, digest = self._limit_output(
            output, spec.max_output_chars
        )
        result = ToolResult.success(
            content=content,
            truncated=truncated,
            metadata=metadata,
        )
        result.metadata["output_digest"] = digest
        return result, digest

    def _record_trace(
        self,
        *,
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

        self.trace.record(
            "tool_call",
            {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "validated_args": validated_args,
                "permission_level": (
                    spec.permission_level.value if spec is not None else None
                ),
                "risk_tags": list(spec.risk_tags) if spec is not None else [],
                "start": started_at,
                "end": ended_at,
                "duration_seconds": duration_seconds,
                "status": "ok" if result.ok else "error",
                "error_code": result.error_code,
                "truncated": result.truncated,
                "output_digest": output_digest,
                "cache_hit": cache_hit,
            },
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
            runtime="tool",
            summary=summary,
            payload=payload or {},
            ids=resolved_ids,
            severity=severity,
        )

    @staticmethod
    def _parse_arguments(raw_arguments: Any) -> Any:
        if isinstance(raw_arguments, dict):
            return raw_arguments
        return json.loads(raw_arguments)

    @staticmethod
    def _should_cache(spec: ToolSpec) -> bool:
        return spec.cacheable and spec.permission_level == PermissionLevel.READ_ONLY

    def _cache_key(self, spec: ToolSpec, validated_args: dict[str, Any]) -> str:
        payload = {
            "tool_name": spec.name,
            "version": spec.version,
            "arguments": validated_args,
            "workspace_root": str(self.workspace_root),
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

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
        text = json.dumps(arguments, ensure_ascii=False, sort_keys=True, default=str)
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
        dumped = result.model_dump(mode="json")
        return self._digest(json.dumps(dumped, ensure_ascii=False, sort_keys=True))


def _tool_policy_shape(
    tool_name: str,
    spec: ToolSpec,
    args: dict[str, Any],
) -> tuple[OperationKind, Capability, ResourceRef]:
    if tool_name == "read_file":
        return (
            OperationKind.READ_FILE,
            Capability.READ_WORKSPACE,
            ResourceRef("file", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name == "list_files":
        return (
            OperationKind.LIST_DIRECTORY,
            Capability.LIST_DIRECTORY,
            ResourceRef("directory", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name == "search_text":
        return (
            OperationKind.SEARCH,
            Capability.READ_WORKSPACE,
            ResourceRef("directory", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name == "workspace_create_file":
        return (
            OperationKind.CREATE_FILE,
            Capability.CREATE_FILE,
            ResourceRef("file", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name == "workspace_delete_file":
        return (
            OperationKind.DELETE_FILE,
            Capability.DELETE_FILE,
            ResourceRef("file", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name == "workspace_move_file":
        return (
            OperationKind.MUTATE_FILE,
            Capability.MOVE_FILE,
            ResourceRef("file", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name.startswith("workspace_"):
        return (
            OperationKind.MUTATE_FILE,
            Capability.MUTATE_WORKSPACE,
            ResourceRef("file", str(args.get("path") or "."), workspace_relative=True),
        )
    if tool_name == "start_process":
        return (
            OperationKind.START_LONG_PROCESS,
            Capability.START_LONG_PROCESS,
            ResourceRef("command", _command_identifier(args)),
        )
    if tool_name == "stop_process":
        return (
            OperationKind.KILL_PROCESS,
            Capability.KILL_PROCESS,
            ResourceRef("process", str(args.get("process_id") or "")),
        )
    if tool_name == "run_command":
        return (
            OperationKind.EXECUTE_COMMAND,
            Capability.EXECUTE_COMMAND,
            ResourceRef("command", _command_identifier(args)),
        )
    if tool_name in {"plan_verification", "get_verification_result"}:
        return (
            OperationKind.READ_FILE,
            Capability.READ_WORKSPACE,
            ResourceRef("workspace", tool_name, workspace_relative=True),
        )
    if tool_name in {"run_verification", "rerun_check"}:
        return (
            OperationKind.VERIFICATION,
            Capability.EXECUTE_PROJECT_CODE,
            ResourceRef("workspace", tool_name, workspace_relative=True),
        )
    if spec.permission_level == PermissionLevel.WRITE:
        return (
            OperationKind.MUTATE_FILE,
            Capability.MUTATE_WORKSPACE,
            ResourceRef("workspace", tool_name, workspace_relative=True),
        )
    if spec.permission_level == PermissionLevel.SHELL:
        return (
            OperationKind.EXECUTE_COMMAND,
            Capability.EXECUTE_COMMAND,
            ResourceRef("command", _command_identifier(args) or tool_name),
        )
    return (
        OperationKind.READ_FILE,
        Capability.READ_WORKSPACE,
        ResourceRef("workspace", tool_name, workspace_relative=True),
    )


def _command_identifier(args: dict[str, Any]) -> str:
    if args.get("shell"):
        return str(args["shell"])
    if args.get("argv"):
        return " ".join(str(part) for part in args["argv"])
    return ""


def _policy_error_code(outcome: DecisionOutcome) -> str:
    mapping = {
        DecisionOutcome.DENY: "policy_denied",
        DecisionOutcome.REQUIRE_REVIEW: "approval_required",
        DecisionOutcome.SANDBOX_REQUIRED: "sandbox_required",
        DecisionOutcome.ASK_USER: "policy_ask_user_required",
        DecisionOutcome.ESCALATE: "policy_escalation_required",
    }
    return mapping.get(outcome, "policy_denied")
