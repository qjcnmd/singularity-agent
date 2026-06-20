from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from pydantic import BaseModel

from miniharness.interaction.models import InteractionMode
from miniharness.policy.config import ApprovalMode, PolicyConfig, SecurityMode

if TYPE_CHECKING:
    from miniharness.model.config import ModelRuntimeConfig


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
                "MINIHARNESS_API_KEY",
            )
            if not os.getenv(name)
        ]
        if base_url is None and not os.getenv("MINIHARNESS_BASE_URL"):
            missing.append("MINIHARNESS_BASE_URL")
        if model is None and not os.getenv("MINIHARNESS_MODEL"):
            missing.append("MINIHARNESS_MODEL")
        if missing:
            raise RuntimeError(
                "Missing required environment variables: " + ", ".join(missing)
            )

        return cls(
            base_url=base_url or os.environ["MINIHARNESS_BASE_URL"],
            api_key=os.environ["MINIHARNESS_API_KEY"],
            model=model or os.environ["MINIHARNESS_MODEL"],
        )


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

    @classmethod
    def from_cli(
        cls,
        *,
        project_root: Path | str,
        max_turns: int = 8,
        profile: str | None = None,
        approval_mode: ApprovalMode | str = ApprovalMode.AUTO_SAFE,
        security_mode: SecurityMode | str = SecurityMode.STRICT,
        interaction_mode: InteractionMode | str = InteractionMode.INTERACTIVE,
        strict: bool = False,
        dry_run: bool = False,
        trace_dir: Path | str | None = None,
        context_db: Path | str | None = None,
        model: str | None = None,
        base_url: str | None = None,
        raw_artifacts: bool = False,
        resume_session: str | None = None,
        project_index_enabled: bool = True,
        project_index_db: Path | str | None = None,
        project_index_build_on_boot: bool = True,
        project_index_max_files: int = 20_000,
        project_index_max_file_size: int = 1_000_000,
        project_index_max_total_bytes: int = 50_000_000,
    ) -> "ProductionRuntimeConfig":
        return cls(
            project_root=Path(project_root).expanduser().resolve(strict=False),
            max_turns=max_turns,
            profile=profile,
            approval_mode=_approval_mode(approval_mode),
            security_mode=_security_mode(security_mode),
            interaction_mode=_interaction_mode(interaction_mode),
            strict=strict,
            dry_run=dry_run,
            trace_dir=Path(trace_dir).expanduser() if trace_dir is not None else None,
            context_db=Path(context_db).expanduser() if context_db is not None else None,
            model=model,
            base_url=base_url,
            raw_artifacts=raw_artifacts,
            resume_session=resume_session,
            project_index_enabled=project_index_enabled,
            project_index_db=Path(project_index_db).expanduser() if project_index_db is not None else None,
            project_index_build_on_boot=project_index_build_on_boot,
            project_index_max_files=project_index_max_files,
            project_index_max_file_size=project_index_max_file_size,
            project_index_max_total_bytes=project_index_max_total_bytes,
        )

    def to_policy_config(self) -> PolicyConfig:
        return PolicyConfig(
            workspace_root=self.project_root,
            approval_mode=self.approval_mode,
            security_mode=self.security_mode,
        )

    def to_model_runtime_config(self) -> "ModelRuntimeConfig":
        from miniharness.model.config import ModelRuntimeConfig

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
        return self.project_index_db or (self.project_root / ".miniharness" / "index.sqlite")

    def to_project_index_config(self):
        from miniharness.code_index import ProjectIndexRuntimeConfig

        return ProjectIndexRuntimeConfig(
            enabled=self.project_index_enabled,
            db_path=self.project_index_db_path(),
            build_on_boot=self.project_index_build_on_boot,
            max_files=self.project_index_max_files,
            max_file_size=self.project_index_max_file_size,
            max_total_bytes=self.project_index_max_total_bytes,
        )


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
