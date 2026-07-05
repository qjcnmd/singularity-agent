from __future__ import annotations

import multiprocessing
import pickle
from concurrent.futures import ThreadPoolExecutor
from concurrent.futures import TimeoutError as FutureTimeout
from contextlib import suppress
from typing import Any

from pydantic import ValidationError

from singularity.observability.models import TraceEventType
from singularity.observability.redaction import TraceRedactor
from singularity.runtime.defaults import (
    DEFAULT_TOOL_CACHE_MAX_ENTRIES,
    DEFAULT_TOOL_EXECUTION_TIMEOUT_SECONDS,
    PROCESS_TERMINATION_GRACE_SECONDS,
)
from singularity.tools.execution_cache import ToolExecutionCache
from singularity.tools.execution_pipeline import (
    PLANNER_ACTION_ID_METADATA_KEY,
    PLANNER_UPDATE_DEFERRED_METADATA_KEY,
    ToolExecutionPipelineState,
)
from singularity.tools.execution_resources import touched_paths
from singularity.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutionFailure,
    ToolResult,
    ToolSpec,
)


class ToolExecutionDispatcher:
    def __init__(
        self,
        *,
        workspace_root: Any,
        redactor: TraceRedactor,
        cache: ToolExecutionCache,
        emit_trace: Any,
        request_trace_ids: Any,
        argument_summary: Any,
        result_digest: Any,
        limit_output: Any,
        throw_if_cancelled: Any,
        update_planner: Any,
    ) -> None:
        self.workspace_root = workspace_root
        self.redactor = redactor
        self.cache = cache
        self.emit_trace = emit_trace
        self.request_trace_ids = request_trace_ids
        self.argument_summary = argument_summary
        self.result_digest = result_digest
        self.limit_output = limit_output
        self.throw_if_cancelled = throw_if_cancelled
        self.update_planner = update_planner

    def preflight_delegated_handler(self, spec: ToolSpec, validated_args: Any) -> ToolResult | None:
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

    def dispatch(self, state: ToolExecutionPipelineState) -> ToolResult:
        assert state.spec is not None
        assert state.validated is not None
        assert state.validated_args is not None
        self.emit_trace(
            TraceEventType.TOOL_DISPATCH_STARTED,
            summary=f"Dispatching tool {state.tool_name}.",
            payload={
                "tool_name": state.tool_name,
                "tool_call_id": state.tool_call_id,
                "permission_level": state.spec.permission_level.value,
                "risk_tags": list(state.spec.risk_tags),
                "backend": state.spec.execution_backend.value,
                "batch_id": state.request.batch_id,
                "argument_digest": state.request.argument_digest,
                "arguments": self.argument_summary(state.validated_args),
            },
            ids=self.request_trace_ids(
                state.request,
                action_id=state.planner_action_id or state.tool_call_id,
            ),
        )
        self.throw_if_cancelled()
        result, state.output_digest = self.execute_handler(state.spec, state.validated)
        self.throw_if_cancelled()
        if state.approval_grant_id:
            result.metadata["approval_grant_id"] = state.approval_grant_id
        if state.policy_decision_id:
            result.metadata["policy_decision_id"] = state.policy_decision_id
        if state.planner_action_id:
            result.metadata[PLANNER_ACTION_ID_METADATA_KEY] = state.planner_action_id
        if state.defer_planner_update:
            result.metadata[PLANNER_UPDATE_DEFERRED_METADATA_KEY] = True
        else:
            self.update_planner(
                tool_call_id=state.tool_call_id,
                tool_name=state.tool_name,
                result=result,
                action_id=state.planner_action_id,
            )
            state.planner_updated = True
        if (
            self.cache.should_cache(state.spec)
            and result.ok
            and not self.cache.is_sensitive_result(state.spec, result)
        ):
            assert state.cache_key is not None
            self.cache.set(
                state.cache_key,
                result,
                max_entries=state.spec.cache_policy.max_entries
                if state.spec.cache_policy
                else DEFAULT_TOOL_CACHE_MAX_ENTRIES,
                touched_paths=self.cache.touched_paths(state.spec, state.validated_args),
            )
        if state.spec.permission_level != PermissionLevel.READ_ONLY:
            self.invalidate_after_write(state.spec, state.validated_args, result)
        state.remember_replay = True
        return result

    def execute_handler(self, spec: ToolSpec, validated_args: Any) -> tuple[ToolResult, str]:
        if _prefer_thread_handler(spec):
            return self.execute_handler_in_thread(spec, validated_args)
        if (
            spec.execution_backend == ToolExecutionBackendKind.IN_PROCESS
            and _handler_can_run_in_process(spec.handler, validated_args)
        ):
            return self.execute_handler_in_process(spec, validated_args)
        return self.execute_handler_in_thread(spec, validated_args)

    @staticmethod
    def _timeout_seconds(spec: ToolSpec) -> float:
        if spec.timeout_seconds is None:
            return DEFAULT_TOOL_EXECUTION_TIMEOUT_SECONDS
        return float(spec.timeout_seconds)

    def execute_handler_in_process(
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
                message=self.redactor.redact_text(f"Tool process failed to start: {exc}"),
                details={"type": type(exc).__name__},
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "process",
                },
            )
            return result, self.result_digest(result)
        child_conn.close()

        try:
            timeout_seconds = self._timeout_seconds(spec)
            process.join(timeout_seconds)
            if process.is_alive():
                process.terminate()
                process.join(PROCESS_TERMINATION_GRACE_SECONDS)
                killed = False
                if process.is_alive():
                    kill = getattr(process, "kill", None)
                    if kill is not None:
                        kill()
                        killed = True
                        process.join(PROCESS_TERMINATION_GRACE_SECONDS)
                still_alive = process.is_alive()
                result = ToolResult.failure(
                    code="timeout",
                    message=f"Tool timed out after {timeout_seconds} seconds.",
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
                return result, self.result_digest(result)

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
                return result, self.result_digest(result)

            payload = parent_conn.recv()
        finally:
            parent_conn.close()
            with suppress(ValueError):
                process.close()

        return self.process_payload_to_result(spec, payload)

    def execute_handler_in_thread(
        self, spec: ToolSpec, validated_args: Any
    ) -> tuple[ToolResult, str]:
        executor = ThreadPoolExecutor(max_workers=1)
        future = executor.submit(spec.handler, validated_args)
        shutdown_wait = True
        timeout_seconds = self._timeout_seconds(spec)
        try:
            output = future.result(timeout=timeout_seconds)
        except FutureTimeout:
            future.cancel()
            executor.shutdown(wait=False, cancel_futures=True)
            shutdown_wait = False
            result = ToolResult.failure(
                    code="timeout",
                    message=f"Tool timed out after {timeout_seconds} seconds.",
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "thread",
                    "timeout_type": "execution",
                    "timeout_untrusted_state": True,
                },
            )
            return result, self.result_digest(result)
        except ToolExecutionFailure as exc:
            result = ToolResult.failure(
                code=exc.code,
                message=self.redactor.redact_text(exc.message),
                details=self.redactor.redact_value(exc.details),
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "thread",
                },
            )
            return result, self.result_digest(result)
        except Exception as exc:
            result = ToolResult.failure(
                code="execution_error",
                message=self.redactor.redact_text(str(exc)),
                details={"type": type(exc).__name__},
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "thread",
                },
            )
            return result, self.result_digest(result)
        finally:
            if shutdown_wait:
                executor.shutdown(wait=True, cancel_futures=True)

        return self.handler_output_to_result(spec, output, handler_isolation="thread")

    def process_payload_to_result(
        self, spec: ToolSpec, payload: dict[str, Any]
    ) -> tuple[ToolResult, str]:
        status = payload.get("status")
        if status == "success":
            return self.handler_output_to_result(
                spec,
                payload.get("output"),
                handler_isolation="process",
            )
        if status == "tool_failure":
            result = ToolResult.failure(
                code=str(payload.get("code") or "execution_error"),
                message=self.redactor.redact_text(str(payload.get("message") or "")),
                details=self.redactor.redact_value(payload.get("details")),
                metadata={
                    "backend": spec.execution_backend.value,
                    "handler_isolation": "process",
                },
            )
            return result, self.result_digest(result)

        result = ToolResult.failure(
            code="execution_error",
            message=self.redactor.redact_text(str(payload.get("message") or "Tool failed.")),
            details={"type": payload.get("type") or "Exception"},
            metadata={
                "backend": spec.execution_backend.value,
                "handler_isolation": "process",
            },
        )
        return result, self.result_digest(result)

    def handler_output_to_result(
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
                    details=self.redactor.redact_value(exc.errors()),
                    metadata={
                        "backend": spec.execution_backend.value,
                        "handler_isolation": handler_isolation,
                    },
                )
                return result, self.result_digest(result)
        content, truncated, metadata, digest = self.limit_output(output, spec.max_output_chars)
        result = ToolResult.success(
            content=self.redactor.redact_value(content),
            truncated=truncated,
            metadata={
                **metadata,
                "backend": spec.execution_backend.value,
                "handler_isolation": handler_isolation,
            },
        )
        result.metadata["output_digest"] = digest
        return result, digest

    def invalidate_after_write(
        self,
        spec: ToolSpec,
        validated_args: dict[str, Any],
        result: ToolResult,
    ) -> None:
        affected: list[str] = []
        seen: set[str] = set()

        content = result.content if isinstance(result.content, dict) else {}
        for key in ("changed_files", "affected_files"):
            value = content.get(key)
            if isinstance(value, list):
                for item in value:
                    if isinstance(item, str) and item not in seen:
                        seen.add(item)
                        affected.append(item)

        for path in touched_paths(spec, validated_args, self.workspace_root):
            if path not in seen:
                seen.add(path)
                affected.append(path)

        if affected:
            self.cache.invalidate_paths(affected)
        else:
            self.cache.clear()


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
        with suppress(Exception):
            conn.send(
                {
                    "status": "exception",
                    "message": f"Tool process could not return result: {exc}",
                    "type": type(exc).__name__,
                }
            )


def _json_safe_process_value(value: Any) -> Any:
    import json

    return json.loads(json.dumps(value, ensure_ascii=False, default=str))


def _handler_can_run_in_process(handler: Any, validated_args: Any) -> bool:
    try:
        pickle.dumps((handler, validated_args))
    except Exception:
        return False
    return True


def _prefer_thread_handler(spec: ToolSpec) -> bool:
    return (
        spec.name in {"list_files", "read_file", "search_text"}
        and spec.permission_level == PermissionLevel.READ_ONLY
        and spec.side_effects.value == "read_workspace"
        and spec.execution_backend == ToolExecutionBackendKind.IN_PROCESS
    )
