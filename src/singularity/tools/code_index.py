from __future__ import annotations

from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict, Field

from singularity.code_index import ProjectIndex
from singularity.policy import Capability, OperationKind, ResourceRef
from singularity.tools.models import (
    PermissionLevel,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)


class IndexRelevantInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    goal: str
    hints: list[str] = Field(default_factory=list)


class IndexSymbolsInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    query: str


class IndexImpactInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    paths: list[str]


class IndexTestsInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    changed_files: list[str]


class IndexNoInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


class CodeIndexToolHandlers:
    def __init__(self, project_index: ProjectIndex) -> None:
        self.component = project_index

    def relevant(self, args: IndexRelevantInput) -> dict[str, Any]:
        return {
            "project_index": self.component.observation_for_goal(
                args.goal,
                hints=args.hints,
            )
        }

    def symbols(self, args: IndexSymbolsInput) -> dict[str, Any]:
        return {"symbols": [symbol.to_dict() for symbol in self.component.find_symbols(args.query)[:50]]}

    def explain(self, args: IndexNoInput) -> dict[str, Any]:
        _ = args
        return {"project_index": self.component.explain()}

    def impact(self, args: IndexImpactInput) -> dict[str, Any]:
        return {"impact": self.component.analyze_impact(args.paths).to_dict()}

    def tests(self, args: IndexTestsInput) -> dict[str, Any]:
        return {"test_impact": self.component.get_test_impact(args.changed_files).to_dict()}


def register_code_index_tools(
    registry: Any,
    project_index: ProjectIndex | None = None,
) -> None:
    project_index = project_index or ProjectIndex(Path(registry.project_root))
    handlers = CodeIndexToolHandlers(project_index)
    for spec in (
        ToolSpec(
            name="index_relevant",
            version="0.1.0",
            description="Find relevant files from the ProjectIndex without reading full file contents.",
            input_model=IndexRelevantInput,
            handler=handlers.relevant,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "project_index", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("project_index", "read_only"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        ),
        ToolSpec(
            name="index_symbols",
            version="0.1.0",
            description="Search symbols from the ProjectIndex.",
            input_model=IndexSymbolsInput,
            handler=handlers.symbols,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "project_index", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("project_index", "read_only"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        ),
        ToolSpec(
            name="index_explain",
            version="0.1.0",
            description="Explain indexed project structure, entrypoints, config facts, and limitations.",
            input_model=IndexNoInput,
            handler=handlers.explain,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.READ_FILE,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "project_index", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("project_index", "read_only"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        ),
        ToolSpec(
            name="index_impact",
            version="0.1.0",
            description="Analyze code-index impact for changed or planned paths.",
            input_model=IndexImpactInput,
            handler=handlers.impact,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "project_index", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("project_index", "impact"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        ),
        ToolSpec(
            name="index_tests",
            version="0.1.0",
            description="Return code-index test impact and suggested verification scope.",
            input_model=IndexTestsInput,
            handler=handlers.tests,
            permission_level=PermissionLevel.READ_ONLY,
            capabilities=(Capability.READ_WORKSPACE,),
            operation=OperationKind.SEARCH,
            resource_resolver=lambda _args, _root: [
                ResourceRef("workspace", "project_index", workspace_relative=True)
            ],
            side_effects=ToolSideEffectKind.READ_WORKSPACE,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            risk_tags=("project_index", "test_mapping"),
            timeout_seconds=10.0,
            max_output_chars=16000,
            cacheable=False,
            idempotent=True,
        ),
    ):
        registry.register(spec)
