from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, Field

from miniharness.tools.models import PermissionLevel, ToolSpec
from miniharness.workspace_state import LocalWorkspaceStateRuntime


class WorkspaceHealthInput(BaseModel):
    refresh_external: bool = Field(
        True,
        description="Refresh external workspace changes before reporting health.",
    )


class WorkspaceHealthToolHandlers:
    def __init__(self, runtime: LocalWorkspaceStateRuntime) -> None:
        self.runtime = runtime

    def workspace_health(self, args: WorkspaceHealthInput) -> dict[str, Any]:
        if args.refresh_external:
            self.runtime.record_external_changes()
        return self.runtime.get_workspace_health().to_observation()


def register_workspace_state_tools(
    registry: Any,
    runtime: LocalWorkspaceStateRuntime | None = None,
) -> None:
    state_runtime = runtime or LocalWorkspaceStateRuntime(Path(registry.project_root))
    handlers = WorkspaceHealthToolHandlers(state_runtime)
    registry.register(
        ToolSpec(
            name="workspace_health",
            version="0.0.1",
            description="Report workspace state through LocalWorkspaceStateRuntime.",
            input_model=WorkspaceHealthInput,
            handler=handlers.workspace_health,
            permission_level=PermissionLevel.READ_ONLY,
            risk_tags=("workspace_state",),
            timeout_seconds=5.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=True,
        )
    )
