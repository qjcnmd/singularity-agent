from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from pydantic import BaseModel

from miniharness.policy.config import ApprovalMode, PolicyConfig

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
    approval_mode: ApprovalMode = ApprovalMode.AUTO_SAFE
    strict: bool = False
    dry_run: bool = False
    trace_dir: Path | None = None
    context_db: Path | None = None
    model: str | None = None
    base_url: str | None = None
    raw_artifacts: bool = False
    resume_session: str | None = None

    @classmethod
    def from_cli(
        cls,
        *,
        project_root: Path | str,
        max_turns: int = 8,
        approval_mode: ApprovalMode | str = ApprovalMode.AUTO_SAFE,
        strict: bool = False,
        dry_run: bool = False,
        trace_dir: Path | str | None = None,
        context_db: Path | str | None = None,
        model: str | None = None,
        base_url: str | None = None,
        raw_artifacts: bool = False,
        resume_session: str | None = None,
    ) -> "ProductionRuntimeConfig":
        return cls(
            project_root=Path(project_root).expanduser().resolve(strict=False),
            max_turns=max_turns,
            approval_mode=_approval_mode(approval_mode),
            strict=strict,
            dry_run=dry_run,
            trace_dir=Path(trace_dir).expanduser() if trace_dir is not None else None,
            context_db=Path(context_db).expanduser() if context_db is not None else None,
            model=model,
            base_url=base_url,
            raw_artifacts=raw_artifacts,
            resume_session=resume_session,
        )

    def to_policy_config(self) -> PolicyConfig:
        return PolicyConfig(
            workspace_root=self.project_root,
            approval_mode=self.approval_mode,
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


def _approval_mode(value: ApprovalMode | str) -> ApprovalMode:
    if isinstance(value, ApprovalMode):
        return value
    try:
        return ApprovalMode[str(value).upper()]
    except KeyError:
        return ApprovalMode(str(value))
