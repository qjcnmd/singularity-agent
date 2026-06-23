from __future__ import annotations

import hashlib
import json
from typing import Any
from uuid import uuid4

from singularity.model.messages import MessageConverter
from singularity.model.models import (
    ModelMessage,
    ModelPreferences,
    ModelPurpose,
    ModelToolSchema,
    ModelTurnRequest,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.model.registry import ModelProviderRegistry
from singularity.model.tools import ModelToolRenderer


class ModelInputRenderer:
    def __init__(
        self,
        *,
        registry: ModelProviderRegistry,
        tool_renderer: ModelToolRenderer,
    ) -> None:
        self.registry = registry
        self.tool_renderer = tool_renderer
        self.converter = MessageConverter()

    def build_request(
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
        stable_messages: list[ModelMessage | dict[str, Any]]
        dynamic_messages: list[ModelMessage | dict[str, Any]] = []

        if instruction_runtime is not None:
            selected_provider = self.registry.select_provider(
                ModelPreferences(),
                purpose=purpose,
            )
            provider_supports_developer = selected_provider.capabilities().supports_developer_message
            if supports_developer_message is not None:
                provider_supports_developer = supports_developer_message
            prompt_bundle = instruction_runtime.build_for_model_turn(
                user_task=user_task or getattr(context, "user_goal", ""),
                purpose=purpose,
                user_session_instructions=user_session_instructions,
                runtime_observations=list(runtime_observations or []),
                retrieved_content=retrieved_content,
                tool_protocol_summary=_tool_protocol_summary(tools),
                supports_developer_message=provider_supports_developer,
                ids={
                    "run_id": run_id,
                    "session_id": session_id,
                    "task_id": task_id,
                    "phase_id": phase_id,
                    "action_id": action_id,
                },
            )
            stable_messages = list(prompt_bundle.messages)
            dynamic_messages = context.messages(
                tools=provider_tools,
                planner_context=planner_context,
                persist=True,
            )[2:]
            messages = [*stable_messages, *dynamic_messages]
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
            stable_messages = messages[:2]
            dynamic_messages = messages[2:]

        tool_schema_hash = self.tool_renderer.schema_hash(tools)
        prompt_metadata = (
            {
                "prompt_manifest_id": prompt_bundle.manifest.manifest_id,
                "prompt_hash": prompt_bundle.prompt_hash,
                "token_estimate": prompt_bundle.token_estimate,
            }
            if prompt_bundle is not None
            else {}
        )
        render_metadata = {
            "input_renderer": "model_input_renderer/v1",
            "stable_prefix_message_count": len(stable_messages),
            "dynamic_tail_message_count": len(dynamic_messages),
            "stable_prefix_hash": _messages_hash(stable_messages),
            "dynamic_tail_hash": _messages_hash(dynamic_messages),
            "tool_schema_hash": tool_schema_hash,
            "tool_names": [tool.name for tool in tools],
        }
        rendered_tool_names = [tool.name for tool in tools]
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
                allowed_tool_names=rendered_tool_names,
            ),
            context_metadata={
                "context_budget": (
                    context.last_budget.__dict__.copy()
                    if getattr(context, "last_budget", None) is not None
                    else {}
                ),
                **prompt_metadata,
                **render_metadata,
            },
            trace_metadata={**prompt_metadata, **render_metadata},
        )

    def _coerce_message(self, message: ModelMessage | dict[str, Any]) -> ModelMessage:
        if isinstance(message, ModelMessage):
            return message
        return self.converter.from_openai_dict(message)


def _tool_protocol_summary(tools: list[ModelToolSchema]) -> str:
    del tools
    return "\n".join(
        [
            "Tool protocol summary:",
            "Only tools exposed in this request's tool schema may be called.",
            "Tool calls must use complete JSON arguments.",
            "The model must not claim tool execution unless ToolRuntime returns a result.",
        ]
    )


def _messages_hash(messages: list[ModelMessage | dict[str, Any]]) -> str:
    payload = [_hashable_message(message) for message in messages]
    text = json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _hashable_message(message: ModelMessage | dict[str, Any]) -> dict[str, Any]:
    if isinstance(message, ModelMessage):
        return {
            "role": message.role.value,
            "content": [
                {
                    "type": block.type.value,
                    "text": block.text,
                    "artifact_ref": block.artifact_ref,
                }
                for block in message.content
            ],
            "name": message.name,
            "tool_call_id": message.tool_call_id,
        }
    return {
        "role": message.get("role"),
        "content": message.get("content"),
        "name": message.get("name"),
        "tool_call_id": message.get("tool_call_id"),
        "tool_calls": message.get("tool_calls"),
    }
