from __future__ import annotations

from miniharness.code_index.exceptions import OptionalBackendUnavailable


class TreeSitterBackend:
    name = "tree_sitter"
    version = "optional"

    def available(self) -> bool:
        try:
            import tree_sitter  # noqa: F401
        except Exception:
            return False
        return True

    def require_available(self) -> None:
        if not self.available():
            raise OptionalBackendUnavailable(self.name)
