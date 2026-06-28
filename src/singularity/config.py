from __future__ import annotations

import os
import tomllib
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

from pydantic import BaseModel

from singularity.interaction.models import InteractionMode
from singularity.policy.config import PolicyConfig
from singularity.policy.permissions import (
    ApprovalPolicy,
    NetworkAccess,
    PermissionProfile,
    PermissionProfileName,
)

if TYPE_CHECKING:
    from singularity.model.config import ModelRunnerConfig


@dataclass(frozen=True)
class EnvFileLoadResult:
    found: bool = False
    loaded: bool = False


class Settings(BaseModel):
    base_url: str
    api_key: str
    model: str

    @classmethod
    def from_env(
        cls,
        *,
        base_url: str | None = None,
        model: str | None = None,
    ) -> Settings:
        missing = [name for name in ("SINGULARITY_API_KEY",) if not os.getenv(name)]
        if base_url is None and not os.getenv("SINGULARITY_BASE_URL"):
            missing.append("SINGULARITY_BASE_URL")
        if model is None and not os.getenv("SINGULARITY_MODEL"):
            missing.append("SINGULARITY_MODEL")
        if missing:
            raise RuntimeError("Missing required environment variables: " + ", ".join(missing))

        return cls(
            base_url=base_url or os.environ["SINGULARITY_BASE_URL"],
            api_key=os.environ["SINGULARITY_API_KEY"],
            model=model or os.environ["SINGULARITY_MODEL"],
        )


BASE_DEFAULT_MAX_TURNS = 8
MEDIUM_TASK_DEFAULT_MAX_TURNS = 12
LONG_TASK_DEFAULT_MAX_TURNS = 16

_LONG_TASK_MARKERS = (
    "refactor",
    "architecture",
    "benchmark",
    "e2e",
    "end-to-end",
    "implement",
    "integration",
    "phase",
    "roadmap",
    "complete",
    "report",
    "commit",
    "push",
    "merge",
    "重构",
    "架构",
    "基准",
    "端到端",
    "实现",
    "集成",
    "阶段",
    "清单",
    "全部",
    "完成",
    "报告",
    "提交",
    "合并",
    "修复",
    "测试",
)


def adaptive_default_max_turns(goal: str | None) -> int:
    text = (goal or "").strip()
    if not text:
        return BASE_DEFAULT_MAX_TURNS

    lowered = text.lower()
    marker_hits = sum(1 for marker in _LONG_TASK_MARKERS if marker in lowered)
    char_count = len(text)

    if char_count >= 240 or marker_hits >= 5 or text.count("\n") >= 2:
        return LONG_TASK_DEFAULT_MAX_TURNS
    if char_count >= 120 or marker_hits >= 2:
        return MEDIUM_TASK_DEFAULT_MAX_TURNS
    return BASE_DEFAULT_MAX_TURNS


_CONFIG_DEFAULTS: dict[str, Any] = {
    "max_turns": BASE_DEFAULT_MAX_TURNS,
    "profile": None,
    "permission_profile": PermissionProfileName.WORKSPACE_WRITE,
    "approval_policy": ApprovalPolicy.ON_REQUEST,
    "network_access": NetworkAccess.DENIED,
    "additional_writable_directories": (),
    "protected_paths": (),
    "windows_sandbox": "elevated",
    "interaction_mode": InteractionMode.INTERACTIVE,
    "strict": False,
    "dry_run": False,
    "trace_dir": None,
    "context_db": None,
    "model": None,
    "base_url": None,
    "raw_artifacts": False,
    "resume_session": None,
    "project_index_enabled": True,
    "project_index_db": None,
    "project_index_build_on_boot": True,
    "project_index_max_files": 20_000,
    "project_index_max_file_size": 1_000_000,
    "project_index_max_total_bytes": 50_000_000,
}


@dataclass(frozen=True)
class ProductionConfig:
    project_root: Path
    max_turns: int = BASE_DEFAULT_MAX_TURNS
    profile: str | None = None
    permission_profile: PermissionProfileName = PermissionProfileName.WORKSPACE_WRITE
    approval_policy: ApprovalPolicy = ApprovalPolicy.ON_REQUEST
    network_access: NetworkAccess = NetworkAccess.DENIED
    additional_writable_directories: tuple[Path, ...] = ()
    protected_paths: tuple[str, ...] = ()
    windows_sandbox: str = "elevated"
    interaction_mode: InteractionMode = InteractionMode.INTERACTIVE
    strict: bool = False
    dry_run: bool = False
    trace_dir: Path | None = None
    context_db: Path | None = None
    model: str | None = None
    base_url: str | None = None
    raw_artifacts: bool = False
    resume_session: str | None = None
    project_index_enabled: bool = True
    project_index_db: Path | None = None
    project_index_build_on_boot: bool = True
    project_index_max_files: int = 20_000
    project_index_max_file_size: int = 1_000_000
    project_index_max_total_bytes: int = 50_000_000
    config_file: Path | None = None
    config_sources: dict[str, str] = field(default_factory=dict)

    @classmethod
    def from_cli(
        cls,
        *,
        project_root: Path | str,
        max_turns: int | None = None,
        profile: str | None = None,
        permission_profile: PermissionProfileName | str | None = None,
        approval_policy: ApprovalPolicy | str | None = None,
        network_access: NetworkAccess | str | None = None,
        additional_writable_directories: list[Path | str] | tuple[Path | str, ...] | None = None,
        protected_paths: list[str] | tuple[str, ...] | None = None,
        windows_sandbox: str | None = None,
        interaction_mode: InteractionMode | str | None = None,
        strict: bool | None = None,
        dry_run: bool | None = None,
        trace_dir: Path | str | None = None,
        context_db: Path | str | None = None,
        model: str | None = None,
        base_url: str | None = None,
        raw_artifacts: bool | None = None,
        resume_session: str | None = None,
        project_index_enabled: bool | None = None,
        project_index_db: Path | str | None = None,
        project_index_build_on_boot: bool | None = None,
        project_index_max_files: int | None = None,
        project_index_max_file_size: int | None = None,
        project_index_max_total_bytes: int | None = None,
        config_file: Path | str | None = None,
        env_root: Path | str | None = None,
        cli_overrides: set[str] | None = None,
        default_max_turns: int | None = None,
    ) -> ProductionConfig:
        root = Path(project_root).expanduser().resolve(strict=False)
        env_base = Path(env_root).expanduser().resolve(strict=False) if env_root is not None else root
        env_file = env_base / ".env"
        env_load = _load_project_env(env_file)
        env_file_source = f"project:{_display_config_path(env_file, root)}" if env_load.found else None
        resolved_config_file = (
            Path(config_file).expanduser() if config_file is not None else root / ".singularity" / "config.toml"
        )
        config_values = _flatten_config(_read_config_file(resolved_config_file))
        config_source = f"config:{_config_file_handle(resolved_config_file, root)}"
        cli_values = {
            "max_turns": max_turns,
            "profile": profile,
            "permission_profile": permission_profile,
            "approval_policy": approval_policy,
            "network_access": network_access,
            "additional_writable_directories": additional_writable_directories,
            "protected_paths": protected_paths,
            "windows_sandbox": windows_sandbox,
            "interaction_mode": interaction_mode,
            "strict": strict,
            "dry_run": dry_run,
            "trace_dir": trace_dir,
            "context_db": context_db,
            "model": model,
            "base_url": base_url,
            "raw_artifacts": raw_artifacts,
            "resume_session": resume_session,
            "project_index_enabled": project_index_enabled,
            "project_index_db": project_index_db,
            "project_index_build_on_boot": project_index_build_on_boot,
            "project_index_max_files": project_index_max_files,
            "project_index_max_file_size": project_index_max_file_size,
            "project_index_max_total_bytes": project_index_max_total_bytes,
        }
        values: dict[str, Any] = {}
        sources: dict[str, str] = {}
        converters: dict[str, Callable[[Any], Any]] = {
            "max_turns": int,
            "profile": _optional_str,
            "permission_profile": _permission_profile_name,
            "approval_policy": _approval_policy,
            "network_access": _network_access,
            "additional_writable_directories": lambda value: _path_tuple(value, root=root),
            "protected_paths": _string_tuple,
            "windows_sandbox": _windows_sandbox,
            "interaction_mode": _interaction_mode,
            "strict": _bool_value,
            "dry_run": _bool_value,
            "trace_dir": _optional_path,
            "context_db": _optional_path,
            "model": _optional_str,
            "base_url": _optional_str,
            "raw_artifacts": _bool_value,
            "resume_session": _optional_str,
            "project_index_enabled": _bool_value,
            "project_index_db": _optional_path,
            "project_index_build_on_boot": _bool_value,
            "project_index_max_files": int,
            "project_index_max_file_size": int,
            "project_index_max_total_bytes": int,
        }
        env_names = {
            "max_turns": "SINGULARITY_MAX_TURNS",
            "profile": "SINGULARITY_PROFILE",
            "permission_profile": "SINGULARITY_PERMISSION_PROFILE",
            "approval_policy": "SINGULARITY_APPROVAL_POLICY",
            "network_access": "SINGULARITY_NETWORK_ACCESS",
            "additional_writable_directories": "SINGULARITY_ADDITIONAL_WRITABLE_DIRECTORIES",
            "protected_paths": "SINGULARITY_PROTECTED_PATHS",
            "windows_sandbox": "SINGULARITY_WINDOWS_SANDBOX",
            "interaction_mode": "SINGULARITY_INTERACTION_MODE",
            "strict": "SINGULARITY_STRICT",
            "dry_run": "SINGULARITY_DRY_RUN",
            "trace_dir": "SINGULARITY_TRACE_DIR",
            "context_db": "SINGULARITY_CONTEXT_DB",
            "model": "SINGULARITY_MODEL",
            "base_url": "SINGULARITY_BASE_URL",
            "raw_artifacts": "SINGULARITY_RAW_ARTIFACTS",
            "resume_session": "SINGULARITY_RESUME_SESSION",
            "project_index_enabled": "SINGULARITY_PROJECT_INDEX_ENABLED",
            "project_index_db": "SINGULARITY_PROJECT_INDEX_DB",
            "project_index_build_on_boot": "SINGULARITY_PROJECT_INDEX_BUILD_ON_BOOT",
            "project_index_max_files": "SINGULARITY_PROJECT_INDEX_MAX_FILES",
            "project_index_max_file_size": "SINGULARITY_PROJECT_INDEX_MAX_FILE_SIZE",
            "project_index_max_total_bytes": "SINGULARITY_PROJECT_INDEX_MAX_TOTAL_BYTES",
        }
        defaults = dict(_CONFIG_DEFAULTS)
        default_sources: dict[str, str] = {}
        if default_max_turns is not None:
            defaults["max_turns"] = max(1, int(default_max_turns))
            default_sources["max_turns"] = "default:adaptive"
        if env_file_source:
            default_sources["env_file"] = env_file_source

        for name, default in defaults.items():
            raw_value, source = _resolve_config_value(
                name=name,
                cli_value=cli_values[name],
                cli_overrides=cli_overrides,
                env_name=env_names[name],
                config_values=config_values,
                config_source=config_source,
                default=default,
            )
            if source == "default" and name in default_sources:
                source = default_sources[name]
            values[name] = converters[name](raw_value)
            sources[name] = source
        return cls(
            project_root=root,
            max_turns=values["max_turns"],
            profile=values["profile"],
            permission_profile=values["permission_profile"],
            approval_policy=values["approval_policy"],
            network_access=values["network_access"],
            additional_writable_directories=values["additional_writable_directories"],
            protected_paths=values["protected_paths"],
            windows_sandbox=values["windows_sandbox"],
            interaction_mode=values["interaction_mode"],
            strict=values["strict"],
            dry_run=values["dry_run"],
            trace_dir=values["trace_dir"],
            context_db=values["context_db"],
            model=values["model"],
            base_url=values["base_url"],
            raw_artifacts=values["raw_artifacts"],
            resume_session=values["resume_session"],
            project_index_enabled=values["project_index_enabled"],
            project_index_db=values["project_index_db"],
            project_index_build_on_boot=values["project_index_build_on_boot"],
            project_index_max_files=values["project_index_max_files"],
            project_index_max_file_size=values["project_index_max_file_size"],
            project_index_max_total_bytes=values["project_index_max_total_bytes"],
            config_file=resolved_config_file if resolved_config_file.exists() else None,
            config_sources={
                **sources,
                **({"env_file": env_file_source} if env_file_source else {}),
            },
        )

    def to_policy_config(self) -> PolicyConfig:
        return PolicyConfig(
            workspace_root=self.project_root,
            permission_profile=self.to_permission_profile(),
        )

    def to_permission_profile(self) -> PermissionProfile:
        return PermissionProfile(
            profile=self.permission_profile,
            workspace_roots=(self.project_root,),
            additional_writable_directories=self.additional_writable_directories,
            network_access=self.network_access,
            approval_policy=self.approval_policy,
            protected_paths=self.protected_paths,
        )

    def to_model_runner_config(self) -> ModelRunnerConfig:
        from singularity.model.config import ModelRunnerConfig

        config = ModelRunnerConfig.from_env(
            base_url=self.base_url,
            model=self.model,
            store_raw_responses=self.raw_artifacts,
        )
        return config

    def to_settings(self) -> Settings:
        return Settings.from_env(base_url=self.base_url, model=self.model)

    def context_db_path(self, run_dir: Path) -> Path:
        return self.context_db or (run_dir / "context.sqlite3")

    def project_index_db_path(self) -> Path:
        return self.project_index_db or (self.project_root / ".singularity" / "index.sqlite")

    def to_project_index_config(self):
        from singularity.code_index import ProjectIndexConfig

        return ProjectIndexConfig(
            enabled=self.project_index_enabled,
            db_path=self.project_index_db_path(),
            build_on_boot=self.project_index_build_on_boot,
            max_files=self.project_index_max_files,
            max_file_size=self.project_index_max_file_size,
            max_total_bytes=self.project_index_max_total_bytes,
        )

    def effective_config(self) -> dict[str, Any]:
        values = {
            "project_root": str(self.project_root),
            "max_turns": self.max_turns,
            "profile": self.profile,
            "permission_profile": self.permission_profile.value,
            "approval_policy": self.approval_policy.value,
            "network_access": self.network_access.value,
            "additional_writable_directories": [str(path) for path in self.additional_writable_directories],
            "protected_paths": list(self.protected_paths),
            "windows_sandbox": self.windows_sandbox,
            "interaction_mode": self.interaction_mode.value,
            "strict": self.strict,
            "dry_run": self.dry_run,
            "trace_dir": str(self.trace_dir) if self.trace_dir else None,
            "context_db": str(self.context_db) if self.context_db else None,
            "model": self.model,
            "base_url": self.base_url,
            "raw_artifacts": self.raw_artifacts,
            "resume_session": self.resume_session,
            "project_index_enabled": self.project_index_enabled,
            "project_index_db": str(self.project_index_db_path()),
            "project_index_build_on_boot": self.project_index_build_on_boot,
            "project_index_max_files": self.project_index_max_files,
            "project_index_max_file_size": self.project_index_max_file_size,
            "project_index_max_total_bytes": self.project_index_max_total_bytes,
        }
        return {
            "values": values,
            "sources": dict(self.config_sources),
            "config_file": _config_file_handle(self.config_file, self.project_root),
        }

    def final_report_config_summary(self) -> dict[str, Any]:
        effective = self.effective_config()
        values = dict(effective["values"])
        values.pop("protected_paths", None)
        values.pop("additional_writable_directories", None)
        permission = self.to_permission_profile().summary().to_dict()
        permission["writable_roots"] = [
            _report_path_handle(Path(path), self.project_root)
            if path != "*"
            else "*"
            for path in permission["writable_roots"]
        ]
        return {
            **values,
            "permission": permission,
            "sources": effective["sources"],
            "config_file": effective["config_file"],
        }


def _report_path_handle(path: Path, project_root: Path) -> str:
    resolved = path.resolve(strict=False)
    try:
        relative = resolved.relative_to(project_root.resolve(strict=False))
        return relative.as_posix() or "."
    except ValueError:
        return f"additional-dir:{resolved.name}"


def _read_config_file(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    return data if isinstance(data, dict) else {}


def _load_project_env(path: Path) -> EnvFileLoadResult:
    if not path.exists():
        return EnvFileLoadResult()
    loaded = False
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return EnvFileLoadResult()
    for line in lines:
        parsed = _parse_env_line(line)
        if parsed is None:
            continue
        name, value = parsed
        if name not in os.environ:
            os.environ[name] = value
            loaded = True
    return EnvFileLoadResult(found=True, loaded=loaded)


def _parse_env_line(line: str) -> tuple[str, str] | None:
    text = line.strip()
    if not text or text.startswith("#"):
        return None
    if text.lower().startswith("export "):
        text = text[7:].lstrip()
    if "=" not in text:
        return None
    name, value = text.split("=", 1)
    name = name.strip()
    if not name or not name.replace("_", "").isalnum() or name[0].isdigit():
        return None
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        value = value[1:-1]
    return name, value


def _flatten_config(data: dict[str, Any]) -> dict[str, Any]:
    flattened = dict(data)
    permissions = flattened.pop("permissions", None)
    if isinstance(permissions, dict):
        mapping = {
            "profile": "permission_profile",
            "approval_policy": "approval_policy",
            "network_access": "network_access",
            "additional_writable_directories": "additional_writable_directories",
            "protected_paths": "protected_paths",
        }
        for source, target in mapping.items():
            if source in permissions:
                flattened[target] = permissions[source]
        windows = permissions.get("windows")
        if isinstance(windows, dict) and "implementation" in windows:
            flattened["windows_sandbox"] = windows["implementation"]
    project_index = flattened.pop("project_index", None)
    if isinstance(project_index, dict):
        mapping = {
            "enabled": "project_index_enabled",
            "db": "project_index_db",
            "db_path": "project_index_db",
            "build_on_boot": "project_index_build_on_boot",
            "max_files": "project_index_max_files",
            "max_file_size": "project_index_max_file_size",
            "max_total_bytes": "project_index_max_total_bytes",
        }
        for source, target in mapping.items():
            if source in project_index:
                flattened[target] = project_index[source]
    return flattened


def _resolve_config_value(
    *,
    name: str,
    cli_value: Any,
    cli_overrides: set[str] | None,
    env_name: str,
    config_values: dict[str, Any],
    config_source: str,
    default: Any,
) -> tuple[Any, str]:
    cli_is_explicit = (
        cli_value is not None if cli_overrides is None else name in cli_overrides and cli_value is not None
    )
    if cli_is_explicit:
        return cli_value, "cli"
    if env_name in os.environ:
        return os.environ[env_name], f"env:{env_name}"
    if name in config_values:
        return config_values[name], config_source
    return default, "default"


def _bool_value(value: Any) -> bool:
    if isinstance(value, bool):
        return value
    text = str(value).strip().lower()
    if text in {"1", "true", "yes", "on"}:
        return True
    if text in {"0", "false", "no", "off"}:
        return False
    raise ValueError(f"Invalid boolean value: {value!r}")


def _optional_str(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value)
    return text if text else None


def _optional_path(value: Any) -> Path | None:
    if value is None:
        return None
    return Path(value).expanduser()


def _config_file_handle(path: Path | None, root: Path) -> str | None:
    if path is None:
        return None
    return _display_config_path(path, root)


def _display_config_path(path: Path, root: Path) -> str:
    try:
        return path.resolve(strict=False).relative_to(root.resolve(strict=False)).as_posix()
    except ValueError:
        return str(path)


def _permission_profile_name(
    value: PermissionProfileName | str,
) -> PermissionProfileName:
    if isinstance(value, PermissionProfileName):
        return value
    return PermissionProfileName(str(value).strip().lower())


def _approval_policy(value: ApprovalPolicy | str) -> ApprovalPolicy:
    if isinstance(value, ApprovalPolicy):
        return value
    return ApprovalPolicy(str(value).strip().lower())


def _network_access(value: NetworkAccess | str) -> NetworkAccess:
    if isinstance(value, NetworkAccess):
        return value
    return NetworkAccess(str(value).strip().lower())


def _path_tuple(value: Any, *, root: Path) -> tuple[Path, ...]:
    if value is None:
        return ()
    if isinstance(value, str):
        items = [item for item in value.split(os.pathsep) if item]
    elif isinstance(value, (list, tuple)):
        items = list(value)
    else:
        raise ValueError("additional_writable_directories must be a list of paths")
    normalized: list[Path] = []
    for item in items:
        path = Path(item).expanduser()
        if not path.is_absolute():
            path = root / path
        resolved = path.resolve(strict=False)
        if resolved not in normalized:
            normalized.append(resolved)
    return tuple(normalized)


def _string_tuple(value: Any) -> tuple[str, ...]:
    if value is None:
        return ()
    if isinstance(value, str):
        items = value.split(os.pathsep)
    elif isinstance(value, (list, tuple)):
        items = value
    else:
        raise ValueError("protected_paths must be a list of path patterns")
    return tuple(dict.fromkeys(str(item).strip() for item in items if str(item).strip()))


def _windows_sandbox(value: Any) -> str:
    implementation = str(value).strip().lower()
    if implementation != "elevated":
        raise ValueError("windows sandbox implementation must be 'elevated'")
    return implementation


def _interaction_mode(value: InteractionMode | str) -> InteractionMode:
    if isinstance(value, InteractionMode):
        return value
    try:
        return InteractionMode[str(value).upper()]
    except KeyError:
        return InteractionMode(str(value))
