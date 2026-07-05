from __future__ import annotations

import fnmatch
import os
from dataclasses import dataclass
from enum import Enum, StrEnum
from pathlib import Path
from typing import Any, TypeVar

from singularity.utils.serialization import coerce_enum_name


class PermissionProfileName(StrEnum):
    READ_ONLY = "read-only"
    WORKSPACE_WRITE = "workspace-write"
    DANGER_FULL_ACCESS = "danger-full-access"


class ApprovalPolicy(StrEnum):
    ON_REQUEST = "on-request"
    NEVER = "never"


class NetworkAccess(StrEnum):
    DENIED = "denied"
    ALLOWED = "allowed"


@dataclass(frozen=True)
class ProtectedPathRule:
    pattern: str
    allow_read: bool = False
    allow_write: bool = False
    allow_execute: bool = False
    hard_deny: bool = True
    description: str = ""

    def denies(self, access: str) -> bool:
        allowed = {
            "read": self.allow_read,
            "write": self.allow_write,
            "execute": self.allow_execute,
        }
        try:
            return not allowed[access]
        except KeyError as exc:
            raise ValueError(f"unsupported protected-path access: {access}") from exc


_GIT_RULES = (
    ProtectedPathRule(".git", allow_read=True, description="Git metadata"),
    ProtectedPathRule(".git/**", allow_read=True, description="Git metadata"),
    ProtectedPathRule("**/.git", allow_read=True, description="Git metadata"),
    ProtectedPathRule("**/.git/**", allow_read=True, description="Git metadata"),
)
_RUNTIME_STATE_RULES = (
    ProtectedPathRule(".singularity", description="Singularity runtime state"),
    ProtectedPathRule(".singularity/**", description="Singularity runtime state"),
    ProtectedPathRule("**/.singularity", description="Singularity runtime state"),
    ProtectedPathRule("**/.singularity/**", description="Singularity runtime state"),
)
_ENVIRONMENT_RULES = (
    ProtectedPathRule(".env", description="Environment file"),
    ProtectedPathRule(".env.*", description="Environment file"),
    ProtectedPathRule("**/.env", description="Environment file"),
    ProtectedPathRule("**/.env.*", description="Environment file"),
)
_CREDENTIAL_DIRECTORY_RULES = tuple(
    ProtectedPathRule(pattern, description="Credential directory")
    for pattern in (
        ".ssh/**",
        "**/.ssh/**",
        ".aws/**",
        "**/.aws/**",
        ".azure/**",
        "**/.azure/**",
        ".config/gcloud/**",
        "**/.config/gcloud/**",
    )
)
_CREDENTIAL_FILE_RULES = tuple(
    ProtectedPathRule(pattern, description="Credential or private-key file")
    for pattern in (
        "credentials",
        "credentials.*",
        "**/credentials",
        "**/credentials.*",
        "token",
        "token.*",
        "*_token",
        "*_token.*",
        "*-token",
        "*-token.*",
        "**/token",
        "**/token.*",
        "**/*_token",
        "**/*_token.*",
        "**/*-token",
        "**/*-token.*",
        "id_rsa",
        "id_ed25519",
        "**/id_rsa",
        "**/id_ed25519",
        "*.pem",
        "*.key",
        "*.pfx",
        "*.p12",
        "**/*.pem",
        "**/*.key",
        "**/*.pfx",
        "**/*.p12",
    )
)

BUILTIN_PROTECTED_PATH_RULES: tuple[ProtectedPathRule, ...] = (
    *_GIT_RULES,
    *_RUNTIME_STATE_RULES,
    *_ENVIRONMENT_RULES,
    *_CREDENTIAL_DIRECTORY_RULES,
    *_CREDENTIAL_FILE_RULES,
)


@dataclass(frozen=True)
class PermissionSummary:
    profile: PermissionProfileName
    writable_roots: tuple[str, ...]
    network_access: NetworkAccess
    approval_policy: ApprovalPolicy
    protected_paths_enforced: bool = True

    def __post_init__(self) -> None:
        object.__setattr__(self, "profile", _enum(PermissionProfileName, self.profile))
        object.__setattr__(
            self, "network_access", _enum(NetworkAccess, self.network_access)
        )
        object.__setattr__(
            self, "approval_policy", _enum(ApprovalPolicy, self.approval_policy)
        )
        object.__setattr__(
            self, "writable_roots", tuple(str(item) for item in self.writable_roots)
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "profile": self.profile.value,
            "writable_roots": list(self.writable_roots),
            "network_access": self.network_access.value,
            "approval_policy": self.approval_policy.value,
            "protected_paths_enforced": self.protected_paths_enforced,
        }


@dataclass(frozen=True)
class PermissionProfile:
    profile: PermissionProfileName
    workspace_roots: tuple[Path | str, ...]
    additional_writable_directories: tuple[Path | str, ...] = ()
    network_access: NetworkAccess = NetworkAccess.DENIED
    approval_policy: ApprovalPolicy = ApprovalPolicy.ON_REQUEST
    protected_paths: tuple[ProtectedPathRule | str, ...] = ()

    def __post_init__(self) -> None:
        object.__setattr__(self, "profile", _enum(PermissionProfileName, self.profile))
        roots = _normalized_paths(self.workspace_roots)
        if not roots:
            raise ValueError("PermissionProfile requires at least one workspace root.")
        object.__setattr__(self, "workspace_roots", roots)
        object.__setattr__(
            self,
            "additional_writable_directories",
            _normalized_paths(self.additional_writable_directories),
        )
        object.__setattr__(
            self, "network_access", _enum(NetworkAccess, self.network_access)
        )
        object.__setattr__(
            self, "approval_policy", _enum(ApprovalPolicy, self.approval_policy)
        )
        custom_rules = tuple(_protected_rule(rule) for rule in self.protected_paths)
        object.__setattr__(
            self,
            "protected_paths",
            tuple(dict.fromkeys((*BUILTIN_PROTECTED_PATH_RULES, *custom_rules))),
        )

    @classmethod
    def default_for_workspace(
        cls,
        workspace_root: Path | str,
        *,
        profile: PermissionProfileName | str = PermissionProfileName.WORKSPACE_WRITE,
        approval_policy: ApprovalPolicy | str = ApprovalPolicy.ON_REQUEST,
        network_access: NetworkAccess | str = NetworkAccess.DENIED,
        additional_writable_directories: tuple[Path | str, ...] = (),
        protected_paths: tuple[ProtectedPathRule | str, ...] = (),
    ) -> PermissionProfile:
        return cls(
            profile=profile,
            workspace_roots=(workspace_root,),
            additional_writable_directories=additional_writable_directories,
            network_access=network_access,
            approval_policy=approval_policy,
            protected_paths=protected_paths,
        )

    def summary(self) -> PermissionSummary:
        writable_roots: tuple[str, ...]
        if self.profile == PermissionProfileName.READ_ONLY:
            writable_roots = ()
        else:
            writable_roots = tuple(
                str(path)
                for path in (
                    *self.workspace_roots,
                    *self.additional_writable_directories,
                )
            )
        return PermissionSummary(
            profile=self.profile,
            writable_roots=writable_roots,
            network_access=self.network_access,
            approval_policy=self.approval_policy,
            protected_paths_enforced=True,
        )

    def is_workspace_path(self, path: Path | str) -> bool:
        resolved = _normalized_path(path)
        return any(_is_within(resolved, root) for root in self.workspace_roots)

    def is_additional_writable_path(self, path: Path | str) -> bool:
        resolved = _normalized_path(path)
        return any(
            _is_within(resolved, root)
            for root in self.additional_writable_directories
        )

    def is_writable_path(self, path: Path | str) -> bool:
        resolved = _normalized_path(path)
        if self.matching_protected_rule(resolved, access="write") is not None:
            return False
        if self.profile == PermissionProfileName.READ_ONLY:
            return False
        return self.is_workspace_path(resolved) or self.is_additional_writable_path(
            resolved
        )

    def matching_protected_rule(
        self, path: Path | str, *, access: str | None = None
    ) -> ProtectedPathRule | None:
        resolved = _normalized_path(path)
        candidates = _path_match_candidates(
            resolved,
            (*self.workspace_roots, *self.additional_writable_directories),
        )
        for rule in self.protected_paths:
            if not _rule_matches(rule, resolved, candidates):
                continue
            if access is None or rule.denies(access):
                return rule
        return None


_EnumT = TypeVar("_EnumT", bound=Enum)


def _enum(enum_type: type[_EnumT], value: _EnumT | str) -> _EnumT:
    return coerce_enum_name(
        enum_type,
        value,
        name_normalizer=lambda text: text.upper().replace("-", "_"),
    )


def _normalized_paths(values: tuple[Path | str, ...]) -> tuple[Path, ...]:
    paths = tuple(_normalized_path(value) for value in values)
    return tuple(dict.fromkeys(paths))


def _normalized_path(value: Path | str) -> Path:
    return Path(value).expanduser().resolve(strict=False)


def _protected_rule(value: ProtectedPathRule | str) -> ProtectedPathRule:
    if isinstance(value, ProtectedPathRule):
        return value
    return ProtectedPathRule(pattern=str(value))


def _is_within(path: Path, root: Path) -> bool:
    try:
        path_key = os.path.normcase(os.path.normpath(str(path)))
        root_key = os.path.normcase(os.path.normpath(str(root)))
        return os.path.commonpath((path_key, root_key)) == root_key
    except ValueError:
        return False


def _path_match_candidates(path: Path, roots: tuple[Path, ...]) -> tuple[str, ...]:
    candidates = {path.as_posix(), path.name}
    for root in roots:
        if _is_within(path, root):
            try:
                candidates.add(path.relative_to(root).as_posix())
            except ValueError:
                continue
    return tuple(os.path.normcase(candidate) for candidate in candidates)


def _rule_matches(
    rule: ProtectedPathRule, path: Path, candidates: tuple[str, ...]
) -> bool:
    if rule.description == "Environment file" and path.name.lower() in {
        ".env.example",
        ".env.sample",
        ".env.template",
    }:
        return False
    pattern = os.path.normcase(rule.pattern.replace("\\", "/"))
    return any(fnmatch.fnmatchcase(candidate, pattern) for candidate in candidates)
