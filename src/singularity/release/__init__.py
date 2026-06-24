"""Release and installation support for installed Singularity CLI usage."""

from singularity.release.metadata import version_info
from singularity.release.paths import UserDataMode, UserDataPaths, resolve_user_data_paths

__all__ = [
    "UserDataMode",
    "UserDataPaths",
    "resolve_user_data_paths",
    "version_info",
]
