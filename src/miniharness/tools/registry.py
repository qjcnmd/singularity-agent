from __future__ import annotations

from pathlib import Path
from typing import Any

from miniharness.tools.models import ToolSpec
from miniharness.tools.policy import ToolPolicy


class ToolRegistry:
    def __init__(self, project_root: Path, *, include_default_tools: bool = True) -> None:
        self.project_root = project_root.resolve()
        self._tools: dict[str, ToolSpec] = {}
        if include_default_tools:
            from miniharness.tools.read_only import register_read_only_tools

            register_read_only_tools(self)

    def register(self, spec: ToolSpec) -> None:
        if spec.name in self._tools:
            raise ValueError(f"Tool already registered: {spec.name}")
        self._tools[spec.name] = spec

    def get(self, name: str) -> ToolSpec | None:
        return self._tools.get(name)

    def list(self) -> list[ToolSpec]:
        return list(self._tools.values())

    def to_openai_tools(self, *, strict: bool = False) -> list[dict[str, Any]]:
        tools: list[dict[str, Any]] = []
        for spec in self._tools.values():
            parameters = spec.input_model.model_json_schema()
            function: dict[str, Any] = {
                "name": spec.name,
                "description": spec.description,
                "parameters": self._parameters_schema(parameters, strict=strict),
            }
            if strict:
                function["strict"] = True
            tools.append({"type": "function", "function": function})
        return tools

    def openai_tools(self, *, strict: bool = False) -> list[dict[str, Any]]:
        return self.to_openai_tools(strict=strict)

    def schema_export(self, *, strict: bool = False) -> list[dict[str, Any]]:
        return self.to_openai_tools(strict=strict)

    def dispatch(self, tool_call: dict[str, Any]) -> dict[str, Any]:
        from miniharness.tools.runtime import ToolRuntime

        runtime = ToolRuntime(
            registry=self,
            policy=ToolPolicy.read_only(),
            trace=None,
            workspace_root=self.project_root,
        )
        return runtime.execute_tool_call(tool_call).model_dump(mode="json")

    @staticmethod
    def _parameters_schema(parameters: dict[str, Any], *, strict: bool) -> dict[str, Any]:
        if strict and parameters.get("type") == "object":
            parameters = dict(parameters)
            parameters["additionalProperties"] = False
        return parameters
