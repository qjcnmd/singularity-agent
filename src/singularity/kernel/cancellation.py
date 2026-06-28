from __future__ import annotations

from dataclasses import dataclass
from threading import RLock

from singularity.kernel.exceptions import CancellationError
from singularity.kernel.models import CancellationReason


@dataclass(frozen=True)
class CancellationState:
    cancelled: bool
    reason: CancellationReason | None = None
    message: str = ""


class CancellationToken:
    def __init__(self, parent: CancellationToken | None = None) -> None:
        self._parent = parent
        self._state = CancellationState(cancelled=False)
        self._children: list[CancellationToken] = []
        self._lock = RLock()

    @property
    def cancelled(self) -> bool:
        return self.state.cancelled

    @property
    def reason(self) -> CancellationReason | None:
        return self.state.reason

    @property
    def state(self) -> CancellationState:
        if self._parent is not None and self._parent.cancelled:
            return self._parent.state
        return self._state

    def cancel(
        self,
        reason: CancellationReason = CancellationReason.SHUTDOWN_REQUESTED,
        message: str = "",
    ) -> None:
        with self._lock:
            self._state = CancellationState(cancelled=True, reason=reason, message=message)
            for child in self._children:
                child.cancel(reason, message)

    def throw_if_cancelled(self) -> None:
        state = self.state
        if state.cancelled:
            raise CancellationError(
                state.message or f"Cancelled: {state.reason.value if state.reason else 'unknown'}",
                code="cancelled",
                details={"reason": state.reason.value if state.reason else None},
            )

    def child_token(self) -> CancellationToken:
        with self._lock:
            child = CancellationToken(parent=self)
            self._children.append(child)
            if self._state.cancelled:
                child.cancel(
                    self._state.reason or CancellationReason.SHUTDOWN_REQUESTED,
                    self._state.message,
                )
            return child


class CancellationManager:
    def __init__(self) -> None:
        self.token = CancellationToken()

    def cancel(
        self,
        reason: CancellationReason = CancellationReason.SHUTDOWN_REQUESTED,
        message: str = "",
    ) -> None:
        self.token.cancel(reason, message)

    def throw_if_cancelled(self) -> None:
        self.token.throw_if_cancelled()

    def child_token(self) -> CancellationToken:
        return self.token.child_token()
