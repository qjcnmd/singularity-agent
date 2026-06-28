from __future__ import annotations

from typing import Any

MUTATION_ERROR_CODES = frozenset(
    {
        "path_outside_workspace",
        "symlink_escape",
        "path_denied",
        "file_class_denied",
        "file_not_found",
        "file_too_large",
        "binary_file_denied",
        "encoding_error",
        "snapshot_mismatch",
        "file_changed",
        "patch_context_not_found",
        "patch_context_ambiguous",
        "invalid_patch",
        "invalid_operation",
        "unsupported_operation",
        "new_file_not_allowed",
        "unexpected_patch_files",
        "policy_denied",
        "review_required",
        "preflight_failed",
        "atomic_write_failed",
        "transaction_failed",
        "rollback_failed",
        "rollback_conflict",
        "diff_too_large",
        "internal_error",
    }
)


class MutationError(RuntimeError):
    def __init__(
        self, code: str, message: str, details: dict[str, Any] | None = None
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details or {}
