from __future__ import annotations

import json
from typing import Any

from singularity.context.tokens import TokenCounter
from singularity.model.models import (
    ContentBlock,
    ContentBlockType,
    ModelCapabilities,
    ModelMessage,
    ModelRole,
)


class MessageConverter:
    def __init__(self, *, token_counter: TokenCounter | None = None) -> None:
        self.token_counter = token_counter or TokenCounter()

    def to_provider_messages(
        self,
        messages: list[ModelMessage],
        *,
        capabilities: ModelCapabilities,
    ) -> list[dict[str, Any]]:
        provider_messages: list[dict[str, Any]] = []
        for message in messages:
            role = message.role.value
            metadata = dict(message.metadata)
            if message.role == ModelRole.DEVELOPER and not capabilities.supports_developer_message:
                role = "system" if capabilities.supports_system_message else "user"
                metadata["developer_fallback"] = role
            provider: dict[str, Any] = {
                "role": role,
                "content": self._content_text(message),
            }
            if message.name:
                provider["name"] = message.name
            if message.tool_call_id:
                provider["tool_call_id"] = message.tool_call_id
            if metadata:
                provider["metadata"] = metadata
            provider_messages.append(provider)
        return provider_messages

    def from_provider_message(self, payload: dict[str, Any]) -> ModelMessage:
        role = ModelRole(payload.get("role") or ModelRole.ASSISTANT.value)
        content = payload.get("content")
        text = "" if content is None else str(content)
        return ModelMessage(
            role=role,
            content=[ContentBlock(type=ContentBlockType.TEXT, text=text)],
            name=payload.get("name"),
            tool_call_id=payload.get("tool_call_id"),
            metadata={
                key: value
                for key, value in payload.items()
                if key not in {"role", "content", "name", "tool_call_id"}
            },
        )

    def from_openai_dict(self, payload: dict[str, Any]) -> ModelMessage:
        content = payload.get("content")
        role = ModelRole(payload.get("role") or ModelRole.USER.value)
        return ModelMessage(
            role=role,
            content=[ContentBlock.from_text("" if content is None else str(content))],
            name=payload.get("name"),
            tool_call_id=payload.get("tool_call_id"),
            metadata={key: value for key, value in payload.items() if key not in {"role", "content", "name", "tool_call_id"}},
        )

    def to_openai_dict(self, message: ModelMessage) -> dict[str, Any]:
        payload: dict[str, Any] = {"role": message.role.value, "content": self._content_text(message)}
        if message.name:
            payload["name"] = message.name
        if message.tool_call_id:
            payload["tool_call_id"] = message.tool_call_id
        payload.update(message.metadata)
        return payload

    def estimate_tokens(self, messages: list[ModelMessage]) -> int:
        provider = [self.to_openai_dict(message) for message in messages]
        return self.token_counter.count_messages(provider)

    @staticmethod
    def _content_text(message: ModelMessage) -> str:
        parts: list[str] = []
        for block in message.content:
            if block.type == ContentBlockType.ARTIFACT_REF and block.artifact_ref:
                parts.append(json.dumps({"artifact_ref": block.artifact_ref, **block.metadata}, sort_keys=True))
            else:
                parts.append(block.text or "")
        return "".join(parts)

