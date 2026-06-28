from __future__ import annotations

import threading
from collections import OrderedDict
from dataclasses import dataclass

from singularity.tools.models import ToolResult


@dataclass
class _ReplayEntry:
    args_fingerprint: str
    result: ToolResult
    replay_allowed: bool


class IdempotencyLedger:
    def __init__(self, *, max_entries: int = 512) -> None:
        if max_entries <= 0:
            raise ValueError("max_entries must be greater than zero.")
        self.max_entries = max_entries
        self._entries: OrderedDict[str, _ReplayEntry] = OrderedDict()
        self._lock = threading.RLock()

    def check(
        self,
        tool_call_id: str | None,
        args_fingerprint: str,
        *,
        replay_allowed: bool,
    ) -> ToolResult | None:
        if not tool_call_id:
            return None
        with self._lock:
            existing = self._entries.get(tool_call_id)
            if existing is None:
                return None
            self._entries.move_to_end(tool_call_id)
            if existing.args_fingerprint != args_fingerprint:
                return ToolResult.failure(
                    code="conflicting_replay",
                    message="Duplicate tool_call_id was reused with different arguments.",
                )
            if not existing.replay_allowed:
                return ToolResult.failure(
                    code="replay_not_allowed",
                    message="Duplicate tool_call_id replay is not allowed for this tool.",
                )
            replay = existing.result.model_copy(deep=True)
            replay.metadata["replay"] = True
            return replay

    def remember(
        self,
        tool_call_id: str | None,
        args_fingerprint: str,
        result: ToolResult,
        *,
        replay_allowed: bool,
    ) -> None:
        if not tool_call_id:
            return
        with self._lock:
            self._entries[tool_call_id] = _ReplayEntry(
                args_fingerprint=args_fingerprint,
                result=result.model_copy(deep=True),
                replay_allowed=replay_allowed,
            )
            self._entries.move_to_end(tool_call_id)
            while len(self._entries) > self.max_entries:
                self._entries.popitem(last=False)
