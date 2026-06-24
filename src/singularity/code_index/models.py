from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field, is_dataclass
from datetime import UTC, datetime
from enum import Enum
from typing import Any


SCHEMA_VERSION = "1.0.0"


class FreshnessStatus(str, Enum):
    FRESH = "fresh"
    STALE_CONTENT = "stale_content"
    STALE_DEPENDENCY = "stale_dependency"
    STALE_CONFIG = "stale_config"
    STALE_SEMANTIC = "stale_semantic"
    INVALID = "invalid"
    UNKNOWN = "unknown"


class TrustLevel(str, Enum):
    COMPONENT_GENERATED = "component_generated"
    WORKSPACE_UNTRUSTED = "workspace_untrusted"
    EXTERNAL_UNTRUSTED = "external_untrusted"


class LanguageId(str, Enum):
    PYTHON = "python"
    JAVASCRIPT = "javascript"
    TYPESCRIPT = "typescript"
    RUST = "rust"
    MARKDOWN = "markdown"
    JSON = "json"
    TOML = "toml"
    YAML = "yaml"
    TEXT = "text"
    UNKNOWN = "unknown"


class FileRole(str, Enum):
    SOURCE = "source"
    TEST = "test"
    CONFIG = "config"
    DOC = "doc"
    ENTRYPOINT = "entrypoint"
    GENERATED = "generated"
    VENDOR = "vendor"
    BUILD_ARTIFACT = "build_artifact"
    LOCKFILE = "lockfile"
    BINARY = "binary"
    HIDDEN = "hidden"
    UNKNOWN = "unknown"


class SymbolKind(str, Enum):
    MODULE = "module"
    CLASS = "class"
    FUNCTION = "function"
    METHOD = "method"
    VARIABLE = "variable"
    STRUCT = "struct"
    ENUM = "enum"
    TRAIT = "trait"
    IMPL = "impl"
    TEST = "test"
    UNKNOWN = "unknown"


class DependencyKind(str, Enum):
    IMPORT = "import"
    EXPORT = "export"
    USE = "use"
    MOD = "mod"
    PACKAGE = "package"
    CONFIG = "config"
    UNKNOWN = "unknown"


class ProjectKind(str, Enum):
    SINGLE_PROJECT = "single_project"
    MONOREPO = "monorepo"
    PACKAGE = "package"
    UNKNOWN = "unknown"


@dataclass(frozen=True)
class Evidence:
    source: str
    path: str | None = None
    line_start: int | None = None
    line_end: int | None = None
    description: str = ""
    digest: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass(frozen=True)
class BackendInfo:
    name: str
    version: str = "1.0.0"
    source: str = "component"

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


DEFAULT_BACKEND = BackendInfo(name="singularity.code_index", version=SCHEMA_VERSION)


@dataclass(kw_only=True)
class IndexFact:
    freshness: FreshnessStatus | str = FreshnessStatus.FRESH
    confidence: float = 1.0
    evidence: list[Evidence] = field(default_factory=list)
    trust_level: TrustLevel | str = TrustLevel.COMPONENT_GENERATED
    backend: BackendInfo = DEFAULT_BACKEND
    source: str = "component"
    observed_at: str = field(default_factory=lambda: _now())

    def __post_init__(self) -> None:
        self.freshness = _enum(FreshnessStatus, self.freshness)
        self.trust_level = _enum(TrustLevel, self.trust_level)
        self.evidence = [
            item if isinstance(item, Evidence) else Evidence(**dict(item))
            for item in self.evidence
        ]
        if not isinstance(self.backend, BackendInfo):
            self.backend = BackendInfo(**dict(self.backend))
        self.confidence = max(0.0, min(1.0, float(self.confidence)))

    def to_dict(self) -> dict[str, Any]:
        return _to_plain(self)


@dataclass(kw_only=True)
class FileRecord(IndexFact):
    path: str = ""
    language: LanguageId | str = LanguageId.UNKNOWN
    roles: list[FileRole | str] = field(default_factory=list)
    size_bytes: int = 0
    sha256: str | None = None
    mtime_ns: int = 0
    is_binary: bool = False
    is_hidden: bool = False
    line_count: int | None = None

    def __post_init__(self) -> None:
        super().__post_init__()
        self.language = _enum(LanguageId, self.language)
        self.roles = [_enum(FileRole, role) for role in (self.roles or [FileRole.UNKNOWN])]

    @property
    def is_source(self) -> bool:
        return FileRole.SOURCE in self.roles

    @property
    def is_test(self) -> bool:
        return FileRole.TEST in self.roles

    @property
    def is_config(self) -> bool:
        return FileRole.CONFIG in self.roles


@dataclass(kw_only=True)
class ProjectRootRecord(IndexFact):
    root_path: str = "."
    kind: ProjectKind | str = ProjectKind.UNKNOWN
    languages: list[LanguageId | str] = field(default_factory=list)
    package_manager: str | None = None
    framework: str | None = None

    def __post_init__(self) -> None:
        super().__post_init__()
        self.kind = _enum(ProjectKind, self.kind)
        self.languages = [_enum(LanguageId, language) for language in self.languages]


@dataclass(kw_only=True)
class EntryPointRecord(IndexFact):
    path: str = ""
    kind: str = "module"
    symbol: str | None = None
    command: str | None = None
    language: LanguageId | str = LanguageId.UNKNOWN

    def __post_init__(self) -> None:
        super().__post_init__()
        self.language = _enum(LanguageId, self.language)


@dataclass(kw_only=True)
class ConfigFactRecord(IndexFact):
    path: str = ""
    key: str = ""
    value: Any = None
    fact_type: str = "config"
    language: LanguageId | str = LanguageId.UNKNOWN

    def __post_init__(self) -> None:
        super().__post_init__()
        self.language = _enum(LanguageId, self.language)


@dataclass(kw_only=True)
class SymbolRecord(IndexFact):
    symbol_id: str = ""
    path: str = ""
    name: str = ""
    qualified_name: str = ""
    kind: SymbolKind | str = SymbolKind.UNKNOWN
    language: LanguageId | str = LanguageId.UNKNOWN
    line_start: int | None = None
    line_end: int | None = None
    signature: str | None = None
    parent_symbol_id: str | None = None
    exported: bool = False

    def __post_init__(self) -> None:
        super().__post_init__()
        self.kind = _enum(SymbolKind, self.kind)
        self.language = _enum(LanguageId, self.language)
        if not self.qualified_name:
            self.qualified_name = self.name
        if not self.symbol_id:
            self.symbol_id = stable_id("symbol", self.path, self.qualified_name, self.kind.value)


@dataclass(kw_only=True)
class DependencyEdgeRecord(IndexFact):
    importer_path: str = ""
    imported: str = ""
    imported_path: str | None = None
    kind: DependencyKind | str = DependencyKind.UNKNOWN
    line: int | None = None
    optional: bool = False

    def __post_init__(self) -> None:
        super().__post_init__()
        self.kind = _enum(DependencyKind, self.kind)


@dataclass(kw_only=True)
class ReferenceRecord(IndexFact):
    path: str = ""
    target: str = ""
    kind: str = "reference"
    line: int | None = None
    symbol_id: str | None = None


@dataclass(kw_only=True)
class CallEdgeRecord(IndexFact):
    caller_symbol_id: str = ""
    callee: str = ""
    callee_symbol_id: str | None = None
    path: str = ""
    line: int | None = None


@dataclass(kw_only=True)
class TestMappingRecord(IndexFact):
    __test__ = False

    source_path: str = ""
    test_path: str = ""
    test_name: str | None = None
    framework: str = ""
    reason: str = ""


@dataclass(kw_only=True)
class DocSectionRecord(IndexFact):
    path: str = ""
    title: str = ""
    level: int = 1
    line_start: int = 1
    line_end: int | None = None
    anchor: str = ""
    summary: str = ""


@dataclass(kw_only=True)
class RelevantFileCandidate(IndexFact):
    path: str = ""
    score: float = 0.0
    reasons: list[str] = field(default_factory=list)
    roles: list[FileRole | str] = field(default_factory=list)

    def __post_init__(self) -> None:
        super().__post_init__()
        self.roles = [_enum(FileRole, role) for role in self.roles]
        self.score = max(0.0, float(self.score))


@dataclass(kw_only=True)
class ContextCandidate(IndexFact):
    path: str = ""
    title: str = ""
    reason: str = ""
    score: float = 0.0
    token_estimate: int = 0
    content_type: str = "file_reference"
    metadata: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        super().__post_init__()
        self.score = max(0.0, float(self.score))


@dataclass(kw_only=True)
class CodeImpactAnalysis(IndexFact):
    requested_paths: list[str] = field(default_factory=list)
    direct_files: list[str] = field(default_factory=list)
    reverse_dependencies: list[str] = field(default_factory=list)
    affected_symbols: list[str] = field(default_factory=list)
    affected_entrypoints: list[str] = field(default_factory=list)
    affected_tests: list[str] = field(default_factory=list)
    config_impact: bool = False
    generated_or_vendor_impact: bool = False
    broad_impact: bool = False
    risk_level: str = "low"
    risk_reasons: list[str] = field(default_factory=list)
    recommended_validation: list[str] = field(default_factory=list)


@dataclass(kw_only=True)
class TestImpactAnalysis(IndexFact):
    __test__ = False

    changed_files: list[str] = field(default_factory=list)
    likely_tests: list[str] = field(default_factory=list)
    commands: list[str] = field(default_factory=list)
    require_full_test: bool = False
    confidence_note: str = ""


@dataclass(kw_only=True)
class IncrementalIndexResult(IndexFact):
    changed_files: list[str] = field(default_factory=list)
    deleted_files: list[str] = field(default_factory=list)
    rebuilt_files: list[str] = field(default_factory=list)
    stale_files: list[str] = field(default_factory=list)
    dirty_reasons: dict[str, list[str]] = field(default_factory=dict)
    full_rebuild_required: bool = False
    summary: dict[str, Any] = field(default_factory=dict)


@dataclass(kw_only=True)
class IndexSummary(IndexFact):
    schema_version: str = SCHEMA_VERSION
    file_count: int = 0
    source_count: int = 0
    test_count: int = 0
    config_count: int = 0
    doc_count: int = 0
    symbol_count: int = 0
    dependency_count: int = 0
    entrypoint_count: int = 0
    languages: list[str] = field(default_factory=list)
    generated_at: str = field(default_factory=lambda: _now())
    limitations: list[str] = field(default_factory=list)


def stable_id(prefix: str, *parts: object) -> str:
    import hashlib

    payload = json.dumps([str(part) for part in parts], ensure_ascii=False, sort_keys=True)
    return f"{prefix}_{hashlib.sha256(payload.encode('utf-8')).hexdigest()[:16]}"


def _enum(enum_cls: type[Enum], value: Any) -> Any:
    if isinstance(value, enum_cls):
        return value
    text = str(value)
    if text in enum_cls.__members__:
        return enum_cls[text]
    return enum_cls(text)


def _to_plain(value: Any) -> Any:
    if isinstance(value, Enum):
        return value.value
    if is_dataclass(value):
        return {key: _to_plain(item) for key, item in asdict(value).items()}
    if isinstance(value, dict):
        return {str(key): _to_plain(item) for key, item in value.items()}
    if isinstance(value, (list, tuple, set)):
        return [_to_plain(item) for item in value]
    return value


def _now() -> str:
    return datetime.now(UTC).isoformat()
