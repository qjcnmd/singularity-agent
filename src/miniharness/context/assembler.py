from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any
from uuid import uuid4

from miniharness.context.models import (
    ContextAuthority,
    ContextBudget,
    ContextBudgetPlan,
    ContextBundle,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextRenderPolicy,
    ContextSensitivity,
)
from miniharness.context.redaction import ContextRedactor
from miniharness.context.tokens import TokenCounter


class ContextOverflowError(ValueError):
    def __init__(self, message: str, *, budget: ContextBudgetPlan) -> None:
        super().__init__(message)
        self.budget = budget
        self.overflow_tokens = budget.overflow_tokens


@dataclass(frozen=True)
class _RenderGroup:
    item_ids: list[str]
    messages: list[dict[str, Any]]
    score: float
    layer: ContextLayer
    required: bool = False


class ContextAssembler:
    def __init__(
        self,
        *,
        token_counter: TokenCounter,
        model_context_window: int,
        output_token_reserve: int,
        reasoning_token_reserve: int = 0,
        redactor: ContextRedactor | None = None,
    ) -> None:
        self.token_counter = token_counter
        self.model_context_window = model_context_window
        self.output_token_reserve = output_token_reserve
        self.reasoning_token_reserve = reasoning_token_reserve
        self.redactor = redactor or ContextRedactor()

    def build_bundle(
        self,
        *,
        items: list[ContextItem],
        run_id: str,
        task_id: str,
        phase_id: str,
        model: str = "",
        provider: str = "",
        tools: list[dict[str, Any]] | None = None,
        render_policy: ContextRenderPolicy | None = None,
        compression_snapshot_id: str | None = None,
        retrieval_query: str | None = None,
    ) -> ContextBundle:
        policy = render_policy or ContextRenderPolicy()
        visible_items = [
            item
            for item in items
            if self._is_visible(item, policy)
            and item.freshness != ContextFreshness.OBSOLETE
        ]
        groups = self._groups_for_items(
            visible_items,
            phase_id=phase_id,
            render_policy=policy,
        )
        tool_tokens = self.token_counter.count_tools(tools)
        required = [group for group in groups if group.required]
        optional = sorted(
            [group for group in groups if not group.required],
            key=lambda group: group.score,
            reverse=True,
        )

        selected: list[_RenderGroup] = []
        for group in required:
            selected.append(group)
        base_messages = [message for group in selected for message in group.messages]
        base_budget = self._budget_for_messages(base_messages, tool_tokens)
        if base_budget.total_tokens > self.model_context_window:
            raise ContextOverflowError(
                "Required context plus reserved output tokens exceed the model context window.",
                budget=base_budget,
            )

        for group in optional:
            candidate = selected + [group]
            if self._budget_for_groups(candidate, tool_tokens).total_tokens <= self.model_context_window:
                selected.append(group)

        selected = self._order_selected_groups(selected)
        messages = [message for group in selected for message in group.messages]
        included = [item_id for group in selected for item_id in group.item_ids]
        excluded = [
            item.item_id
            for item in items
            if item.item_id not in included
            and item.layer not in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}
        ]
        budget = self._budget_for_messages(messages, tool_tokens)
        bundle = ContextBundle(
            bundle_id=f"bundle_{uuid4().hex[:12]}",
            run_id=run_id,
            task_id=task_id,
            phase_id=phase_id,
            model=model,
            provider=provider,
            messages=messages,
            included_item_ids=included,
            excluded_item_ids=excluded,
            budget=budget,
            compression_snapshot_id=compression_snapshot_id,
            retrieval_query=retrieval_query,
            render_policy=policy,
            metadata={
                "exclusion_reasons": self._exclusion_reasons(items, included),
                "lost_evidence_warning": self._lost_evidence_warning(items, included),
            },
        )
        return bundle

    def assemble(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]] | None = None,
        summary: str | None = None,
    ) -> tuple[list[dict[str, Any]], ContextBudget]:
        tool_tokens = self.token_counter.count_tools(tools)
        base = self._base_messages(messages, summary=summary)
        self._assert_base_fits(base, tool_tokens)
        groups = self._history_groups(messages[2:])
        selected: list[list[dict[str, Any]]] = []
        for group in reversed(groups):
            candidate = base + [
                message
                for selected_group in reversed(selected)
                for message in selected_group
            ]
            candidate = candidate + group
            if self._fits(candidate, tool_tokens):
                selected.append(group)

        assembled = base + [
            message for selected_group in reversed(selected) for message in selected_group
        ]
        return assembled, self._budget_for_messages(assembled, tool_tokens)

    def needs_compression(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]] | None = None,
    ) -> bool:
        tool_tokens = self.token_counter.count_tools(tools)
        return not self._fits(messages, tool_tokens)

    def _groups_for_items(
        self,
        items: list[ContextItem],
        *,
        phase_id: str,
        render_policy: ContextRenderPolicy,
    ) -> list[_RenderGroup]:
        standalone: list[ContextItem] = []
        assistant_items: list[ContextItem] = []
        tool_by_call: dict[str, ContextItem] = {}
        for item in items:
            message = self._message_for_item(item, render_policy)
            if message.get("role") == "assistant" and message.get("tool_calls"):
                assistant_items.append(item)
                continue
            if message.get("role") == "tool" and message.get("tool_call_id"):
                tool_by_call[str(message["tool_call_id"])] = item
                continue
            standalone.append(item)

        grouped_ids: set[str] = set()
        groups: list[_RenderGroup] = []
        for assistant in assistant_items:
            assistant_message = self._message_for_item(assistant, render_policy)
            call_ids = [
                str(call["id"])
                for call in assistant_message.get("tool_calls", [])
                if call.get("id")
            ]
            tools = [tool_by_call[call_id] for call_id in call_ids if call_id in tool_by_call]
            if len(tools) != len(call_ids):
                standalone.append(assistant)
                continue
            grouped_ids.add(assistant.item_id)
            grouped_ids.update(tool.item_id for tool in tools)
            groups.append(
                _RenderGroup(
                    item_ids=[assistant.item_id, *[tool.item_id for tool in tools]],
                    messages=[
                        assistant_message,
                        *[self._message_for_item(tool, render_policy) for tool in tools],
                    ],
                    score=max(
                        self._score_item(assistant, phase_id),
                        *[self._score_item(tool, phase_id) for tool in tools],
                    ),
                    layer=assistant.layer,
                    required=assistant.pinned or any(tool.pinned for tool in tools),
                )
            )
        for call_id, tool in tool_by_call.items():
            if tool.item_id not in grouped_ids:
                standalone.append(tool)
        for item in standalone:
            if item.item_id in grouped_ids:
                continue
            groups.append(
                _RenderGroup(
                    item_ids=[item.item_id],
                    messages=[self._message_for_item(item, render_policy)],
                    score=self._score_item(item, phase_id),
                    layer=item.layer,
                    required=item.pinned
                    or item.layer in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL},
                )
            )
        return groups

    def _message_for_item(
        self,
        item: ContextItem,
        policy: ContextRenderPolicy,
    ) -> dict[str, Any]:
        if isinstance(item.content, dict) and item.content.get("role"):
            message = dict(item.content)
            if policy.redact_sensitive:
                message["content"] = self.redactor.redact_value(message.get("content"))
            return message
        if item.metadata.get("raw_message"):
            content = item.content
            if policy.redact_sensitive:
                content = self.redactor.redact_value(content)
            return {
                "role": str(item.metadata.get("role") or "system"),
                "content": content if isinstance(content, str) else json.dumps(content, ensure_ascii=False, default=str),
            }
        role = "system"
        if item.layer == ContextLayer.USER_GOAL or item.item_type == ContextItemType.USER_MESSAGE:
            role = "user"
        elif item.item_type == ContextItemType.ASSISTANT_MESSAGE:
            role = "assistant"

        content = item.content
        if item.item_type == ContextItemType.TOOL_OBSERVATION:
            content = self._render_tool_observation(item, policy)
        elif item.layer == ContextLayer.COMPRESSED_HISTORY and item.item_type == ContextItemType.SUMMARY:
            content = f"Context summary:\n{item.content}"
        elif item.layer not in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}:
            refs = [ref.ref_id for ref in item.references]
            payload = json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str)
            refs_text = f" refs={','.join(refs)}" if refs else ""
            content = (
                f"[context:{item.item_id} {item.layer.value}/{item.item_type.value}"
                f" source={item.source_runtime.value} fresh={item.freshness.value}{refs_text}] "
                f"{payload}"
            )
        if policy.redact_sensitive:
            content = self.redactor.redact_value(content)
        return {
            "role": role,
            "content": content if isinstance(content, str) else json.dumps(content, ensure_ascii=False, default=str),
        }

    def _render_tool_observation(
        self,
        item: ContextItem,
        policy: ContextRenderPolicy,
    ) -> dict[str, Any]:
        content = item.content if isinstance(item.content, dict) else {"preview": item.content}
        rendered = {
            "context_item_id": item.item_id,
            "tool_name": content.get("tool_name"),
            "tool_call_id": content.get("tool_call_id"),
            "ok": content.get("ok"),
            "preview": content.get("preview") or content.get("content"),
            "truncated": content.get("truncated"),
            "reference_ids": content.get("reference_ids")
            or [ref.ref_id for ref in item.references],
            "raw_digest": content.get("raw_digest"),
        }
        if policy.include_raw_tool_outputs:
            rendered["raw_result"] = content.get("raw_result")
        return rendered

    def _is_visible(self, item: ContextItem, policy: ContextRenderPolicy) -> bool:
        if item.sensitivity == ContextSensitivity.SECRET and not policy.include_secret_content:
            return policy.redact_sensitive
        return True

    def _score_item(self, item: ContextItem, phase_id: str) -> float:
        score = float(item.importance)
        if item.pinned:
            score += 100
        if item.phase_id == phase_id:
            score += 10
        if item.freshness == ContextFreshness.CURRENT:
            score += 2
        if item.freshness == ContextFreshness.STALE:
            score -= 2
        score += _layer_weight(item.layer)
        score += _authority_weight(item.authority)
        if item.relevance_score is not None:
            score += item.relevance_score
        return score

    def _order_selected_groups(self, groups: list[_RenderGroup]) -> list[_RenderGroup]:
        front_layers = {
            ContextLayer.SYSTEM,
            ContextLayer.USER_GOAL,
            ContextLayer.COMPRESSED_HISTORY,
            ContextLayer.TASK_STATE,
            ContextLayer.PLANNER_STATE,
        }
        near_end_layers = {
            ContextLayer.POLICY_STATE,
            ContextLayer.VERIFICATION,
            ContextLayer.FAILURE_MEMORY,
            ContextLayer.TOOL_OBSERVATIONS,
            ContextLayer.WORKSPACE_STATE,
        }
        front = [group for group in groups if group.layer in front_layers]
        middle = [group for group in groups if group.layer not in front_layers | near_end_layers]
        tail = [group for group in groups if group.layer in near_end_layers]
        front.sort(key=lambda group: _layer_order(group.layer))
        middle.sort(key=lambda group: group.score)
        tail.sort(key=lambda group: group.score)
        return front + middle + tail

    def _budget_for_groups(
        self,
        groups: list[_RenderGroup],
        tool_tokens: int,
    ) -> ContextBudgetPlan:
        return self._budget_for_messages(
            [message for group in groups for message in group.messages],
            tool_tokens,
        )

    def _budget_for_messages(
        self,
        messages: list[dict[str, Any]],
        tool_tokens: int,
    ) -> ContextBudgetPlan:
        message_tokens = self.token_counter.count_messages(messages)
        system_tokens = self.token_counter.count_messages(
            [message for message in messages if message.get("role") == "system"]
        )
        recent_tokens = self.token_counter.count_messages(
            [message for message in messages if message.get("role") in {"assistant", "user"}]
        )
        used = (
            message_tokens
            + tool_tokens
            + self.output_token_reserve
            + self.reasoning_token_reserve
        )
        return ContextBudgetPlan(
            model_context_window=self.model_context_window,
            output_token_reserve=self.output_token_reserve,
            reasoning_token_reserve=self.reasoning_token_reserve,
            tool_schema_tokens=tool_tokens,
            system_tokens=system_tokens,
            pinned_tokens=system_tokens,
            evidence_tokens=max(0, message_tokens - system_tokens - recent_tokens),
            recent_dialogue_tokens=recent_tokens,
            summary_tokens=0,
            available_tokens=max(
                0,
                self.model_context_window
                - self.output_token_reserve
                - self.reasoning_token_reserve
                - tool_tokens,
            ),
            used_tokens=used,
            overflow_tokens=max(0, used - self.model_context_window),
            soft_limit=max(0, int(self.model_context_window * 0.9)),
            hard_limit=self.model_context_window,
            message_tokens=message_tokens,
        )

    def _base_messages(
        self,
        messages: list[dict[str, Any]],
        *,
        summary: str | None,
    ) -> list[dict[str, Any]]:
        base = [dict(messages[0]), dict(messages[1])]
        if summary:
            base.append(
                {
                    "role": "system",
                    "content": f"Context summary:\n{summary}",
                }
            )
        return base

    def _history_groups(
        self,
        history: list[dict[str, Any]],
    ) -> list[list[dict[str, Any]]]:
        groups: list[list[dict[str, Any]]] = []
        index = 0
        while index < len(history):
            message = history[index]
            if message.get("role") == "assistant" and message.get("tool_calls"):
                call_ids = {
                    call.get("id")
                    for call in message.get("tool_calls", [])
                    if call.get("id")
                }
                group = [message]
                index += 1
                while (
                    index < len(history)
                    and history[index].get("role") == "tool"
                    and history[index].get("tool_call_id") in call_ids
                ):
                    group.append(history[index])
                    index += 1
                groups.append(group)
                continue
            groups.append([message])
            index += 1
        return groups

    def _assert_base_fits(self, messages: list[dict[str, Any]], tool_tokens: int) -> None:
        budget = self._budget_for_messages(messages, tool_tokens)
        if budget.total_tokens > self.model_context_window:
            raise ContextOverflowError(
                "System/user context plus reserved output tokens exceed the model context window.",
                budget=budget,
            )

    def _fits(self, messages: list[dict[str, Any]], tool_tokens: int) -> bool:
        return self._budget_for_messages(messages, tool_tokens).total_tokens <= self.model_context_window

    @staticmethod
    def _exclusion_reasons(
        items: list[ContextItem],
        included_item_ids: list[str],
    ) -> dict[str, str]:
        included = set(included_item_ids)
        return {
            item.item_id: (
                "secret_not_rendered"
                if item.sensitivity == ContextSensitivity.SECRET
                else "budget_or_priority"
            )
            for item in items
            if item.item_id not in included
        }

    @staticmethod
    def _lost_evidence_warning(
        items: list[ContextItem],
        included_item_ids: list[str],
    ) -> bool:
        included = set(included_item_ids)
        return any(item.item_id not in included for item in items)


def _layer_weight(layer: ContextLayer) -> float:
    weights = {
        ContextLayer.SYSTEM: 100,
        ContextLayer.USER_GOAL: 90,
        ContextLayer.TASK_STATE: 40,
        ContextLayer.PLANNER_STATE: 38,
        ContextLayer.POLICY_STATE: 36,
        ContextLayer.VERIFICATION: 34,
        ContextLayer.FAILURE_MEMORY: 32,
        ContextLayer.WORKSPACE_STATE: 30,
        ContextLayer.MUTATION if hasattr(ContextLayer, "MUTATION") else ContextLayer.EVIDENCE: 28,
        ContextLayer.EVIDENCE: 26,
        ContextLayer.TOOL_OBSERVATIONS: 24,
        ContextLayer.COMPRESSED_HISTORY: 22,
        ContextLayer.RECENT_DIALOGUE: 10,
        ContextLayer.REFERENCES: 8,
        ContextLayer.SCRATCHPAD: 0,
    }
    return weights.get(layer, 0)


def _authority_weight(authority: ContextAuthority) -> float:
    weights = {
        ContextAuthority.SYSTEM: 10,
        ContextAuthority.USER: 9,
        ContextAuthority.RUNTIME: 7,
        ContextAuthority.TOOL: 6,
        ContextAuthority.SUMMARY: 4,
        ContextAuthority.MODEL: 1,
    }
    return weights.get(authority, 0)


def _layer_order(layer: ContextLayer) -> int:
    order = {
        ContextLayer.SYSTEM: 0,
        ContextLayer.USER_GOAL: 1,
        ContextLayer.COMPRESSED_HISTORY: 2,
        ContextLayer.TASK_STATE: 3,
        ContextLayer.PLANNER_STATE: 4,
    }
    return order.get(layer, 50)
