"""Release/runtime support for installed Singularity CLI usage."""

from singularity.release.metadata import version_info
from singularity.release.paths import RuntimeMode, RuntimePaths, resolve_runtime_paths

__all__ = [
    "RuntimeMode",
    "RuntimePaths",
    "resolve_runtime_paths",
    "version_info",
]
