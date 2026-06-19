from __future__ import annotations


class CodeIndexError(Exception):
    def __init__(
        self,
        message: str,
        *,
        code: str = "code_index_error",
        details: dict[str, object] | None = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.details = details or {}


class PathOutsideWorkspaceError(CodeIndexError):
    def __init__(self, path: str) -> None:
        super().__init__(
            "Path is outside the workspace root.",
            code="path_outside_workspace",
            details={"path": path},
        )


class IndexBudgetExceededError(CodeIndexError):
    def __init__(self, budget: str, limit: int) -> None:
        super().__init__(
            "Project index budget exceeded.",
            code="index_budget_exceeded",
            details={"budget": budget, "limit": limit},
        )


class IndexStoreError(CodeIndexError):
    pass


class OptionalBackendUnavailable(CodeIndexError):
    def __init__(self, backend: str) -> None:
        super().__init__(
            f"Optional code index backend is unavailable: {backend}",
            code="optional_backend_unavailable",
            details={"backend": backend},
        )
