from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable

from pydantic import BaseModel

from singularity.interaction.models import InteractionMode
from singularity.policy.config import ApprovalMode, PolicyConfig, SecurityMode

if TYPE_CHECKING:
    from singularity.model.config import ModelRuntimeConfig


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
    ) -> "Settings":
        missing = [
            name
            for name in (
                "SINGULARITY_API_KEY",
            )
            if not os.getenv(name)
        ]
        if base_url is None and not os.getenv("SINGULARITY_BASE_URL"):
            missing.append("SINGULARITY_BASE_URL")
        if model is None and not os.getenv("SINGULARITY_MODEL"):
            missing.append("SINGULARITY_MODEL")
        if missing:
            raise RuntimeError(
                "Missing required environment variables: " + ", ".join(missing)
            )

        return cls(
            base_url=base_url or os.environ["SINGULARITY_BASE_URL"],
            api_key=os.environ["SINGULARITY_API_KEY"],
            model=model or os.environ["SINGULARITY_MODEL"],
        )


_CONFIG_DEFAULTS: dict[str, Any] = {
    "max_turns": 8,
    "profile": None,
    "approval_mode": ApprovalMode.AUTO_SAFE,
    "security_mode": SecurityMode.STRICT,
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
class ProductionRuntimeConfig:
    project_root: Path
    max_turns: int = 8
    profile: str | None = None
    approval_mode: ApprovalMode = ApprovalMode.AUTO_SAFE
    security_mode: SecurityMode = SecurityMode.STRICT
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
        approval_mode: ApprovalMode | str | None = None,
        security_mode: SecurityMode | str | None = None,
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
        cli_overrides: set[str] | None = None,
    ) -> "ProductionRuntimeConfig":
        root = Path(project_root).expanduser().resolve(strict=False)
        resolved_config_file = (
            Path(config_file).expanduser()
            if config_file is not None
            else root / ".singularity" / "config.toml"
        )
        config_values = _flatten_config(_read_config_file(resolved_config_file))
        config_source = f"config:{_config_file_handle(resolved_config_file, root)}"
        cli_values = {
            "max_turns": max_turns,
            "profile": profile,
            "approval_mode": approval_mode,
            "security_mode": security_mode,
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
            "approval_mode": _approval_mode,
            "security_mode": _security_mode,
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
            "approval_mode": "SINGULARITY_APPROVAL_MODE",
            "security_mode": "SINGULARITY_SECURITY_MODE",
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
        for name, default in _CONFIG_DEFAULTS.items():
            raw_value, source = _resolve_config_value(
                name=name,
                cli_value=cli_values[name],
                cli_overrides=cli_overrides,
                env_name=env_names[name],
                config_values=config_values,
                config_source=config_source,
                default=default,
            )
            values[name] = converters[name](raw_value)
            sources[name] = source
        return cls(
            project_root=root,
            max_turns=values["max_turns"],
            profile=values["profile"],
            approval_mode=values["approval_mode"],
            security_mode=values["security_mode"],
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
            config_sources=sources,
        )

    def to_policy_config(self) -> PolicyConfig:
        return PolicyConfig(
            workspace_root=self.project_root,
            approval_mode=self.approval_mode,
            security_mode=self.security_mode,
        )

    def to_model_runtime_config(self) -> "ModelRuntimeConfig":
        from singularity.model.config import ModelRuntimeConfig

        config = ModelRuntimeConfig.from_env(
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
        from singularity.code_index import ProjectIndexRuntimeConfig

        return ProjectIndexRuntimeConfig(
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
            "approval_mode": self.approval_mode.value,
            "security_mode": self.security_mode.value,
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
        return {
            **effective["values"],
            "sources": effective["sources"],
            "config_file": effective["config_file"],
        }


def _read_config_file(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    return data if isinstance(data, dict) else {}


def _flatten_config(data: dict[str, Any]) -> dict[str, Any]:
    flattened = dict(data)
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
        cli_value is not None
        if cli_overrides is None
        else name in cli_overrides and cli_value is not None
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
    try:
        return path.resolve(strict=False).relative_to(root.resolve(strict=False)).as_posix()
    except ValueError:
        return str(path)


def _approval_mode(value: ApprovalMode | str) -> ApprovalMode:
    if isinstance(value, ApprovalMode):
        return value
    try:
        return ApprovalMode[str(value).upper()]
    except KeyError:
        return ApprovalMode(str(value))


def _interaction_mode(value: InteractionMode | str) -> InteractionMode:
    if isinstance(value, InteractionMode):
        return value
    try:
        return InteractionMode[str(value).upper()]
    except KeyError:
        return InteractionMode(str(value))


def _security_mode(value: SecurityMode | str) -> SecurityMode:
    if isinstance(value, SecurityMode):
        return value
    try:
        return SecurityMode[str(value).upper()]
    except KeyError:
        return SecurityMode(str(value))
