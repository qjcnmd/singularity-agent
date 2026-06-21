from __future__ import annotations

import fnmatch
from collections.abc import Mapping

from singularity.sandbox.models import SandboxEnvPolicy


class SandboxEnvironmentBuilder:
    def build_env(
        self,
        policy: SandboxEnvPolicy,
        base_env: Mapping[str, str],
    ) -> dict[str, str]:
        env: dict[str, str] = {}
        if policy.inherit_env:
            candidates = dict(base_env)
        else:
            candidates = {
                key: base_env[key]
                for key in policy.allowlist
                if key in base_env and not self._matches(key, policy.denylist_patterns, policy)
            }
        for key, value in candidates.items():
            if self._matches(key, policy.denylist_patterns, policy):
                continue
            env[str(key)] = str(value)
        for key, value in policy.extra_env.items():
            if self._matches(key, policy.denylist_patterns, policy):
                continue
            env[str(key)] = str(value)
        env.setdefault("PYTHONIOENCODING", "utf-8")
        return env

    def redact_env(self, env: Mapping[str, str]) -> dict[str, str]:
        policy = SandboxEnvPolicy()
        return {
            str(key): "[REDACTED]"
            if self._matches(str(key), policy.redacted_patterns, policy)
            else str(value)
            for key, value in env.items()
        }

    @staticmethod
    def _matches(name: str, patterns: list[str], policy: SandboxEnvPolicy) -> bool:
        value = name.upper() if policy.case_insensitive else name
        for pattern in patterns:
            candidate = pattern.upper() if policy.case_insensitive else pattern
            if fnmatch.fnmatchcase(value, candidate):
                return True
        return False
