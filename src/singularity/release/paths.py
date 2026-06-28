from __future__ import annotations

import os
from dataclasses import asdict, dataclass
from enum import StrEnum
from pathlib import Path

APP_NAME = "singularity"


class UserDataMode(StrEnum):
    USER = "user"
    DEVELOPMENT = "development"
    PORTABLE = "portable"


@dataclass(frozen=True)
class UserDataPaths:
    mode: UserDataMode
    root: Path
    config_dir: Path
    state_dir: Path
    cache_dir: Path
    logs_dir: Path
    traces_dir: Path
    memory_dir: Path
    eval_dir: Path
    backups_dir: Path
    tmp_dir: Path

    @property
    def config_file(self) -> Path:
        return self.config_dir / "singularity.json"

    @property
    def manifest_file(self) -> Path:
        return self.state_dir / "installation-manifest.json"

    def directories(self) -> tuple[Path, ...]:
        return (
            self.config_dir,
            self.state_dir,
            self.cache_dir,
            self.logs_dir,
            self.traces_dir,
            self.memory_dir,
            self.eval_dir,
            self.backups_dir,
            self.tmp_dir,
        )

    def to_dict(self) -> dict[str, str]:
        payload = asdict(self)
        payload["mode"] = self.mode.value
        for key, value in list(payload.items()):
            if isinstance(value, Path):
                payload[key] = str(value)
        return payload


def resolve_user_data_paths(
    *,
    mode: UserDataMode | str | None = None,
    home: Path | str | None = None,
    project_root: Path | str | None = None,
) -> UserDataPaths:
    resolved_mode = _mode(mode)
    env_home = os.getenv("SINGULARITY_HOME")
    if home is not None:
        root = Path(home).expanduser()
    elif env_home:
        root = Path(env_home).expanduser()
    elif resolved_mode == UserDataMode.DEVELOPMENT or resolved_mode == UserDataMode.PORTABLE:
        root = Path(project_root or Path.cwd()).expanduser() / ".singularity"
    else:
        root = _user_data_root()
    root = root.resolve(strict=False)
    return UserDataPaths(
        mode=resolved_mode,
        root=root,
        config_dir=root / "config",
        state_dir=root / "state",
        cache_dir=root / "cache",
        logs_dir=root / "logs",
        traces_dir=root / "traces",
        memory_dir=root / "memory",
        eval_dir=root / "eval",
        backups_dir=root / "backups",
        tmp_dir=root / "tmp",
    )


def _mode(value: UserDataMode | str | None) -> UserDataMode:
    raw = value or os.getenv("SINGULARITY_MODE") or UserDataMode.USER.value
    try:
        return UserDataMode(str(raw).lower())
    except ValueError as exc:
        raise ValueError("SINGULARITY_MODE must be user, development, or portable.") from exc


def _user_data_root() -> Path:
    try:
        import platformdirs
    except ModuleNotFoundError:
        if os.name == "nt":
            base = Path(os.getenv("LOCALAPPDATA") or Path.home() / "AppData" / "Local")
        elif sys_platform() == "darwin":
            base = Path.home() / "Library" / "Application Support"
        else:
            base = Path(os.getenv("XDG_DATA_HOME") or Path.home() / ".local" / "share")
        return base / APP_NAME
    try:
        return Path(platformdirs.user_data_dir(APP_NAME, appauthor=False))
    except TypeError:
        return Path(platformdirs.user_data_dir(APP_NAME))


def sys_platform() -> str:
    import sys

    return sys.platform
