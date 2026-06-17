from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any
from uuid import uuid4

from miniharness.context.assembler import ContextAssembler, ContextBudget
from miniharness.context.store import (
    ContextReference,
    ContextSnapshot,
    ObservationStore,
)
from miniharness.context.tokens import TokenCounter
from miniharness.provider import ToolChoiceMode


TOOL_RESULT_PREVIEW_LIMIT = 4000


@dataclass(frozen=True)
class ToolObservation:
    id: str
    tool_name: str
    tool_call_id: str | None
    ok: bool
    raw_result: dict[str, Any]
    preview: str
    truncated: bool
    metadata: dict[str, Any] = field(default_factory=dict)
    run_id: str = ""
    turn: int = 0
    created_at: str = ""
    input_tokens: int = 0
    preview_tokens: int = 0
    raw_digest: str = ""
    source_refs: list[ContextReference] = field(default_factory=list)
    cache_hit: bool = False
    duration_seconds: float | None = None
    error_code: str | None = None
    tool_version: str | None = None
    truncation_reason: str | None = None


class ContextManager:
    def __init__(
        self,
        *,
        system_prompt: str,
        user_goal: str,
        provider: Any | None = None,
        model_context_window: int = 128000,
        output_token_reserve: int = 4096,
        db_path: Path | None = None,
        run_id: str | None = None,
        token_counter: TokenCounter | None = None,
    ) -> None:
        self.run_id = run_id or uuid4().hex
        self.user_goal = user_goal
        self.provider = provider
        self.token_counter = token_counter or TokenCounter()
        self.assembler = ContextAssembler(
            token_counter=self.token_counter,
            model_context_window=model_context_window,
            output_token_reserve=output_token_reserve,
        )
        self.store = ObservationStore(db_path)
        self._messages: list[dict[str, Any]] = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user_goal},
        ]
        self.tool_observations: list[ToolObservation] = []
        self.last_budget: ContextBudget | None = None
        self._summary: str | None = None
        self._persist_initial_messages()

    def messages(
        self,
        *,
        tools: list[dict[str, Any]] | None = None,
        planner_context: dict[str, Any] | None = None,
    ) -> list[dict[str, Any]]:
        if self.assembler.needs_compression(messages=self._messages, tools=tools):
            self._compress_if_possible()
        source_messages = self._messages
        if planner_context is not None:
            source_messages = [*self._messages, planner_context]
        assembled, budget = self.assembler.assemble(
            messages=source_messages,
            tools=tools,
            summary=self._summary,
        )
        self.last_budget = budget
        return assembled

    def add_assistant_message(self, message: dict[str, Any]) -> None:
        copied = dict(message)
        self._messages.append(copied)
        self.store.append_message(run_id=self.run_id, message=copied)

    def add_tool_result(
        self,
        *,
        tool_call: dict[str, Any],
        result: dict[str, Any],
        turn: int = 0,
    ) -> ToolObservation:
        function = tool_call.get("function") or {}
        tool_name = function.get("name", "<unknown>")
        tool_call_id = tool_call.get("id")
        preview, truncated, truncation_reason = self._preview_result(result)
        raw_digest = self._digest_json(result)
        references = self._references_for_result(result, raw_digest=raw_digest)
        metadata = dict(result.get("metadata") or {})
        observation = ToolObservation(
            id=uuid4().hex,
            run_id=self.run_id,
            turn=turn,
            tool_name=tool_name,
            tool_call_id=tool_call_id,
            ok=bool(result.get("ok")),
            raw_result=result,
            preview=preview,
            truncated=truncated,
            metadata={
                "result_keys": sorted(result.keys()),
                **metadata,
            },
            created_at=self._now(),
            input_tokens=self.token_counter.count_text(
                json.dumps(tool_call, ensure_ascii=False, sort_keys=True, default=str)
            ),
            preview_tokens=self.token_counter.count_text(preview),
            raw_digest=raw_digest,
            source_refs=references,
            cache_hit=bool(metadata.get("cache_hit")),
            duration_seconds=metadata.get("duration_seconds"),
            error_code=result.get("error_code"),
            tool_version=metadata.get("tool_version"),
            truncation_reason=truncation_reason,
        )
        object.__setattr__(
            observation,
            "source_refs",
            [
                ContextReference(
                    id=ref.id,
                    type=ref.type,
                    path=ref.path,
                    line_start=ref.line_start,
                    line_end=ref.line_end,
                    digest=ref.digest,
                    observation_id=observation.id,
                )
                for ref in references
            ],
        )
        self.tool_observations.append(observation)
        self.store.save_observation(observation)
        tool_message = self._tool_message(observation)
        self._messages.append(tool_message)
        self.store.append_message(run_id=self.run_id, message=tool_message)
        return observation

    def _persist_initial_messages(self) -> None:
        if self.store.load_messages(self.run_id):
            return
        for message in self._messages:
            self.store.append_message(run_id=self.run_id, message=message)

    def _compress_if_possible(self) -> None:
        if self.provider is None:
            return
        compression_messages = [
            {
                "role": "system",
                "content": (
                    "Summarize Miniharness context as compact JSON with keys: "
                    "summary, goal, constraints, verified_facts, failed_attempts, reference_ids."
                ),
            },
            {
                "role": "user",
                "content": json.dumps(
                    {
                        "goal": self.user_goal,
                        "messages": self._messages,
                        "observation_ids": [
                            observation.id for observation in self.tool_observations
                        ],
                    },
                    ensure_ascii=False,
                    default=str,
                ),
            },
        ]
        response = self.provider.chat(
            messages=compression_messages,
            tools=[],
            tool_choice=ToolChoiceMode.NONE,
        )
        content = (
            ((response.get("choices") or [{}])[0].get("message") or {}).get("content")
            or ""
        )
        summary, known_ids = self._parse_summary(content)
        self._summary = summary
        snapshot = ContextSnapshot(
            id=uuid4().hex,
            run_id=self.run_id,
            goal=self.user_goal,
            summary=summary,
            retained_messages=self._messages[:2],
            known_observation_ids=known_ids,
            version=self.store.current_version(self.run_id),
            created_at=self._now(),
        )
        self.store.save_snapshot(snapshot)

    def _tool_message(self, observation: ToolObservation) -> dict[str, Any]:
        return {
            "role": "tool",
            "tool_call_id": observation.tool_call_id,
            "name": observation.tool_name,
            "content": json.dumps(
                {
                    "ok": observation.ok,
                    "tool_name": observation.tool_name,
                    "tool_call_id": observation.tool_call_id,
                    "observation_id": observation.id,
                    "reference_ids": [ref.id for ref in observation.source_refs],
                    "content": observation.preview,
                    "truncated": observation.truncated,
                },
                ensure_ascii=False,
            ),
        }

    @staticmethod
    def _preview_result(result: dict[str, Any]) -> tuple[str, bool, str | None]:
        content = result.get("content")
        if isinstance(content, dict) and isinstance(content.get("content"), str):
            source = content["content"]
        elif isinstance(content, str):
            source = content
        else:
            source = json.dumps(result, ensure_ascii=False, sort_keys=True, default=str)
        result_already_truncated = bool(result.get("truncated"))
        if len(source) <= TOOL_RESULT_PREVIEW_LIMIT:
            return source, result_already_truncated, (
                "tool_result" if result_already_truncated else None
            )
        return source[:TOOL_RESULT_PREVIEW_LIMIT], True, "preview_limit"

    def _references_for_result(
        self, result: dict[str, Any], *, raw_digest: str
    ) -> list[ContextReference]:
        content = result.get("content")
        if not isinstance(content, dict) or not content.get("path"):
            return []
        line_start = content.get("line")
        line_end = line_start
        ref_id = f"ref_{uuid4().hex}"
        return [
            ContextReference(
                id=ref_id,
                type="file",
                path=str(content["path"]),
                line_start=line_start if isinstance(line_start, int) else None,
                line_end=line_end if isinstance(line_end, int) else None,
                digest=raw_digest,
                observation_id="",
            )
        ]

    @staticmethod
    def _parse_summary(content: str) -> tuple[str, list[str]]:
        try:
            parsed = json.loads(content)
        except json.JSONDecodeError:
            return content, []
        summary = parsed.get("summary") or content
        reference_ids = parsed.get("reference_ids") or []
        if not isinstance(reference_ids, list):
            reference_ids = []
        return str(summary), [str(reference_id) for reference_id in reference_ids]

    @staticmethod
    def _digest_json(value: Any) -> str:
        payload = json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)
        return hashlib.sha256(payload.encode("utf-8")).hexdigest()

    @staticmethod
    def _now() -> str:
        return datetime.now(UTC).isoformat()
