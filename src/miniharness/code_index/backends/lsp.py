from __future__ import annotations

from miniharness.code_index.exceptions import OptionalBackendUnavailable


class LspBackend:
    name = "lsp"
    version = "optional"

    def available(self) -> bool:
        return False

    def require_available(self) -> None:
        raise OptionalBackendUnavailable(self.name)
