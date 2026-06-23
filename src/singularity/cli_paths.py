from __future__ import annotations

from pathlib import Path


def resolve_project_root(project_root: Path | str | None = None) -> Path:
    return Path(project_root or Path.cwd()).expanduser().resolve(strict=False)
