from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from typing import Any, Iterable, TypeVar

from miniharness.code_index.exceptions import IndexStoreError
from miniharness.code_index.models import (
    SCHEMA_VERSION,
    BackendInfo,
    CallEdgeRecord,
    ConfigFactRecord,
    DependencyEdgeRecord,
    DocSectionRecord,
    EntryPointRecord,
    FileRecord,
    FreshnessStatus,
    IndexFact,
    IndexSummary,
    ProjectRootRecord,
    ReferenceRecord,
    SymbolRecord,
    TestMappingRecord,
    stable_id,
)


T = TypeVar("T", bound=IndexFact)


class ProjectIndexStore:
    def __init__(self, path: Path | str) -> None:
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()

    def reset(self) -> None:
        with self._connect() as db:
            for table in _TABLES:
                db.execute(f"DELETE FROM {_table_name(table)}")
            db.execute("DELETE FROM index_metadata")
            self._write_metadata(db, "schema_version", SCHEMA_VERSION)

    def upsert_files(self, records: Iterable[FileRecord]) -> None:
        with self._connect() as db:
            for record in records:
                db.execute(
                    """
                    INSERT INTO files (
                        path, language, roles_json, size_bytes, sha256, mtime_ns,
                        is_binary, is_hidden, line_count, freshness, confidence, payload_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(path) DO UPDATE SET
                        language=excluded.language,
                        roles_json=excluded.roles_json,
                        size_bytes=excluded.size_bytes,
                        sha256=excluded.sha256,
                        mtime_ns=excluded.mtime_ns,
                        is_binary=excluded.is_binary,
                        is_hidden=excluded.is_hidden,
                        line_count=excluded.line_count,
                        freshness=excluded.freshness,
                        confidence=excluded.confidence,
                        payload_json=excluded.payload_json
                    """,
                    (
                        record.path,
                        record.language.value,
                        json.dumps([role.value for role in record.roles]),
                        record.size_bytes,
                        record.sha256,
                        record.mtime_ns,
                        int(record.is_binary),
                        int(record.is_hidden),
                        record.line_count,
                        record.freshness.value,
                        record.confidence,
                        _dump(record),
                    ),
                )

    def upsert_project_roots(self, records: Iterable[ProjectRootRecord]) -> None:
        with self._connect() as db:
            for record in records:
                db.execute(
                    """
                    INSERT INTO project_roots (
                        root_path, kind, languages_json, package_manager, framework,
                        freshness, confidence, payload_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(root_path) DO UPDATE SET
                        kind=excluded.kind,
                        languages_json=excluded.languages_json,
                        package_manager=excluded.package_manager,
                        framework=excluded.framework,
                        freshness=excluded.freshness,
                        confidence=excluded.confidence,
                        payload_json=excluded.payload_json
                    """,
                    (
                        record.root_path,
                        record.kind.value,
                        json.dumps([language.value for language in record.languages]),
                        record.package_manager,
                        record.framework,
                        record.freshness.value,
                        record.confidence,
                        _dump(record),
                    ),
                )

    def upsert_entrypoints(self, records: Iterable[EntryPointRecord]) -> None:
        with self._connect() as db:
            for record in records:
                key = stable_id("entry", record.path, record.kind, record.symbol, record.command)
                db.execute(
                    """
                    INSERT INTO entrypoints (
                        id, path, kind, symbol, command, language, freshness, confidence, payload_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(id) DO UPDATE SET
                        path=excluded.path, kind=excluded.kind, symbol=excluded.symbol,
                        command=excluded.command, language=excluded.language,
                        freshness=excluded.freshness, confidence=excluded.confidence,
                        payload_json=excluded.payload_json
                    """,
                    (
                        key,
                        record.path,
                        record.kind,
                        record.symbol,
                        record.command,
                        record.language.value,
                        record.freshness.value,
                        record.confidence,
                        _dump(record),
                    ),
                )

    def upsert_config_facts(self, records: Iterable[ConfigFactRecord]) -> None:
        self._upsert_generic(
            "config_facts",
            records,
            key_fields=("path", "key", "fact_type"),
            fields=("path", "key", "fact_type", "freshness", "confidence", "payload_json"),
            values=lambda record: (
                record.path,
                record.key,
                record.fact_type,
                record.freshness.value,
                record.confidence,
                _dump(record),
            ),
        )

    def upsert_symbols(self, records: Iterable[SymbolRecord]) -> None:
        with self._connect() as db:
            for record in records:
                db.execute(
                    """
                    INSERT INTO symbols (
                        symbol_id, path, name, qualified_name, kind, language,
                        line_start, line_end, freshness, confidence, payload_json
                    ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(symbol_id) DO UPDATE SET
                        path=excluded.path, name=excluded.name,
                        qualified_name=excluded.qualified_name, kind=excluded.kind,
                        language=excluded.language, line_start=excluded.line_start,
                        line_end=excluded.line_end, freshness=excluded.freshness,
                        confidence=excluded.confidence, payload_json=excluded.payload_json
                    """,
                    (
                        record.symbol_id,
                        record.path,
                        record.name,
                        record.qualified_name,
                        record.kind.value,
                        record.language.value,
                        record.line_start,
                        record.line_end,
                        record.freshness.value,
                        record.confidence,
                        _dump(record),
                    ),
                )

    def upsert_dependencies(self, records: Iterable[DependencyEdgeRecord]) -> None:
        self._upsert_generic(
            "dependencies",
            records,
            key_fields=("importer_path", "imported", "kind", "line"),
            fields=(
                "importer_path",
                "imported",
                "imported_path",
                "kind",
                "line",
                "freshness",
                "confidence",
                "payload_json",
            ),
            values=lambda record: (
                record.importer_path,
                record.imported,
                record.imported_path,
                record.kind.value,
                record.line,
                record.freshness.value,
                record.confidence,
                _dump(record),
            ),
        )

    def upsert_references(self, records: Iterable[ReferenceRecord]) -> None:
        self._upsert_generic(
            "references",
            records,
            key_fields=("path", "target", "kind", "line"),
            fields=("path", "target", "kind", "line", "symbol_id", "freshness", "confidence", "payload_json"),
            values=lambda record: (
                record.path,
                record.target,
                record.kind,
                record.line,
                record.symbol_id,
                record.freshness.value,
                record.confidence,
                _dump(record),
            ),
        )

    def upsert_call_edges(self, records: Iterable[CallEdgeRecord]) -> None:
        self._upsert_generic(
            "call_edges",
            records,
            key_fields=("caller_symbol_id", "callee", "path", "line"),
            fields=(
                "caller_symbol_id",
                "callee",
                "callee_symbol_id",
                "path",
                "line",
                "freshness",
                "confidence",
                "payload_json",
            ),
            values=lambda record: (
                record.caller_symbol_id,
                record.callee,
                record.callee_symbol_id,
                record.path,
                record.line,
                record.freshness.value,
                record.confidence,
                _dump(record),
            ),
        )

    def upsert_test_mappings(self, records: Iterable[TestMappingRecord]) -> None:
        self._upsert_generic(
            "test_mappings",
            records,
            key_fields=("source_path", "test_path", "test_name"),
            fields=(
                "source_path",
                "test_path",
                "test_name",
                "framework",
                "freshness",
                "confidence",
                "payload_json",
            ),
            values=lambda record: (
                record.source_path,
                record.test_path,
                record.test_name,
                record.framework,
                record.freshness.value,
                record.confidence,
                _dump(record),
            ),
        )

    def upsert_doc_sections(self, records: Iterable[DocSectionRecord]) -> None:
        self._upsert_generic(
            "doc_sections",
            records,
            key_fields=("path", "title", "line_start"),
            fields=(
                "path",
                "title",
                "level",
                "line_start",
                "line_end",
                "freshness",
                "confidence",
                "payload_json",
            ),
            values=lambda record: (
                record.path,
                record.title,
                record.level,
                record.line_start,
                record.line_end,
                record.freshness.value,
                record.confidence,
                _dump(record),
            ),
        )

    def delete_by_path(self, path: str) -> None:
        with self._connect() as db:
            db.execute("DELETE FROM files WHERE path = ?", (path,))
            for table in ("entrypoints", "config_facts", "symbols", "references", "call_edges", "doc_sections"):
                db.execute(f"DELETE FROM {_table_name(table)} WHERE path = ?", (path,))
            db.execute("DELETE FROM dependencies WHERE importer_path = ? OR imported_path = ?", (path, path))
            db.execute("DELETE FROM test_mappings WHERE source_path = ? OR test_path = ?", (path, path))

    def clear_file_facts(self, path: str) -> None:
        with self._connect() as db:
            for table in ("entrypoints", "config_facts", "symbols", "references", "call_edges", "doc_sections"):
                db.execute(f"DELETE FROM {_table_name(table)} WHERE path = ?", (path,))
            db.execute("DELETE FROM dependencies WHERE importer_path = ?", (path,))
            db.execute("DELETE FROM test_mappings WHERE source_path = ? OR test_path = ?", (path, path))

    def mark_stale(
        self,
        paths: Iterable[str],
        freshness: FreshnessStatus = FreshnessStatus.STALE_CONTENT,
    ) -> None:
        normalized = sorted(set(paths))
        if not normalized:
            return
        with self._connect() as db:
            for path in normalized:
                for table, column in (
                    ("files", "path"),
                    ("entrypoints", "path"),
                    ("config_facts", "path"),
                    ("symbols", "path"),
                    ("dependencies", "importer_path"),
                    ("references", "path"),
                    ("call_edges", "path"),
                    ("doc_sections", "path"),
                ):
                    self._mark_rows_stale(
                        db,
                        table,
                        f"{column} = ?",
                        (path,),
                        freshness,
                    )
                self._mark_rows_stale(
                    db,
                    "test_mappings",
                    "source_path = ? OR test_path = ?",
                    (path, path),
                    freshness,
                )

    def mark_global_invalid(self, reason: str) -> None:
        with self._connect() as db:
            for table in _TABLES:
                if table == "project_roots":
                    db.execute("UPDATE project_roots SET freshness = ?", (FreshnessStatus.INVALID.value,))
                else:
                    db.execute(f"UPDATE {_table_name(table)} SET freshness = ?", (FreshnessStatus.INVALID.value,))
            self._write_metadata(db, "global_invalid_reason", reason)

    def set_metadata(self, key: str, value: Any) -> None:
        with self._connect() as db:
            self._write_metadata(db, key, json.dumps(value, ensure_ascii=False, sort_keys=True, default=str))

    def get_metadata(self, key: str) -> str | None:
        with self._connect() as db:
            row = db.execute("SELECT value FROM index_metadata WHERE key = ?", (key,)).fetchone()
        return str(row["value"]) if row else None

    def load_summary(self) -> IndexSummary:
        with self._connect() as db:
            files = db.execute(
                "SELECT language, roles_json, freshness FROM files"
            ).fetchall()
            symbol_count = db.execute("SELECT COUNT(*) AS c FROM symbols").fetchone()["c"]
            dependency_count = db.execute("SELECT COUNT(*) AS c FROM dependencies").fetchone()["c"]
            entrypoint_count = db.execute("SELECT COUNT(*) AS c FROM entrypoints").fetchone()["c"]
        languages = sorted({row["language"] for row in files if row["language"]})
        roles = [set(json.loads(row["roles_json"] or "[]")) for row in files]
        worst = _worst_freshness([row["freshness"] for row in files])
        return IndexSummary(
            file_count=len(files),
            source_count=sum("source" in item for item in roles),
            test_count=sum("test" in item for item in roles),
            config_count=sum("config" in item for item in roles),
            doc_count=sum("doc" in item for item in roles),
            symbol_count=int(symbol_count),
            dependency_count=int(dependency_count),
            entrypoint_count=int(entrypoint_count),
            languages=languages,
            freshness=worst,
            backend=BackendInfo(name="project_index_store", version=SCHEMA_VERSION),
            limitations=[
                "tree-sitter and LSP backends are optional and not required for the static index.",
                "Workspace file contents are treated as untrusted data and stored as metadata only.",
            ],
        )

    def all_files(self) -> list[FileRecord]:
        return self._load_records("SELECT payload_json FROM files ORDER BY path", FileRecord)

    def files_by_path(self, paths: Iterable[str]) -> dict[str, FileRecord]:
        normalized = sorted(set(paths))
        if not normalized:
            return {}
        placeholders = ",".join("?" for _ in normalized)
        rows = self._load_records(
            f"SELECT payload_json FROM files WHERE path IN ({placeholders})",
            FileRecord,
            tuple(normalized),
        )
        return {record.path: record for record in rows}

    def query_symbols(self, query: str, *, limit: int = 50) -> list[SymbolRecord]:
        pattern = f"%{query}%"
        return self._load_records(
            """
            SELECT payload_json FROM symbols
            WHERE name LIKE ? OR qualified_name LIKE ? OR path LIKE ?
            ORDER BY confidence DESC, path, line_start
            LIMIT ?
            """,
            SymbolRecord,
            (pattern, pattern, pattern, limit),
        )

    def symbols_for_paths(self, paths: Iterable[str]) -> list[SymbolRecord]:
        normalized = sorted(set(paths))
        if not normalized:
            return []
        placeholders = ",".join("?" for _ in normalized)
        return self._load_records(
            f"SELECT payload_json FROM symbols WHERE path IN ({placeholders}) ORDER BY path, line_start",
            SymbolRecord,
            tuple(normalized),
        )

    def query_dependencies(self, path: str) -> list[DependencyEdgeRecord]:
        return self._load_records(
            "SELECT payload_json FROM dependencies WHERE importer_path = ? ORDER BY line",
            DependencyEdgeRecord,
            (path,),
        )

    def query_reverse_dependencies(self, paths: Iterable[str]) -> list[DependencyEdgeRecord]:
        normalized = sorted(set(paths))
        if not normalized:
            return []
        placeholders = ",".join("?" for _ in normalized)
        return self._load_records(
            f"""
            SELECT payload_json FROM dependencies
            WHERE imported_path IN ({placeholders}) OR imported IN ({placeholders})
            ORDER BY importer_path, line
            """,
            DependencyEdgeRecord,
            tuple(normalized + normalized),
        )

    def query_tests(self, paths: Iterable[str]) -> list[TestMappingRecord]:
        normalized = sorted(set(paths))
        if not normalized:
            return []
        placeholders = ",".join("?" for _ in normalized)
        return self._load_records(
            f"""
            SELECT payload_json FROM test_mappings
            WHERE source_path IN ({placeholders}) OR test_path IN ({placeholders})
            ORDER BY confidence DESC, test_path
            """,
            TestMappingRecord,
            tuple(normalized + normalized),
        )

    def query_entrypoints(self) -> list[EntryPointRecord]:
        return self._load_records(
            "SELECT payload_json FROM entrypoints ORDER BY confidence DESC, path",
            EntryPointRecord,
        )

    def query_config_facts(self) -> list[ConfigFactRecord]:
        return self._load_records(
            "SELECT payload_json FROM config_facts ORDER BY path, key",
            ConfigFactRecord,
        )

    def query_docs(self, query: str = "", *, limit: int = 50) -> list[DocSectionRecord]:
        if query:
            pattern = f"%{query}%"
            return self._load_records(
                """
                SELECT payload_json FROM doc_sections
                WHERE title LIKE ? OR payload_json LIKE ? OR path LIKE ?
                ORDER BY confidence DESC, path, line_start
                LIMIT ?
                """,
                DocSectionRecord,
                (pattern, pattern, pattern, limit),
            )
        return self._load_records(
            "SELECT payload_json FROM doc_sections ORDER BY path, line_start LIMIT ?",
            DocSectionRecord,
            (limit,),
        )

    def _upsert_generic(
        self,
        table: str,
        records: Iterable[T],
        *,
        key_fields: tuple[str, ...],
        fields: tuple[str, ...],
        values: Any,
    ) -> None:
        with self._connect() as db:
            for record in records:
                payload = record.to_dict()
                key = stable_id(table, *(payload.get(field) for field in key_fields))
                columns = ("id", *fields)
                placeholders = ", ".join("?" for _ in columns)
                updates = ", ".join(f"{field}=excluded.{field}" for field in fields)
                db.execute(
                    f"""
                    INSERT INTO {_table_name(table)} ({", ".join(columns)})
                    VALUES ({placeholders})
                    ON CONFLICT(id) DO UPDATE SET {updates}
                    """,
                    (key, *values(record)),
                )

    def _load_records(
        self,
        sql: str,
        cls: type[T],
        args: tuple[Any, ...] = (),
    ) -> list[T]:
        with self._connect() as db:
            rows = db.execute(sql, args).fetchall()
        return [_loads(row["payload_json"], cls) for row in rows]

    def _connect(self) -> sqlite3.Connection:
        try:
            db = sqlite3.connect(self.path)
            db.row_factory = sqlite3.Row
            return db
        except sqlite3.Error as exc:
            raise IndexStoreError(str(exc), code="index_store_connect_failed") from exc

    def _init_schema(self) -> None:
        with self._connect() as db:
            db.executescript(
                """
                PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS files (
                    path TEXT PRIMARY KEY,
                    language TEXT NOT NULL,
                    roles_json TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL,
                    sha256 TEXT,
                    mtime_ns INTEGER NOT NULL,
                    is_binary INTEGER NOT NULL,
                    is_hidden INTEGER NOT NULL,
                    line_count INTEGER,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS project_roots (
                    root_path TEXT PRIMARY KEY,
                    kind TEXT NOT NULL,
                    languages_json TEXT NOT NULL,
                    package_manager TEXT,
                    framework TEXT,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS entrypoints (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    symbol TEXT,
                    command TEXT,
                    language TEXT NOT NULL,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS config_facts (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    key TEXT NOT NULL,
                    fact_type TEXT NOT NULL,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS symbols (
                    symbol_id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    name TEXT NOT NULL,
                    qualified_name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    language TEXT NOT NULL,
                    line_start INTEGER,
                    line_end INTEGER,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS dependencies (
                    id TEXT PRIMARY KEY,
                    importer_path TEXT NOT NULL,
                    imported TEXT NOT NULL,
                    imported_path TEXT,
                    kind TEXT NOT NULL,
                    line INTEGER,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS "references" (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    target TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    line INTEGER,
                    symbol_id TEXT,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS call_edges (
                    id TEXT PRIMARY KEY,
                    caller_symbol_id TEXT NOT NULL,
                    callee TEXT NOT NULL,
                    callee_symbol_id TEXT,
                    path TEXT NOT NULL,
                    line INTEGER,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS test_mappings (
                    id TEXT PRIMARY KEY,
                    source_path TEXT NOT NULL,
                    test_path TEXT NOT NULL,
                    test_name TEXT,
                    framework TEXT NOT NULL,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS doc_sections (
                    id TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    title TEXT NOT NULL,
                    level INTEGER NOT NULL,
                    line_start INTEGER NOT NULL,
                    line_end INTEGER,
                    freshness TEXT NOT NULL,
                    confidence REAL NOT NULL,
                    payload_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS index_metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_symbols_path ON symbols(path);
                CREATE INDEX IF NOT EXISTS idx_dependencies_imported_path ON dependencies(imported_path);
                CREATE INDEX IF NOT EXISTS idx_test_mappings_source ON test_mappings(source_path);
                """
            )
            self._write_metadata(db, "schema_version", SCHEMA_VERSION)

    @staticmethod
    def _mark_rows_stale(
        db: sqlite3.Connection,
        table: str,
        where_sql: str,
        args: tuple[Any, ...],
        freshness: FreshnessStatus,
    ) -> None:
        table_sql = _table_name(table)
        rows = db.execute(
            f"SELECT id, payload_json FROM {table_sql} WHERE {where_sql}"
            if table not in {"files", "symbols", "project_roots"}
            else (
                f"SELECT path AS id, payload_json FROM files WHERE {where_sql}"
                if table == "files"
                else f"SELECT symbol_id AS id, payload_json FROM symbols WHERE {where_sql}"
                if table == "symbols"
                else f"SELECT root_path AS id, payload_json FROM project_roots WHERE {where_sql}"
            ),
            args,
        ).fetchall()
        key_column = "path" if table == "files" else "symbol_id" if table == "symbols" else "root_path" if table == "project_roots" else "id"
        for row in rows:
            payload = json.loads(row["payload_json"])
            payload["freshness"] = freshness.value
            db.execute(
                f"UPDATE {table_sql} SET freshness = ?, payload_json = ? WHERE {key_column} = ?",
                (freshness.value, json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str), row["id"]),
            )

    @staticmethod
    def _write_metadata(db: sqlite3.Connection, key: str, value: str) -> None:
        db.execute(
            """
            INSERT INTO index_metadata (key, value)
            VALUES (?, ?)
            ON CONFLICT(key) DO UPDATE SET value=excluded.value
            """,
            (key, value),
        )


def _dump(record: IndexFact) -> str:
    return json.dumps(record.to_dict(), ensure_ascii=False, sort_keys=True, default=str)


def _loads(text: str, cls: type[T]) -> T:
    return cls(**json.loads(text))


def _worst_freshness(values: Iterable[str]) -> FreshnessStatus:
    order = [
        FreshnessStatus.FRESH,
        FreshnessStatus.STALE_CONTENT,
        FreshnessStatus.STALE_DEPENDENCY,
        FreshnessStatus.STALE_CONFIG,
        FreshnessStatus.STALE_SEMANTIC,
        FreshnessStatus.UNKNOWN,
        FreshnessStatus.INVALID,
    ]
    worst = FreshnessStatus.FRESH
    for value in values:
        status = FreshnessStatus(value)
        if order.index(status) > order.index(worst):
            worst = status
    return worst


_TABLES = (
    "files",
    "project_roots",
    "entrypoints",
    "config_facts",
    "symbols",
    "dependencies",
    "references",
    "call_edges",
    "test_mappings",
    "doc_sections",
)


def _table_name(table: str) -> str:
    return '"references"' if table == "references" else table
