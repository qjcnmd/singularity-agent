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


class ModelTurnRequestBuilder:
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
        prompt_assembly: Any | None = None,
        user_task: str | None = None,
        user_session_instructions: list[str] | None = None,
        component_observations: list[dict[str, Any]] | None = None,
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

        if prompt_assembly is not None:
            selected_provider = self.registry.select_provider(
                ModelPreferences(),
                purpose=purpose,
            )
            provider_supports_developer = selected_provider.capabilities().supports_developer_message
            if supports_developer_message is not None:
                provider_supports_developer = supports_developer_message
            prompt_bundle = prompt_assembly.build_for_model_turn(
                user_task=user_task or getattr(context, "user_goal", ""),
                purpose=purpose,
                user_session_instructions=user_session_instructions,
                component_observations=list(component_observations or []),
                retrieved_content=retrieved_content,
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
        context_bundle = getattr(context, "last_bundle", None)
        context_bundle_metadata = dict(getattr(context_bundle, "metadata", None) or {})
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
            "model_turn_request_builder": "model_turn_request_builder/v1",
            "stable_prefix_message_count": len(stable_messages),
            "dynamic_tail_message_count": len(dynamic_messages),
            "stable_prefix_hash": _messages_hash(stable_messages),
            "dynamic_tail_hash": _messages_hash(dynamic_messages),
            "tool_schema_hash": tool_schema_hash,
            "tool_names": [tool.name for tool in tools],
            "context_bundle_id": getattr(context_bundle, "bundle_id", None),
            "context_bundle_digest": getattr(context_bundle, "bundle_digest", None),
            "compression_snapshot_id": getattr(context_bundle, "compression_snapshot_id", None),
            "context_shape_hash": context_bundle_metadata.get("context_shape_hash"),
            "context_ordering_hash": context_bundle_metadata.get("context_ordering_hash"),
            "context_bundle_metadata": context_bundle_metadata,
            "tool_protocol": _tool_protocol_metadata(tools),
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


def _tool_protocol_metadata(tools: list[ModelToolSchema]) -> dict[str, Any]:
    tool_names = [tool.name for tool in tools]
    return {
        "version": "tool_protocol_metadata/v1",
        "tool_names": tool_names,
        "tool_count": len(tool_names),
        "has_verification_tools": _has_verification_tools(tool_names),
        "has_edit_tools": _has_edit_tools(tool_names),
    }


def _has_verification_tools(tool_names: list[str]) -> bool:
    verification_tools = {
        "plan_verification",
        "run_verification",
        "get_verification_result",
        "rerun_check",
    }
    return bool(verification_tools.intersection(tool_names))


def _has_edit_tools(tool_names: list[str]) -> bool:
    edit_tools = {"edit_plan", "edit_preview", "edit_apply", "apply_patch", "write_file"}
    return bool(edit_tools.intersection(tool_names))


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
