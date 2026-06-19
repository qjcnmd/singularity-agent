from __future__ import annotations

import shutil
import subprocess
from pathlib import Path


class RgBackend:
    name = "rg"
    version = "1.0.0"

    def available(self) -> bool:
        return shutil.which("rg") is not None

    def search(self, workspace_root: Path, query: str, *, limit: int = 100) -> list[dict[str, object]]:
        if not self.available() or not query:
            return []
        completed = subprocess.run(
            ["rg", "--line-number", "--no-heading", "--", query],
            cwd=workspace_root,
            check=False,
            text=True,
            capture_output=True,
            timeout=5,
        )
        matches = []
        for line in completed.stdout.splitlines()[:limit]:
            parts = line.split(":", 2)
            if len(parts) < 3:
                continue
            matches.append({"path": parts[0], "line": int(parts[1]), "preview": parts[2][:240]})
        return matches
