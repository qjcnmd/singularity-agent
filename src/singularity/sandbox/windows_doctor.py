from __future__ import annotations

from functools import lru_cache

import singularity.sandbox.windows as _windows


@lru_cache(maxsize=1)
def probe_windows_sandbox():
    return _windows._probe_windows_sandbox_uncached()
