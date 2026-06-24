from __future__ import annotations

from typing import Any


class KernelError(RuntimeError):
    def __init__(
        self,
        message: str,
        *,
        code: str = "kernel_error",
        details: dict[str, Any] | None = None,
        final_report: Any | None = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.details = details or {}
        self.final_report = final_report

    def to_dict(self) -> dict[str, Any]:
        return {
            "type": type(self).__name__,
            "code": self.code,
            "message": str(self),
            "details": self.details,
        }


class KernelBootstrapError(KernelError):
    pass


class AgentGraphError(KernelError):
    pass


class AgentGraphInitializationError(KernelError):
    pass


class ComponentHealthError(KernelError):
    pass


class WorkspaceLockError(KernelError):
    pass


class CancellationError(KernelError):
    pass


class ShutdownError(KernelError):
    pass


class RecoveryError(KernelError):
    pass


class FinalizationError(KernelError):
    pass
