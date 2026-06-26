from __future__ import annotations

import re
from pathlib import Path
from typing import Any

SUMMARY_LIMIT = 700
TAIL_LIMIT = 400
MIN_REPAIR_CONFIDENCE = 0.45
FAILURE_CATEGORY_PATTERN = re.compile(r"^[a-z][a-z0-9_]{2,80}$")


def _paths_from_failure_source(source: dict[str, Any]) -> list[str]:
    paths: list[str] = []
    _append_unique(paths, source.get("target_file"))
    for key in ("affected_files", "changed_files", "suspect_files"):
        for path in source.get(key) or []:
            _append_unique(paths, path)
    evidence = source.get("evidence")
    if isinstance(evidence, dict):
        for parsed in evidence.get("parsed_failures") or []:
            if isinstance(parsed, dict):
                _append_unique(paths, parsed.get("file"))
        for path in evidence.get("sandbox_changed_files") or []:
            _append_unique(paths, path)
    for hint in source.get("repair_hints") or []:
        if isinstance(hint, dict):
            _append_unique(paths, hint.get("target_file"))
    for status in source.get("check_status") or []:
        if isinstance(status, dict):
            _append_unique(paths, status.get("file"))
    return paths


def _normalize_workspace_path(path: Any, *, workspace_root: str) -> str | None:
    text = str(path or "").strip().replace("\\", "/")
    if not text:
        return None
    if text.startswith("workspace:"):
        text = text.removeprefix("workspace:")
    if text.startswith("file://"):
        text = text.removeprefix("file://")
    candidate = Path(text)
    if candidate.is_absolute():
        if not workspace_root:
            return None
        try:
            return candidate.resolve(strict=False).relative_to(Path(workspace_root).resolve(strict=False)).as_posix()
        except ValueError:
            return None
    normalized = Path(text).as_posix()
    if normalized.startswith("../") or normalized == ".." or "/../" in normalized or normalized.startswith("/"):
        return None
    return normalized


def _trim_dict(value: dict[str, Any]) -> dict[str, Any]:
    return {
        str(key): _limit(item, SUMMARY_LIMIT) if isinstance(item, str) else item
        for key, item in value.items()
        if key not in {"raw_output", "raw_stdout", "raw_stderr", "content"}
    }


def _strings(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    if isinstance(value, list | tuple | set):
        return [str(item) for item in value if item is not None]
    return [str(value)]


def _text(value: Any) -> str:
    return _limit(str(value or ""), SUMMARY_LIMIT)


def _confidence(value: Any) -> float:
    try:
        return max(0.0, min(1.0, float(value)))
    except (TypeError, ValueError):
        return 0.5


def _limit(value: Any, limit: int) -> str:
    text = str(value or "")
    return text if len(text) <= limit else text[:limit] + "...[truncated]"


def _append_unique(values: list[str], value: Any) -> None:
    if value is None:
        return
    text = str(value)
    if text and text not in values:
        values.append(text)
