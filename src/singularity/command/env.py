from __future__ import annotations

import os
from dataclasses import dataclass, field

from singularity.command.output import SecretRedactor
from singularity.command.policy import is_secret_env_name

DEFAULT_INHERITED_ENV = {
    "COMSPEC",
    "HOME",
    "LANG",
    "LC_ALL",
    "PATH",
    "PATHEXT",
    "PYTHONIOENCODING",
    "SYSTEMDRIVE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "VIRTUAL_ENV",
    "WINDIR",
}


@dataclass(frozen=True)
class EnvBuildResult:
    env: dict[str, str]
    denied: list[str] = field(default_factory=list)
    redactor: SecretRedactor = field(default_factory=SecretRedactor)


class EnvPolicy:
    def __init__(self, *, inherited_allowlist: set[str] | None = None) -> None:
        self.inherited_allowlist = inherited_allowlist or DEFAULT_INHERITED_ENV

    def build(self, requested: dict[str, str]) -> EnvBuildResult:
        redactor = SecretRedactor()
        env: dict[str, str] = {}
        for name in sorted(self.inherited_allowlist):
            value = os.environ.get(name)
            if value is not None and not is_secret_env_name(name):
                env[name] = value
        env.setdefault("PYTHONIOENCODING", "utf-8")

        denied: list[str] = []
        for name, value in requested.items():
            if is_secret_env_name(name):
                denied.append(name)
                redactor.add_literal(str(value))
                continue
            env[name] = str(value)
        redactor.add_env_values(requested)
        return EnvBuildResult(env=env, denied=sorted(denied), redactor=redactor)
