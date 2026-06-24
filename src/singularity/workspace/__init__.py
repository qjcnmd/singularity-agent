from singularity.workspace.diff import DiffEngine, DiffHunk, FileDiff
from singularity.workspace.errors import MUTATION_ERROR_CODES, MutationError
from singularity.workspace.operations import (
    ApplyUnifiedDiff,
    CreateFile,
    DeleteFile,
    FormatFile,
    InsertAfter,
    InsertBefore,
    MoveFile,
    ReplaceRange,
    ReplaceText,
    UpdateJson,
    UpdateToml,
    UpdateYaml,
)
from singularity.workspace.pathing import (
    ResolvedWorkspacePath,
    WorkspacePathResolver,
    WorkspaceRoot,
)
from singularity.workspace.policy import FileClassifier, PolicyDecision, WorkspacePolicy
from singularity.workspace.mutation_manager import (
    ChangeSet,
    MutationResult,
    WorkspaceMutationManager,
    RollbackManager,
)
from singularity.workspace.snapshot import FileSnapshot, WorkspaceIndex

__all__ = [
    "ApplyUnifiedDiff",
    "ChangeSet",
    "CreateFile",
    "DeleteFile",
    "DiffEngine",
    "DiffHunk",
    "FileClassifier",
    "FileDiff",
    "FileSnapshot",
    "FormatFile",
    "InsertAfter",
    "InsertBefore",
    "MoveFile",
    "MUTATION_ERROR_CODES",
    "MutationError",
    "MutationResult",
    "WorkspaceMutationManager",
    "PolicyDecision",
    "ReplaceRange",
    "ReplaceText",
    "ResolvedWorkspacePath",
    "RollbackManager",
    "UpdateJson",
    "UpdateToml",
    "UpdateYaml",
    "WorkspaceIndex",
    "WorkspacePathResolver",
    "WorkspacePolicy",
    "WorkspaceRoot",
]
