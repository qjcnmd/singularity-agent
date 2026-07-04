from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, replace
from typing import Any
from uuid import uuid4

from singularity.context.models import (
    CacheAttribution,
    CacheAttributionSource,
    ContextBudget,
    ContextBudgetPlan,
    ContextBundle,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextRenderPolicy,
    ContextSensitivity,
    ContextUsageReport,
)
from singularity.context.ranking import authority_weight, layer_order, layer_weight
from singularity.context.redaction import ContextRedactor
from singularity.context.tokens import TokenCounter

MAX_CONTEXT_FRAGMENT_TOKENS = 1000
MAX_CONTEXT_FRAGMENT_BYTES = 12000
LATEST_PROTOCOL_GROUP_SCORE_BONUS = 1000.0


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
            and item.freshness == ContextFreshness.CURRENT
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
            candidate = [*selected, group]
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
        item_by_id = {item.item_id: item for item in items}
        context_shape = self._context_shape(selected, item_by_id)
        usage_report = self._usage_report(
            items,
            included_item_ids=included,
            excluded_item_ids=excluded,
            budget=budget,
        )
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
                "context_shape_hash": _hash_json(context_shape),
                "context_ordering_hash": _hash_json(included),
                "context_shape": context_shape,
                "cache": {
                    "input_tokens": budget.message_tokens + budget.tool_schema_tokens,
                    "cached_input_tokens": 0,
                    "cache_hit_ratio": 0.0,
                    "cache_miss_reasons": [],
                    "cache_attribution": CacheAttribution(
                        source=CacheAttributionSource.UNKNOWN,
                        confidence=0.0,
                        reasons=[],
                        evidence=[],
                    ).to_dict(),
                },
                "context_usage_report": usage_report.to_dict(),
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
        protocol_group_indices: list[int] = []
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
            group = _RenderGroup(
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
            groups.append(group)
            protocol_group_indices.append(len(groups) - 1)
        if protocol_group_indices:
            latest_protocol_index = protocol_group_indices[-1]
            groups[latest_protocol_index] = replace(
                groups[latest_protocol_index],
                score=groups[latest_protocol_index].score + LATEST_PROTOCOL_GROUP_SCORE_BONUS,
            )
        for _call_id, tool in tool_by_call.items():
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
            if message.get("role") == "tool":
                message["content"] = self._bounded_tool_content(
                    str(message.get("content") or ""),
                    max_tokens=policy.max_tool_preview_tokens,
                )
                return message
            if (
                item.layer not in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}
                and not message.get("tool_calls")
            ):
                message["content"] = self._bounded_fragment(item, message.get("content"))
            return message
        if item.metadata.get("raw_message"):
            content = item.content
            if policy.redact_sensitive:
                content = self.redactor.redact_value(content)
            if item.layer not in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}:
                raw = content if isinstance(content, str) else json.dumps(content, ensure_ascii=False, default=str)
                content = (
                    f"{self._bounded_text(raw, max_tokens=MAX_CONTEXT_FRAGMENT_TOKENS)}\n"
                    f"[context:{item.item_id} {item.layer.value}/{item.item_type.value} "
                    f"source={item.source_component.value} digest={item.content_digest[:12]}]"
                )
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
            content = (
                "Context summary:\n"
                f"{self._bounded_text(str(item.content), max_tokens=160)}"
            )
        elif item.layer not in {ContextLayer.SYSTEM, ContextLayer.USER_GOAL}:
            refs = [ref.ref_id for ref in item.references]
            payload = json.dumps(item.content, ensure_ascii=False, sort_keys=True, default=str)
            refs_text = f" refs={','.join(refs)}" if refs else ""
            content = (
                f"[context:{item.item_id} {item.layer.value}/{item.item_type.value}"
                f" source={item.source_component.value} fresh={item.freshness.value}"
                f" digest={item.content_digest[:12]}{refs_text}] "
                f"{self._bounded_text(payload, max_tokens=MAX_CONTEXT_FRAGMENT_TOKENS)}"
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
        metadata_value = content.get("metadata")
        metadata = metadata_value if isinstance(metadata_value, dict) else {}
        rendered = {
            "context_item_id": item.item_id,
            "tool_name": content.get("tool_name"),
            "tool_call_id": content.get("tool_call_id"),
            "ok": content.get("ok"),
            "preview": self._bounded_text(
                str(content.get("preview") or content.get("content") or ""),
                max_tokens=policy.max_tool_preview_tokens,
            ),
            "truncated": content.get("truncated"),
            "reference_ids": content.get("reference_ids")
            or [ref.ref_id for ref in item.references],
            "raw_digest": content.get("raw_digest"),
            "artifact_refs": content.get("artifact_refs") or metadata.get("artifact_refs") or [],
            "logs_ref": content.get("logs_ref") or metadata.get("logs_ref"),
        }
        if policy.include_raw_tool_outputs:
            rendered["raw_result"] = content.get("raw_result")
        return rendered

    def _is_visible(self, item: ContextItem, policy: ContextRenderPolicy) -> bool:
        if item.pinned:
            return True
        if item.sensitivity == ContextSensitivity.SECRET and not policy.include_secret_content:
            return policy.redact_sensitive
        return True

    def _bounded_fragment(self, item: ContextItem, value: Any) -> str:
        raw = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
        bounded = self._bounded_text(raw, max_tokens=MAX_CONTEXT_FRAGMENT_TOKENS)
        token_count = self.token_counter.count_text(bounded)
        byte_count = len(bounded.encode("utf-8"))
        return (
            f"[context:{item.item_id} {item.layer.value}/{item.item_type.value} "
            f"source={item.source_component.value} digest={item.content_digest[:12]} "
            f"t={token_count} b={byte_count}] {bounded}"
        )

    def _bounded_text(self, text: str, *, max_tokens: int) -> str:
        value = text
        encoded = value.encode("utf-8")
        truncated = False
        if len(encoded) > MAX_CONTEXT_FRAGMENT_BYTES:
            value = encoded[:MAX_CONTEXT_FRAGMENT_BYTES].decode("utf-8", errors="ignore")
            truncated = True
        for _ in range(4):
            token_count = self.token_counter.count_text(value)
            if token_count <= max_tokens:
                break
            keep = max(1, int(len(value) * max_tokens / max(token_count, 1)))
            value = value[:keep]
            truncated = True
        if truncated:
            value = value.rstrip() + "\n[truncated:context_fragment_cap]"
        return value

    def _bounded_tool_content(self, content: str, *, max_tokens: int) -> str:
        if self.token_counter.count_text(content) <= max_tokens:
            return content
        try:
            payload = json.loads(content)
        except json.JSONDecodeError:
            return self._bounded_text(content, max_tokens=max_tokens)
        if not isinstance(payload, dict):
            return self._bounded_text(content, max_tokens=max_tokens)
        already_preview_limited = (
            payload.get("truncated") is True
            and payload.get("truncation_reason")
            not in {None, "context_fragment_cap"}
        )
        if already_preview_limited:
            return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
        for key in ("preview", "content", "content_preview"):
            if isinstance(payload.get(key), str):
                payload[key] = self._bounded_text(
                    payload[key],
                    max_tokens=max(32, max_tokens // 2),
                )
        metadata = payload.get("metadata")
        if isinstance(metadata, dict):
            payload["metadata"] = {
                key: value
                for key, value in metadata.items()
                if key in {"status", "result_ref", "policy_decision_id", "tool_name"}
            }
        payload["truncated"] = True
        payload["truncation_reason"] = payload.get("truncation_reason") or "context_fragment_cap"
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    def _context_shape(
        self,
        groups: list[_RenderGroup],
        item_by_id: dict[str, ContextItem],
    ) -> list[dict[str, Any]]:
        shape: list[dict[str, Any]] = []
        for group in groups:
            for item_id in group.item_ids:
                item = item_by_id.get(item_id)
                if item is None:
                    continue
                shape.append(
                    {
                        "layer": item.layer.value,
                        "item_type": item.item_type.value,
                        "source_component": item.source_component.value,
                        "role": group.messages[0].get("role") if group.messages else "",
                    }
                )
        return shape

    def _usage_report(
        self,
        items: list[ContextItem],
        *,
        included_item_ids: list[str],
        excluded_item_ids: list[str],
        budget: ContextBudgetPlan,
    ) -> ContextUsageReport:
        included = set(included_item_ids)
        layer_tokens: dict[str, int] = {}
        for item in items:
            if item.item_id not in included:
                continue
            layer_tokens[item.layer.value] = layer_tokens.get(item.layer.value, 0) + int(item.token_count or 0)
        stale = [
            item.item_id
            for item in items
            if item.freshness != ContextFreshness.CURRENT and not item.pinned
        ]
        summary = [
            item.item_id
            for item in items
            if item.item_id in included and item.layer == ContextLayer.COMPRESSED_HISTORY
        ]
        recent_tail = [
            item.item_id
            for item in items
            if item.item_id in included and item.layer == ContextLayer.RECENT_DIALOGUE
        ]
        recommendations: list[str] = []
        if excluded_item_ids:
            recommendations.append("compact_or_retrieve_excluded_context")
        if stale:
            recommendations.append("avoid_reintroducing_stale_items")
        return ContextUsageReport(
            layer_token_usage=layer_tokens,
            included_item_ids=list(included_item_ids),
            excluded_item_ids=list(excluded_item_ids),
            stale_item_ids=stale,
            summary_item_ids=summary,
            recent_tail_item_ids=recent_tail,
            input_tokens=budget.message_tokens + budget.tool_schema_tokens,
            cache_miss_reasons=[],
            recommendations=recommendations,
        )

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
        score += layer_weight(item.layer)
        score += authority_weight(item.authority)
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
        front.sort(key=lambda group: layer_order(group.layer))
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


def _hash_json(value: Any) -> str:
    text = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
    return hashlib.sha256(text.encode("utf-8")).hexdigest()
