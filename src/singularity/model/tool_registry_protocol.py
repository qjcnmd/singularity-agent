from __future__ import annotations

from typing import Any, Protocol

from singularity.tools.models import RegisteredToolRecord, ToolSpec


class ModelToolRegistryProtocol(Protocol):
    def schema_export(self, *, strict: bool = False) -> list[dict[str, Any]]:
        ...

    def list_model_visible(self) -> list[ToolSpec]:
        ...

    def get(self, name: str) -> ToolSpec | None:
        ...

    def get_record(self, name: str) -> RegisteredToolRecord | None:
        ...
