from __future__ import annotations

import hashlib
import json
import re
import time
from typing import Any
from uuid import uuid4

from singularity.kernel.cancellation import is_cancellation_error, throw_if_cancelled
from singularity.model.budget import ModelBudgetManager
from singularity.model.config import ModelRunnerConfig
from singularity.model.errors import (
    ModelBudgetExceeded,
    ModelCapabilityError,
    ModelContextTooLong,
)
from singularity.model.messages import MessageConverter
from singularity.model.models import (
    ContentBlock,
    ModelError,
    ModelErrorKind,
    ModelMessage,
    ModelPreferences,
    ModelPurpose,
    ModelRole,
    ModelToolCall,
    ModelToolParseStatus,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
    ModelUsage,
    ModelValidationResult,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.model.providers import (
    ChatProviderModelProvider,
    MockModelProvider,
    ModelProvider,
    ProviderRequest,
    ProviderResponse,
)
from singularity.model.registry import ModelProviderRegistry
from singularity.model.request_builder import ModelTurnRequestBuilder
from singularity.model.retry import ModelRetryController, RetryPolicy
from singularity.model.streaming import ProviderStreamEventType, StreamingAccumulator
from singularity.model.tools import ModelToolRenderer, ToolCallNormalizer
from singularity.model.validation import ModelResponseValidator
from singularity.observability.models import (
    TraceArtifactKind,
    TraceEventType,
    TraceSeverity,
)
from singularity.tools.registry import ToolRegistry

SECRET_PATTERNS = (
    re.compile(r"\b[A-Z0-9_]*(?:API_KEY|TOKEN|SECRET|PASSWORD)\s*=", re.IGNORECASE),
    re.compile(r"\bsk-[A-Za-z0-9_\-]{8,}\b"),
)
ENV_ASSIGNMENT_PATTERN = re.compile(r"(?im)^\s*(?:export\s+)?[A-Z_][A-Z0-9_]{1,}\s*=")


class ModelRunner:
    def __init__(
        self,
        *,
        registry: ModelProviderRegistry,
        tool_registry: ToolRegistry,
        config: ModelRunnerConfig | None = None,
        trace: Any | None = None,
    ) -> None:
        self.registry = registry
        self.tool_registry = tool_registry
        self.config = config or ModelRunnerConfig()
        self.trace = trace
        self.converter = MessageConverter()
        self.tool_renderer = ModelToolRenderer(tool_registry)
        self.request_builder = ModelTurnRequestBuilder(
            registry=registry,
            tool_renderer=self.tool_renderer,
        )
        self.tool_normalizer = ToolCallNormalizer(tool_registry)
        self.validator = ModelResponseValidator(tool_registry)
        self.budget_manager = ModelBudgetManager()
        self.turn_count = 0
        self._last_cache_shape: dict[str, dict[str, Any]] = {}

    @classmethod
    def with_mock_provider(
        cls,
        provider: MockModelProvider,
        *,
        tool_registry: ToolRegistry,
        config: ModelRunnerConfig | None = None,
        trace: Any | None = None,
    ) -> ModelRunner:
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
        config: ModelRunnerConfig | None = None,
        trace: Any | None = None,
        provider_name: str = "legacy_chat",
    ) -> ModelRunner:
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
        prompt_assembly: Any | None = None,
        user_task: str | None = None,
        user_session_instructions: list[str] | None = None,
        component_observations: list[dict[str, Any]] | None = None,
        retrieved_content: list[dict[str, Any]] | None = None,
        supports_developer_message: bool | None = None,
        strict_tools: bool = False,
    ) -> ModelTurnRequest:
        return self.request_builder.build_request(
            context,
            run_id=run_id,
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            action_id=action_id,
            purpose=purpose,
            allowed_tool_names=allowed_tool_names,
            planner_context=planner_context,
            tool_choice=tool_choice,
            prompt_assembly=prompt_assembly,
            user_task=user_task,
            user_session_instructions=user_session_instructions,
            component_observations=component_observations,
            retrieved_content=retrieved_content,
            supports_developer_message=supports_developer_message,
            strict_tools=strict_tools,
        )

    def supports_review_output_mode(self, mode: str) -> bool:
        try:
            provider = self.registry.select_provider(ModelPreferences(), purpose=None)
        except Exception:
            return False
        capabilities = provider.capabilities()
        if mode == "structured_output":
            return bool(capabilities.supports_structured_outputs)
        if mode == "forced_tool_call":
            return bool(capabilities.supports_tools)
        if mode == "json_mode":
            return bool(capabilities.supports_json_mode)
        return mode == "rule_only"

    def run_turn(self, request: ModelTurnRequest) -> ModelTurnResult:
        throw_if_cancelled(self)
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
                    message=export_error["code"],
                    retryable=False,
                    metadata={"context_export_diagnostics": export_error["diagnostics"]},
                )
                event_ids.extend(self._emit_request_failed(request, error))
                validation = self.validator.validate(
                    assistant_message=ModelMessage.assistant_text(""),
                    tool_calls=[],
                    tool_choice=request.tool_choice,
                    allowed_tool_names=self._allowed_tool_names(request),
                )
                validation.errors.append(export_error["code"])
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
            request, capability_adjustments = self._apply_provider_capability_adjustments(
                request,
                provider,
            )
            estimated_usage = self.budget_manager.check_budget(
                messages=request.messages,
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
                latency_ms = _latency_ms(started)
                self.budget_manager.check_response_budget(
                    provider_response.usage,
                    budget=request.budget,
                    latency_ms=latency_ms,
                )
                cache_metadata = self._cache_metadata(request, provider_response.usage)
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
                    latency_ms=latency_ms,
                    trace_event_ids=event_ids,
                    raw_response_ref=self._write_raw_artifact(request, provider_response),
                    metadata={
                        "cache": cache_metadata,
                        "cache_miss_reasons": cache_metadata["cache_miss_reasons"],
                    },
                )
            raw_ref = self._write_raw_artifact(request, provider_response)
            cache_metadata = self._cache_metadata(request, provider_response.usage)
            event_ids.extend(
                self._emit_response_received(
                    request,
                    provider_response,
                    raw_ref,
                    cache_metadata=cache_metadata,
                )
            )
            for tool_call in tool_calls:
                event_ids.extend(self._emit_tool_call(request, tool_call, provider))
            latency_ms = _latency_ms(started)
            self.budget_manager.check_response_budget(
                provider_response.usage,
                budget=request.budget,
                latency_ms=latency_ms,
            )
            result = ModelTurnResult(
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
                latency_ms=latency_ms,
                trace_event_ids=event_ids,
                raw_response_ref=raw_ref,
                metadata={
                    "cache": cache_metadata,
                    "cache_miss_reasons": cache_metadata["cache_miss_reasons"],
                    "context_bundle_metadata": {
                        **dict(request.context_metadata.get("context_bundle_metadata") or {}),
                        "cache": cache_metadata,
                    },
                },
            )
            if capability_adjustments:
                result.metadata["capability_adjustments"] = capability_adjustments
            return result
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
            if is_cancellation_error(exc):
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
        throw_if_cancelled(self)
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
            throw_if_cancelled(self)
            preferences = request.model_preferences
            if model_name is not None:
                preferences = preferences.__class__.from_dict(
                    {**preferences.to_dict(), "model_name": model_name}
                )
            provider_request = ProviderRequest(
                request_id=request.request_id,
                purpose=request.purpose.value,
                messages=request.messages,
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

    def _apply_provider_capability_adjustments(
        self,
        request: ModelTurnRequest,
        provider: ModelProvider,
    ) -> tuple[ModelTurnRequest, dict[str, Any]]:
        capabilities = provider.capabilities()
        downgraded: list[str] = []
        blocked: list[str] = []
        if request.tools and not capabilities.supports_tools:
            blocked.append("tools")
        if blocked:
            raise ModelError(
                kind=ModelErrorKind.UNSUPPORTED_CAPABILITY,
                message=f"Provider {provider.name()} does not support required capabilities: {', '.join(blocked)}.",
                retryable=False,
                provider_name=provider.name(),
                model_name=request.model_preferences.model_name,
                metadata={
                    "capability": blocked[0],
                    "blocked": blocked,
                    "provider_capabilities": self.registry.provider_capability_summary(provider),
                },
            )

        preferences = request.model_preferences
        if preferences.json_mode and not capabilities.supports_json_mode:
            preferences = ModelPreferences.from_dict(
                {**preferences.to_dict(), "json_mode": False}
            )
            downgraded.append("json_mode")
        if preferences.structured_output_schema and not capabilities.supports_structured_outputs:
            preferences = ModelPreferences.from_dict(
                {**preferences.to_dict(), "structured_output_schema": None}
            )
            downgraded.append("structured_outputs")
        if preferences.stream and not capabilities.supports_streaming:
            preferences = ModelPreferences.from_dict(
                {**preferences.to_dict(), "stream": False}
            )
            downgraded.append("streaming")

        tool_choice = request.tool_choice
        if (
            tool_choice.max_tool_calls > 1
            and not capabilities.supports_parallel_tool_calls
        ):
            tool_choice = ToolChoicePolicy.from_dict(
                {**tool_choice.to_dict(), "max_tool_calls": 1}
            )
            downgraded.append("parallel_tool_calls")

        messages = [
            self._fold_developer_message(message, capabilities)
            for message in request.messages
        ]
        if any(
            isinstance(original, ModelMessage)
            and isinstance(adjusted, ModelMessage)
            and original.role == ModelRole.DEVELOPER
            and adjusted.role != ModelRole.DEVELOPER
            for original, adjusted in zip(request.messages, messages, strict=False)
        ):
            downgraded.append("developer_message")

        adjusted_request = ModelTurnRequest.from_dict(
            {
                **request.to_dict(),
                "messages": [
                    message.to_dict() if isinstance(message, ModelMessage) else message
                    for message in messages
                ],
                "model_preferences": preferences.to_dict(),
                "tool_choice": tool_choice.to_dict(),
            }
        )
        adjustment = {
            "provider": provider.name(),
            "downgraded": downgraded,
            "blocked": blocked,
            "provider_capabilities": self.registry.provider_capability_summary(provider),
        }
        if not downgraded and not blocked:
            return adjusted_request, {}
        adjusted_request.trace_metadata = {
            **adjusted_request.trace_metadata,
            "capability_adjustments": adjustment,
        }
        return adjusted_request, adjustment

    @staticmethod
    def _fold_developer_message(
        message: ModelMessage | dict[str, Any],
        capabilities: Any,
    ) -> ModelMessage | dict[str, Any]:
        if not isinstance(message, ModelMessage):
            return message
        if message.role != ModelRole.DEVELOPER or capabilities.supports_developer_message:
            return message
        role = ModelRole.SYSTEM if capabilities.supports_system_message else ModelRole.USER
        return ModelMessage(
            role=role,
            content=[
                ContentBlock.from_dict(block.to_dict())
                for block in message.content
            ],
            name=message.name,
            tool_call_id=message.tool_call_id,
            metadata={**message.metadata, "developer_fallback": role.value},
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
            throw_if_cancelled(self)
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
        request_tool_names = {tool.name for tool in request.tools}
        for call in response.tool_calls:
            if call.tool_name in request_tool_names:
                normalized.append(self._normalize_request_local_tool_call(call, seen_ids=seen))
                continue
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

    @staticmethod
    def _normalize_request_local_tool_call(
        call: ModelToolCall,
        *,
        seen_ids: set[str],
    ) -> ModelToolCall:
        errors = list(call.validation_errors)
        tool_call_id = call.tool_call_id or "<missing>"
        if not call.tool_call_id:
            errors.append("missing_tool_call_id")
        if tool_call_id in seen_ids:
            errors.append("duplicate_tool_call_id")
        seen_ids.add(tool_call_id)
        raw_arguments = call.raw_arguments or "{}"
        try:
            parsed = json.loads(raw_arguments)
        except json.JSONDecodeError as exc:
            return ModelToolCall(
                tool_call_id=tool_call_id,
                tool_name=call.tool_name,
                arguments={},
                raw_arguments=raw_arguments,
                parse_status=ModelToolParseStatus.INVALID_JSON,
                validation_errors=[*errors, str(exc)],
                provider_metadata=call.provider_metadata,
            )
        if not isinstance(parsed, dict):
            return ModelToolCall(
                tool_call_id=tool_call_id,
                tool_name=call.tool_name,
                arguments={},
                raw_arguments=raw_arguments,
                parse_status=ModelToolParseStatus.SCHEMA_MISMATCH,
                validation_errors=[*errors, "arguments_not_object"],
                provider_metadata=call.provider_metadata,
            )
        return ModelToolCall(
            tool_call_id=tool_call_id,
            tool_name=call.tool_name,
            arguments=parsed,
            raw_arguments=raw_arguments,
            parse_status=(
                ModelToolParseStatus.VALID
                if not errors and call.parse_status == ModelToolParseStatus.VALID
                else call.parse_status
            ),
            validation_errors=errors,
            provider_metadata=call.provider_metadata,
        )

    def _allowed_tool_names(self, request: ModelTurnRequest) -> list[str]:
        if request.tool_choice.mode == ToolChoiceMode.ALLOWED_TOOLS:
            return list(request.tool_choice.allowed_tool_names)
        if request.tool_choice.allowed_tool_names:
            return list(request.tool_choice.allowed_tool_names)
        if request.tools:
            return [tool.name for tool in request.tools]
        return [spec.name for spec in self.tool_registry.list_model_visible()]

    def _context_export_error(self, request: ModelTurnRequest) -> dict[str, Any] | None:
        if not self.config.allow_remote_provider:
            return None
        policy = self.config.context_export_policy
        for index, message in enumerate(request.messages):
            text = message.text
            if policy.deny_secret_like_content:
                for pattern in SECRET_PATTERNS:
                    if pattern.search(text):
                        return {
                            "code": "context_export_policy_secret_like_content",
                            "diagnostics": _export_diagnostics(
                                message,
                                message_index=index,
                                rule="secret_like_content",
                                pattern=pattern,
                            ),
                        }
            if policy.deny_env_content and ENV_ASSIGNMENT_PATTERN.search(text):
                return {
                    "code": "context_export_policy_env_content",
                    "diagnostics": _export_diagnostics(
                        message,
                        message_index=index,
                        rule="env_content",
                        pattern=ENV_ASSIGNMENT_PATTERN,
                    ),
                }
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
        *,
        cache_metadata: dict[str, Any],
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
            "cache": cache_metadata,
            "cache_miss_reasons": cache_metadata["cache_miss_reasons"],
            "raw_response_ref": raw_ref,
        }
        return self._emit(
            TraceEventType.MODEL_RESPONSE_RECEIVED,
            summary="Model response received.",
            payload=payload,
            ids=self._trace_ids(request),
        )

    def _cache_metadata(
        self,
        request: ModelTurnRequest,
        usage: ModelUsage,
    ) -> dict[str, Any]:
        input_tokens = int(usage.input_tokens or 0)
        cached_tokens = int(usage.cached_input_tokens or 0)
        response_cache = dict((request.trace_metadata or {}).get("cache") or {})
        shape = {
            "stable_prefix_hash": request.context_metadata.get("stable_prefix_hash"),
            "dynamic_tail_hash": request.context_metadata.get("dynamic_tail_hash"),
            "tool_schema_hash": request.context_metadata.get("tool_schema_hash"),
            "context_shape_hash": request.context_metadata.get("context_shape_hash"),
            "context_ordering_hash": request.context_metadata.get("context_ordering_hash"),
            "compression_snapshot_id": request.context_metadata.get("compression_snapshot_id"),
        }
        cache_key = f"{request.run_id}:{request.session_id}:{request.task_id}"
        previous = self._last_cache_shape.get(cache_key)
        reasons: list[str] = []
        source = "unknown"
        confidence = 0.0
        evidence: list[str] = []
        provider_name = response_cache.get("provider_name") or request.model_preferences.provider_name
        model_name = response_cache.get("model_name") or request.model_preferences.model_name
        if cached_tokens > 0:
            source = "provider_native"
            confidence = 1.0
            evidence.append("usage.cached_input_tokens")
        elif isinstance(response_cache.get("cache_attribution"), dict):
            attribution = dict(response_cache.get("cache_attribution") or {})
            source = str(attribution.get("source") or "component_inferred")
            confidence = float(attribution.get("confidence") or 0.0)
            reasons.extend(str(item) for item in attribution.get("reasons") or [])
            evidence.extend(str(item) for item in attribution.get("evidence") or [])
            provider_name = attribution.get("provider_name") or provider_name
            model_name = attribution.get("model_name") or model_name
        if cached_tokens == 0 and source == "unknown":
            source = "component_inferred"
            confidence = 0.35 if previous is not None else 0.2
            if previous is None:
                reasons.append("first_request")
            else:
                reasons.append("provider_cache_diagnostics_missing")
        if input_tokens > 0 and cached_tokens == 0:
            if previous is None:
                reasons.append("first_request")
            else:
                if previous.get("tool_schema_hash") != shape.get("tool_schema_hash"):
                    reasons.append("tool_schema_change")
                if previous.get("context_shape_hash") != shape.get("context_shape_hash") or previous.get("dynamic_tail_hash") != shape.get("dynamic_tail_hash"):
                    reasons.append("context_shape_change")
                if previous.get("compression_snapshot_id") != shape.get("compression_snapshot_id"):
                    reasons.append("compaction_change")
                if previous.get("context_ordering_hash") != shape.get("context_ordering_hash"):
                    reasons.append("ordering_change")
                if not reasons:
                    reasons.append("provider_cache_miss")
        self._last_cache_shape[cache_key] = shape
        reasons = list(dict.fromkeys(reasons))
        evidence = list(dict.fromkeys(evidence))
        return {
            "input_tokens": input_tokens,
            "cached_input_tokens": cached_tokens,
            "cache_hit_ratio": _cache_ratio(cached_tokens, input_tokens),
            "cache_miss_reasons": reasons,
            "cache_attribution": {
                "source": source,
                "confidence": confidence,
                "reasons": reasons,
                "evidence": evidence,
                "provider_name": provider_name,
                "model_name": model_name,
            },
            **{key: value for key, value in shape.items() if value is not None},
        }

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
                component="model",
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
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _export_diagnostics(
    message: ModelMessage,
    *,
    message_index: int,
    rule: str,
    pattern: re.Pattern[str],
) -> dict[str, Any]:
    metadata = dict(message.metadata or {})
    return {
        "rule": rule,
        "message_index": message_index,
        "role": message.role.value,
        "message_name": message.name,
        "section": metadata.get("section"),
        "source_type": metadata.get("source_type"),
        "prompt_manifest_id": metadata.get("prompt_manifest_id"),
        "pattern_hash": _hash_text(pattern.pattern)[:12],
        "text_hash": _hash_text(message.text)[:12],
    }


def _cache_ratio(cached_tokens: int, input_tokens: int) -> float:
    if input_tokens <= 0:
        return 0.0
    return round(cached_tokens / input_tokens, 4)
