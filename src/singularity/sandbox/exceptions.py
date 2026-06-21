from __future__ import annotations


class SandboxError(Exception):
    pass


class SandboxUnavailable(SandboxError):
    pass


class SandboxCapabilityError(SandboxUnavailable):
    pass


class SandboxSetupError(SandboxError):
    pass


class SandboxExecutionError(SandboxError):
    pass


class SandboxTimeout(SandboxExecutionError):
    pass


class SandboxViolationError(SandboxError):
    pass


class SandboxCleanupError(SandboxError):
    pass
