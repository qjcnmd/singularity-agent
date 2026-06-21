from __future__ import annotations

import importlib.metadata
import importlib.util
import platform
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from singularity import __version__
from singularity.release.paths import RuntimePaths, resolve_runtime_paths


OPTIONAL_FEATURE_MODULES = {
    "eval": {"yaml": "PyYAML"},
    "sandbox": {},
    "devtools": {"tiktoken": "tiktoken"},
}
PACKAGE_DISTRIBUTION_NAME = "singularity-agent"


@dataclass(frozen=True)
class VersionInfo:
    version: str
    python_version: str
    platform: str
    install_path: str
    installed_package: bool
    config_dir: str
    runtime_dir: str
    optional_features: dict[str, dict[str, Any]] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "version": self.version,
            "python_version": self.python_version,
            "platform": self.platform,
            "install_path": self.install_path,
            "installed_package": self.installed_package,
            "config_dir": self.config_dir,
            "runtime_dir": self.runtime_dir,
            "optional_features": self.optional_features,
        }


def package_version() -> str:
    source_version = _pyproject_version()
    try:
        metadata_version = importlib.metadata.version(PACKAGE_DISTRIBUTION_NAME)
    except importlib.metadata.PackageNotFoundError:
        return source_version or __version__
    if source_version and _source_root().joinpath("src", "singularity").exists():
        return source_version
    return metadata_version


def version_info(paths: RuntimePaths | None = None) -> VersionInfo:
    runtime_paths = paths or resolve_runtime_paths()
    install_path = _install_path()
    return VersionInfo(
        version=package_version(),
        python_version=sys.version.split()[0],
        platform=platform.platform(),
        install_path=str(install_path),
        installed_package=_installed_package(),
        config_dir=str(runtime_paths.config_dir),
        runtime_dir=str(runtime_paths.root),
        optional_features=optional_feature_status(),
    )


def optional_feature_status() -> dict[str, dict[str, Any]]:
    statuses: dict[str, dict[str, Any]] = {}
    for feature, modules in OPTIONAL_FEATURE_MODULES.items():
        missing = [
            package_name
            for module_name, package_name in modules.items()
            if importlib.util.find_spec(module_name) is None
        ]
        statuses[feature] = {
            "available": not missing,
            "missing": missing,
        }
    return statuses


def requires_python() -> str:
    try:
        value = importlib.metadata.metadata(PACKAGE_DISTRIBUTION_NAME).get("Requires-Python")
    except importlib.metadata.PackageNotFoundError:
        value = None
    if value:
        return value
    pyproject = _source_root() / "pyproject.toml"
    if pyproject.exists():
        try:
            import tomllib

            return str(tomllib.loads(pyproject.read_text(encoding="utf-8"))["project"]["requires-python"])
        except Exception:
            pass
    return ">=3.11"


def _pyproject_version() -> str | None:
    pyproject = _source_root() / "pyproject.toml"
    if not pyproject.exists():
        return None
    try:
        import tomllib

        return str(tomllib.loads(pyproject.read_text(encoding="utf-8"))["project"]["version"])
    except Exception:
        return None


def _installed_package() -> bool:
    try:
        importlib.metadata.distribution(PACKAGE_DISTRIBUTION_NAME)
    except importlib.metadata.PackageNotFoundError:
        return False
    return True


def _install_path() -> Path:
    spec = importlib.util.find_spec("singularity")
    if spec and spec.origin:
        return Path(spec.origin).resolve(strict=False).parent
    return _source_root()


def _source_root() -> Path:
    return Path(__file__).resolve(strict=False).parents[3]
