from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools.models import PermissionLevel, ToolSpec
from singularity.tools.models import ToolSideEffectKind, ToolSensitivityLevel
from singularity.workspace_state import LocalWorkspaceStateRuntime


class WorkspaceHealthInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

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
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.READ_FILE,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "workspace_health", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("workspace_state",),
            timeout_seconds=5.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=True,
        )
    )
