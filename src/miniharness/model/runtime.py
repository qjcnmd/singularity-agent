from __future__ import annotations

import json
import re
import time
from typing import Any
from uuid import uuid4

from miniharness.model.budget import ModelBudgetManager
from miniharness.model.config import ModelRuntimeConfig
from miniharness.model.errors import (
    ModelBudgetExceeded,
    ModelCapabilityError,
    ModelContextTooLong,
)
from miniharness.model.messages import MessageConverter
from miniharness.model.models import (
    ModelError,
    ModelErrorKind,
    ModelMessage,
    ModelPreferences,
    ModelPurpose,
    ModelToolCall,
    ModelToolSchema,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
    ModelUsage,
    ModelValidationResult,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from miniharness.model.providers import (
    ChatProviderModelProvider,
    MockModelProvider,
    ModelProvider,
    ProviderRequest,
    ProviderResponse,
)
from miniharness.model.registry import ModelProviderRegistry
from miniharness.model.retry import ModelRetryController, RetryPolicy
from miniharness.model.streaming import ProviderStreamEventType, StreamingAccumulator
from miniharness.model.tools import ModelToolRenderer, ToolCallNormalizer
from miniharness.model.validation import ModelResponseValidator
from miniharness.observability.models import (
    TraceArtifactKind,
    TraceEventType,
    TraceSeverity,
)
from miniharness.tools.registry import ToolRegistry


SECRET_PATTERNS = (
    re.compile(r"\b[A-Z0-9_]*(?:API_KEY|TOKEN|SECRET|PASSWORD)\s*=", re.IGNORECASE),
    re.compile(r"\bsk-[A-Za-z0-9_\-]{8,}\b"),
)


class ModelRuntime:
    def __init__(
        self,
        *,
        registry: ModelProviderRegistry,
        tool_registry: ToolRegistry,
        config: ModelRuntimeConfig | None = None,
        trace: Any | None = None,
    ) -> None:
        self.registry = registry
        self.tool_registry = tool_registry
        self.config = config or ModelRuntimeConfig()
        self.trace = trace
        self.converter = MessageConverter()
        self.tool_renderer = ModelToolRenderer(tool_registry)
        self.tool_normalizer = ToolCallNormalizer(tool_registry)
        self.validator = ModelResponseValidator(tool_registry)
        self.budget_manager = ModelBudgetManager()
        self.turn_count = 0

    @classmethod
    def with_mock_provider(
        cls,
        provider: MockModelProvider,
        *,
        tool_registry: ToolRegistry,
        config: ModelRuntimeConfig | None = None,
        trace: Any | None = None,
    ) -> "ModelRuntime":
        registry = ModelProviderRegistry(default_provider_name=provider.name())
        registry.register(provider)
        return cls(
            registry=registry,
            tool_registry=tool_registry,
            config=config,
            trace=trace,
        )

    @classmethod
    def from_chat_provider(
        cls,
        provider: Any,
        *,
        tool_registry: ToolRegistry,
        config: ModelRuntimeConfig | None = None,
        trace: Any | None = None,
        provider_name: str = "legacy_chat",
    ) -> "ModelRuntime":
        adapter = ChatProviderModelProvider(provider, provider_name=provider_name)
        registry = ModelProviderRegistry(default_provider_name=adapter.name())
        registry.register(adapter)
        return cls(
            registry=registry,
            tool_registry=tool_registry,
            config=config,
            trace=trace,
        )

    def build_request_from_context(
        self,
        context: Any,
        *,
        run_id: str,
        session_id: str,
        task_id: str,
        phase_id: str,
        action_id: str,
        purpose: ModelPurpose,
        allowed_tool_names: list[str] | None = None,
        planner_context: dict[str, Any] | None = None,
        tool_choice: ToolChoicePolicy | None = None,
        instruction_runtime: Any | None = None,
        user_task: str | None = None,
        user_session_instructions: list[str] | None = None,
        runtime_observations: list[dict[str, Any]] | None = None,
        retrieved_content: list[dict[str, Any]] | None = None,
        supports_developer_message: bool | None = None,
        strict_tools: bool = False,
    ) -> ModelTurnRequest:
        tools = self.tool_renderer.render(
            allowed_tool_names=allowed_tool_names,
            strict=strict_tools,
        )
        provider_tools = self.tool_renderer.to_provider_tools(tools, strict=strict_tools)
        prompt_bundle = None
        if instruction_runtime is not None:
            selected_provider = self.registry.select_provider(
                ModelPreferences(),
                purpose=purpose,
            )
            provider_supports_developer = selected_provider.capabilities().supports_developer_message
            if supports_developer_message is not None:
                provider_supports_developer = supports_developer_message
            observations = list(runtime_observations or [])
            if planner_context is not None:
                observations.append(
                    {
                        "source_type": "runtime_observation",
                        "origin": "planner_context",
                        "content": planner_context.get("content") if isinstance(planner_context, dict) else planner_context,
                    }
                )
            if hasattr(context, "instruction_sources"):
                observations.extend(context.instruction_sources())
            prompt_bundle = instruction_runtime.build_for_model_turn(
                user_task=user_task or getattr(context, "user_goal", ""),
                purpose=purpose,
                user_session_instructions=user_session_instructions,
                runtime_observations=observations,
                retrieved_content=retrieved_content,
                tool_protocol_summary=self._tool_protocol_summary(tools),
                supports_developer_message=provider_supports_developer,
                ids={
                    "run_id": run_id,
                    "session_id": session_id,
                    "task_id": task_id,
                    "phase_id": phase_id,
                    "action_id": action_id,
                },
            )
            history_messages = context.messages(
                tools=provider_tools,
                planner_context=None,
                persist=True,
            )[2:]
            messages = [*prompt_bundle.messages, *history_messages]
            for message in messages:
                if isinstance(message, ModelMessage):
                    message.metadata.setdefault("prompt_manifest_id", prompt_bundle.manifest.manifest_id)
                    message.metadata.setdefault("prompt_hash", prompt_bundle.prompt_hash)
        else:
            messages = context.messages(
                tools=provider_tools,
                planner_context=planner_context,
                persist=True,
            )
        prompt_metadata = (
            {
                "prompt_manifest_id": prompt_bundle.manifest.manifest_id,
                "prompt_hash": prompt_bundle.prompt_hash,
                "token_estimate": prompt_bundle.token_estimate,
            }
            if prompt_bundle is not None
            else {}
        )
        return ModelTurnRequest(
            request_id=f"model_req_{uuid4().hex[:12]}",
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            action_id=action_id,
            purpose=purpose,
            messages=[self._coerce_message(message) for message in messages],
            tools=tools,
            tool_choice=tool_choice
            or ToolChoicePolicy(
                mode=(
                    ToolChoiceMode.AUTO
                    if allowed_tool_names is None
                    else ToolChoiceMode.ALLOWED_TOOLS
                ),
                allowed_tool_names=(
                    [tool.name for tool in tools]
                    if allowed_tool_names is None
                    else allowed_tool_names
                ),
            ),
            context_metadata={
                "context_budget": (
                    context.last_budget.__dict__.copy()
                    if getattr(context, "last_budget", None) is not None
                    else {}
                ),
                **prompt_metadata,
            },
            trace_metadata=prompt_metadata,
        )

    def run_turn(self, request: ModelTurnRequest) -> ModelTurnResult:
        _throw_if_cancelled(self)
        self.turn_count += 1
        started = time.perf_counter()
        request = self._normalize_request(request)
        event_ids: list[str] = []
        provider: ModelProvider | None = None
        try:
            export_error = self._context_export_error(request)
            if export_error:
                error = ModelError(
                    kind=ModelErrorKind.INVALID_REQUEST,
                    message=export_error,
                    retryable=False,
                )
                event_ids.extend(self._emit_request_failed(request, error))
                validation = self.validator.validate(
                    assistant_message=ModelMessage.assistant_text(""),
                    tool_calls=[],
                    tool_choice=request.tool_choice,
                    allowed_tool_names=self._allowed_tool_names(request),
                )
                validation.errors.append(export_error)
                return self._invalid_result(
                    request,
                    validation_errors=validation.errors,
                    event_ids=event_ids,
                    error=error,
                )

            provider = self.registry.select_provider(
                request.model_preferences,
                purpose=request.purpose,
            )
            self.registry.check_capabilities(
                provider,
                requires_tools=bool(request.tools),
                requires_streaming=request.model_preferences.stream,
                requires_json_mode=request.model_preferences.json_mode,
            )
            estimated_usage = self.budget_manager.check_budget(
                messages=request.messages,  # type: ignore[arg-type]
                tools=request.tools,
                budget=request.budget,
            )
            self.budget_manager.check_context_window(
                estimated_usage,
                max_context_tokens=provider.capabilities().max_context_tokens,
            )
            event_ids.extend(self._emit_request_created(request, estimated_usage))

            provider_response = self._send_with_retry(provider, request)
            tool_calls = self._normalize_tool_calls(provider_response, request)
            validation = self.validator.validate(
                assistant_message=provider_response.message,
                tool_calls=tool_calls,
                tool_choice=request.tool_choice,
                allowed_tool_names=self._allowed_tool_names(request),
                capabilities=provider.capabilities(),
            )
            if not validation.valid:
                event_ids.extend(
                    self._emit_output_rejected(request, provider_response, validation.errors)
                )
                return ModelTurnResult(
                    request_id=request.request_id,
                    response_id=provider_response.response_id,
                    status=ModelTurnStatus.INVALID,
                    assistant_message=provider_response.message,
                    tool_calls=tool_calls,
                    usage=provider_response.usage,
                    finish_reason=provider_response.finish_reason,
                    validation=validation,
                    provider_name=provider.name(),
                    model_name=provider_response.model_name,
                    latency_ms=_latency_ms(started),
                    trace_event_ids=event_ids,
                    raw_response_ref=self._write_raw_artifact(request, provider_response),
                )
            raw_ref = self._write_raw_artifact(request, provider_response)
            event_ids.extend(self._emit_response_received(request, provider_response, raw_ref))
            for tool_call in tool_calls:
                event_ids.extend(self._emit_tool_call(request, tool_call, provider))
            return ModelTurnResult(
                request_id=request.request_id,
                response_id=provider_response.response_id,
                status=ModelTurnStatus.SUCCESS,
                assistant_message=provider_response.message,
                tool_calls=tool_calls,
                usage=provider_response.usage,
                finish_reason=provider_response.finish_reason,
                validation=validation,
                provider_name=provider.name(),
                model_name=provider_response.model_name,
                latency_ms=_latency_ms(started),
                trace_event_ids=event_ids,
                raw_response_ref=raw_ref,
            )
        except (ModelBudgetExceeded, ModelContextTooLong) as exc:
            kind = (
                ModelErrorKind.CONTEXT_LENGTH_EXCEEDED
                if isinstance(exc, ModelContextTooLong)
                else ModelErrorKind.BUDGET_EXCEEDED
            )
            status = (
                ModelTurnStatus.FAILED
                if isinstance(exc, ModelContextTooLong)
                else ModelTurnStatus.BUDGET_EXCEEDED
            )
            error = ModelError(kind=kind, message=str(exc), retryable=False)
            event_ids.extend(self._emit_request_failed(request, error))
            return self._failed_result(request, status=status, error=error, event_ids=event_ids, started=started)
        except (ModelError, ModelCapabilityError) as exc:
            error = exc if isinstance(exc, ModelError) else ModelError(
                kind=ModelErrorKind.UNSUPPORTED_CAPABILITY,
                message=str(exc),
                retryable=False,
                provider_name=provider.name() if provider else None,
            )
            event_ids.extend(self._emit_request_failed(request, error))
            return self._failed_result(request, status=ModelTurnStatus.FAILED, error=error, event_ids=event_ids, started=started)
        except Exception as exc:
            if _is_cancellation_error(exc):
                raise
            error = ModelError(
                kind=ModelErrorKind.UNKNOWN_PROVIDER_ERROR,
                message=str(exc),
                retryable=False,
                provider_name=provider.name() if provider else None,
                metadata={"type": type(exc).__name__},
            )
            event_ids.extend(self._emit_request_failed(request, error))
            return self._failed_result(request, status=ModelTurnStatus.FAILED, error=error, event_ids=event_ids, started=started)

    def _send_with_retry(
        self,
        provider: ModelProvider,
        request: ModelTurnRequest,
    ) -> ProviderResponse:
        _throw_if_cancelled(self)
        policy = RetryPolicy(
            max_attempts=max(1, request.budget.max_retries + 1),
            backoff_seconds=float(self.config.retry_policy.get("backoff_seconds", 0.25)),
            fallback_models=list(
                request.model_preferences.fallback_models
                or self.config.retry_policy.get("fallback_models", [])
            ),
        )
        controller = ModelRetryController(policy)

        def operation(model_name: str | None) -> ProviderResponse:
            _throw_if_cancelled(self)
            preferences = request.model_preferences
            if model_name is not None:
                preferences = preferences.__class__.from_dict(
                    {**preferences.to_dict(), "model_name": model_name}
                )
            provider_request = ProviderRequest(
                request_id=request.request_id,
                purpose=request.purpose.value,
                messages=request.messages,  # type: ignore[arg-type]
                tools=request.tools,
                tool_choice=request.tool_choice,
                preferences=preferences,
                policy_metadata=request.policy_metadata,
                trace_metadata=request.trace_metadata,
            )
            if preferences.stream:
                return self._stream_provider_response(provider, provider_request)
            return provider.complete(provider_request)

        return controller.run(
            operation,
            initial_model=request.model_preferences.model_name,
        )

    def _stream_provider_response(
        self,
        provider: ModelProvider,
        provider_request: ProviderRequest,
    ) -> ProviderResponse:
        accumulator = StreamingAccumulator()
        finish_reason = "stop"
        usage = ModelUsage()
        for event in provider.stream(provider_request):
            _throw_if_cancelled(self)
            if event.type == ProviderStreamEventType.ERROR:
                if isinstance(event.error, ModelError):
                    raise event.error
                raise ModelError(
                    kind=ModelErrorKind.UNKNOWN_PROVIDER_ERROR,
                    message=str(event.error or "Streaming provider error."),
                    retryable=False,
                    provider_name=provider.name(),
                    model_name=provider_request.preferences.model_name,
                )
            if event.type == ProviderStreamEventType.USAGE_DELTA and event.usage_delta:
                usage = self.budget_manager.merge_usage(
                    usage,
                    ModelUsage(
                        input_tokens=int(event.usage_delta.get("input_tokens") or 0),
                        output_tokens=int(event.usage_delta.get("output_tokens") or 0),
                        total_tokens=int(event.usage_delta.get("total_tokens") or 0),
                        cached_input_tokens=int(event.usage_delta.get("cached_input_tokens") or 0),
                        reasoning_tokens=int(event.usage_delta.get("reasoning_tokens") or 0),
                    ),
                )
            elif event.type == ProviderStreamEventType.RESPONSE_COMPLETED:
                finish_reason = str(event.metadata.get("finish_reason") or "stop")
            accumulator.add(event)
        streaming_response = accumulator.to_response()
        return ProviderResponse(
            response_id=f"stream_resp_{uuid4().hex[:12]}",
            message=ModelMessage.assistant_text(streaming_response.message.content),
            tool_calls=streaming_response.tool_calls,
            usage=usage,
            finish_reason=(
                "tool_calls" if streaming_response.tool_calls else finish_reason
            ),
            provider_name=provider.name(),
            model_name=provider_request.preferences.model_name,
            raw_response=None,
        )

    def _normalize_request(self, request: ModelTurnRequest) -> ModelTurnRequest:
        request.messages = [self._coerce_message(message) for message in request.messages]
        if self.config.default_model and not request.model_preferences.model_name:
            request.model_preferences.model_name = self.config.default_model
        if (
            self.config.default_temperature is not None
            and request.model_preferences.temperature is None
        ):
            request.model_preferences.temperature = self.config.default_temperature
        if request.model_preferences.max_output_tokens is None:
            request.model_preferences.max_output_tokens = self.config.default_max_output_tokens
        return request

    def _coerce_message(self, message: ModelMessage | dict[str, Any]) -> ModelMessage:
        if isinstance(message, ModelMessage):
            return message
        return self.converter.from_openai_dict(message)

    def _normalize_tool_calls(
        self,
        response: ProviderResponse,
        request: ModelTurnRequest,
    ) -> list[ModelToolCall]:
        normalized: list[ModelToolCall] = []
        seen: set[str] = set()
        allowed = self._allowed_tool_names(request)
        for call in response.tool_calls:
            raw = call.provider_metadata.get("raw_tool_call")
            if isinstance(raw, dict):
                normalized.append(
                    self.tool_normalizer.normalize(
                        raw,
                        allowed_tool_names=allowed,
                        seen_ids=seen,
                    )
                )
            else:
                normalized.append(
                    self.tool_normalizer.normalize(
                        call.to_provider_tool_call(),
                        allowed_tool_names=allowed,
                        seen_ids=seen,
                    )
                )
        return normalized

    def _allowed_tool_names(self, request: ModelTurnRequest) -> list[str]:
        if request.tool_choice.mode == ToolChoiceMode.ALLOWED_TOOLS:
            return list(request.tool_choice.allowed_tool_names)
        if request.tool_choice.allowed_tool_names:
            return list(request.tool_choice.allowed_tool_names)
        if request.tools:
            return [tool.name for tool in request.tools]
        return [spec.name for spec in self.tool_registry.list()]

    @staticmethod
    def _tool_protocol_summary(tools: list[ModelToolSchema]) -> str:
        names = ", ".join(tool.name for tool in tools) if tools else "none"
        return "\n".join(
            [
                "Tool protocol summary:",
                "Only registered tools exposed in this request may be called.",
                "Tool calls must use complete JSON arguments.",
                "The model must not claim tool execution unless ToolRuntime returns a result.",
                f"Exposed tools: {names}.",
            ]
        )

    def _context_export_error(self, request: ModelTurnRequest) -> str | None:
        if not self.config.allow_remote_provider:
            return None
        policy = self.config.context_export_policy
        text = "\n".join(message.text for message in request.messages)  # type: ignore[union-attr]
        if policy.deny_secret_like_content and any(
            pattern.search(text) for pattern in SECRET_PATTERNS
        ):
            return "context_export_policy_secret_like_content"
        if policy.deny_env_content and ".env" in text.lower():
            return "context_export_policy_env_content"
        return None

    def _invalid_result(
        self,
        request: ModelTurnRequest,
        *,
        validation_errors: list[str],
        event_ids: list[str],
        error: ModelError | None = None,
    ) -> ModelTurnResult:
        return ModelTurnResult(
            request_id=request.request_id,
            response_id=f"model_resp_{uuid4().hex[:12]}",
            status=ModelTurnStatus.INVALID,
            assistant_message=None,
            usage=ModelUsage(),
            validation=ModelValidationResult(valid=False, errors=validation_errors),
            error=error,
            trace_event_ids=event_ids,
            metadata={"validation_errors": validation_errors},
        )

    def _failed_result(
        self,
        request: ModelTurnRequest,
        *,
        status: ModelTurnStatus,
        error: ModelError,
        event_ids: list[str],
        started: float,
    ) -> ModelTurnResult:
        return ModelTurnResult(
            request_id=request.request_id,
            response_id=f"model_resp_{uuid4().hex[:12]}",
            status=status,
            usage=ModelUsage(),
            error=error,
            latency_ms=_latency_ms(started),
            trace_event_ids=event_ids,
        )

    def _emit_request_created(
        self,
        request: ModelTurnRequest,
        estimated_usage: ModelUsage,
    ) -> list[str]:
        payload = {
            "request_id": request.request_id,
            "purpose": request.purpose.value,
            "message_count": len(request.messages),
            "tool_count": len(request.tools),
            "schema_hash": self.tool_renderer.schema_hash(request.tools),
            "estimated_usage": estimated_usage.to_dict(),
            "tool_choice": request.tool_choice.to_dict(),
        }
        return self._emit(
            TraceEventType.MODEL_REQUEST_CREATED,
            summary=f"Model request created for {request.purpose.value}.",
            payload=payload,
            ids=self._trace_ids(request),
        )

    def _emit_response_received(
        self,
        request: ModelTurnRequest,
        response: ProviderResponse,
        raw_ref: str | None,
    ) -> list[str]:
        payload = {
            "request_id": request.request_id,
            "response_id": response.response_id,
            "provider_name": response.provider_name,
            "model_name": response.model_name,
            "finish_reason": response.finish_reason,
            "tool_call_count": len(response.tool_calls),
            "content_hash": _hash_text(response.message.text),
            "usage": response.usage.to_dict(),
            "raw_response_ref": raw_ref,
        }
        return self._emit(
            TraceEventType.MODEL_RESPONSE_RECEIVED,
            summary="Model response received.",
            payload=payload,
            ids=self._trace_ids(request),
        )

    def _emit_tool_call(
        self,
        request: ModelTurnRequest,
        tool_call: ModelToolCall,
        provider: ModelProvider,
    ) -> list[str]:
        return self._emit(
            TraceEventType.MODEL_TOOL_CALL_PROPOSED,
            summary=f"Model proposed tool call {tool_call.tool_name}.",
            payload={
                "request_id": request.request_id,
                "tool_call_id": tool_call.tool_call_id,
                "function": tool_call.tool_name,
                "arguments_hash": _hash_text(tool_call.raw_arguments),
                "provider_name": provider.name(),
            },
            ids={**self._trace_ids(request), "action_id": tool_call.tool_call_id},
        )

    def _emit_output_rejected(
        self,
        request: ModelTurnRequest,
        response: ProviderResponse,
        errors: list[str],
    ) -> list[str]:
        return self._emit(
            TraceEventType.MODEL_OUTPUT_REJECTED,
            summary="Model output rejected.",
            payload={
                "request_id": request.request_id,
                "response_id": response.response_id,
                "errors": errors,
                "finish_reason": response.finish_reason,
            },
            ids=self._trace_ids(request),
            severity=TraceSeverity.WARNING,
        )

    def _emit_request_failed(
        self,
        request: ModelTurnRequest,
        error: ModelError,
    ) -> list[str]:
        return self._emit(
            TraceEventType.MODEL_REQUEST_FAILED,
            summary=error.message,
            payload={
                "request_id": request.request_id,
                "error": error.to_dict(),
            },
            ids=self._trace_ids(request),
            severity=TraceSeverity.ERROR,
        )

    def _emit(
        self,
        event_type: TraceEventType,
        *,
        summary: str,
        payload: dict[str, Any],
        ids: dict[str, Any],
        severity: TraceSeverity = TraceSeverity.INFO,
    ) -> list[str]:
        if self.trace is None:
            return []
        if hasattr(self.trace, "emit"):
            event = self.trace.emit(
                event_type,
                runtime="model",
                summary=summary,
                payload=payload,
                ids=ids,
                severity=severity,
            )
            event_id = getattr(event, "event_id", None)
            return [event_id] if event_id else []
        if hasattr(self.trace, "record"):
            self.trace.record(
                event_type.value,
                {
                    **payload,
                    "task_id": ids.get("task_id"),
                    "session_id": ids.get("session_id"),
                    "phase_id": ids.get("phase_id"),
                    "action_id": ids.get("action_id"),
                },
            )
        return []

    def _write_raw_artifact(
        self,
        request: ModelTurnRequest,
        response: ProviderResponse,
    ) -> str | None:
        if not self.config.store_raw_responses or self.trace is None:
            return None
        if not hasattr(self.trace, "write_artifact"):
            return None
        artifact = self.trace.write_artifact(
            kind=TraceArtifactKind.MODEL_MESSAGE,
            text=json.dumps(
                {
                    "request_id": request.request_id,
                    "response_id": response.response_id,
                    "message": response.message.to_dict(),
                    "tool_calls": [tool_call.to_dict() for tool_call in response.tool_calls],
                    "usage": response.usage.to_dict(),
                    "finish_reason": response.finish_reason,
                },
                ensure_ascii=False,
                sort_keys=True,
                default=str,
            ),
            task_id=request.task_id,
            summary="Redacted model response.",
            sensitive=True,
            content_type="application/json",
        )
        return artifact.artifact_id

    @staticmethod
    def _trace_ids(request: ModelTurnRequest) -> dict[str, Any]:
        return {
            "run_id": request.run_id,
            "session_id": request.session_id,
            "task_id": request.task_id,
            "phase_id": request.phase_id,
            "action_id": request.action_id,
        }


def _latency_ms(started: float) -> int:
    return max(0, int((time.perf_counter() - started) * 1000))


def _hash_text(text: str) -> str:
    import hashlib

    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _throw_if_cancelled(runtime: Any) -> None:
    token = getattr(runtime, "cancellation_token", None)
    if token is not None and hasattr(token, "throw_if_cancelled"):
        token.throw_if_cancelled()


def _is_cancellation_error(exc: BaseException) -> bool:
    return type(exc).__name__ == "CancellationError" and getattr(exc, "code", None) == "cancelled"
