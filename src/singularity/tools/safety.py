from __future__ import annotations

import re
from pathlib import Path

from singularity.observability.redaction import TraceRedactor


SENSITIVE_NAME_RE = re.compile(
    r"(^\.env(?:\..*)?$|^id_rsa$|^id_dsa$|^id_ecdsa$|^id_ed25519$|"
    r".*\.(?:pem|key|p12|pfx)$|.*(?:credential|credentials|token|secret|api[_-]?key).*)",
    re.IGNORECASE,
)

SENSITIVE_DIRS = {
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Roaming/Mozilla/Firefox/Profiles",
}


class FileSensitivityClassifier:
    def __init__(self, workspace_root: Path) -> None:
        self.workspace_root = workspace_root.resolve(strict=False)

    def classify_path(self, path: Path) -> str:
        try:
            relative = path.resolve(strict=False).relative_to(self.workspace_root)
        except ValueError:
            return "secret"
        parts = relative.parts
        lowered_parts = [part.lower() for part in parts]
        joined = "/".join(parts)
        if any(part in {".ssh", ".gnupg", ".aws", ".azure"} for part in lowered_parts):
            return "secret"
        if any(joined.lower().startswith(item.lower()) for item in SENSITIVE_DIRS):
            return "secret"
        if any(SENSITIVE_NAME_RE.match(part) for part in parts):
            return "secret"
        return "workspace"

    def is_sensitive(self, path: Path) -> bool:
        return self.classify_path(path) in {"sensitive", "secret"}


_REDACTOR = TraceRedactor()


def redact_secret_text(text: str) -> str:
    return _REDACTOR.redact_text(text)
