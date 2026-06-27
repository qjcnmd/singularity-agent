from __future__ import annotations

import os
from typing import Protocol

from singularity.sandbox.models import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxRequest,
    SandboxResult,
)
from singularity.sandbox.windows import WindowsSandboxBackend


class SandboxBackend(Protocol):
    def name(self) -> str:
        ...

    def capabilities(self) -> SandboxCapabilities:
        ...

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        ...

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        ...

    def cleanup(self, prepared: PreparedSandbox) -> None:
        ...


def default_sandbox_backends() -> list[SandboxBackend]:
    if os.name != "nt":
        return []
    return [WindowsSandboxBackend()]
