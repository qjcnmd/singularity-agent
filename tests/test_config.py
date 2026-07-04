from __future__ import annotations

from singularity import config
from singularity.config import (
    BASE_DEFAULT_MAX_TURNS,
    LONG_TASK_DEFAULT_MAX_TURNS,
    MEDIUM_TASK_DEFAULT_MAX_TURNS,
    adaptive_default_max_turns,
)


def test_adaptive_default_max_turns_uses_named_complexity_thresholds() -> None:
    assert adaptive_default_max_turns("") == BASE_DEFAULT_MAX_TURNS
    assert adaptive_default_max_turns("x" * config.MEDIUM_TASK_CHAR_THRESHOLD) == MEDIUM_TASK_DEFAULT_MAX_TURNS
    assert adaptive_default_max_turns("x" * config.LONG_TASK_CHAR_THRESHOLD) == LONG_TASK_DEFAULT_MAX_TURNS
    assert adaptive_default_max_turns("refactor architecture benchmark implement commit") == LONG_TASK_DEFAULT_MAX_TURNS
