from __future__ import annotations

import re
from typing import Iterable

from miniharness.code_index.models import (
    ContextCandidate,
    Evidence,
    FileRecord,
    FileRole,
    FreshnessStatus,
    RelevantFileCandidate,
    TrustLevel,
)
from miniharness.code_index.store import ProjectIndexStore


class ProjectIndexQueryService:
    def __init__(self, store: ProjectIndexStore) -> None:
        self.store = store

    def find_relevant_files(
        self,
        goal: str,
        hints: Iterable[str] | None = None,
        *,
        limit: int = 20,
    ) -> list[RelevantFileCandidate]:
        terms = _terms(goal, hints or [])
        files = self.store.all_files()
        symbols = self.store.query_symbols(" ".join(terms[:3]) if terms else "", limit=200) if terms else []
        symbol_paths = {symbol.path for symbol in symbols}
        entrypoints = {entry.path for entry in self.store.query_entrypoints()}
        docs = self.store.query_docs(" ".join(terms[:3]), limit=50) if terms else []
        doc_paths = {doc.path for doc in docs}
        tests = {mapping.test_path for mapping in self.store.query_tests(symbol_paths)}

        candidates: list[RelevantFileCandidate] = []
        for file in files:
            score, reasons = self._score_file(file, terms, symbol_paths, entrypoints, doc_paths, tests)
            if score <= 0:
                continue
            candidates.append(
                RelevantFileCandidate(
                    path=file.path,
                    score=round(score, 3),
                    reasons=reasons,
                    roles=file.roles,
                    freshness=file.freshness,
                    confidence=min(file.confidence, 0.95),
                    evidence=[
                        Evidence(
                            source="project_index_query",
                            path=file.path,
                            description="Relevant file scoring from path, symbols, docs, tests, and entrypoints.",
                        )
                    ],
                    trust_level=TrustLevel.RUNTIME_GENERATED,
                    source="project_index_query",
                )
            )
        return sorted(
            candidates,
            key=lambda item: (-item.score, -item.confidence, item.path),
        )[:limit]

    def find_symbols(self, query: str, *, limit: int = 50):
        return self.store.query_symbols(query, limit=limit)

    def get_context_candidates(
        self,
        goal: str,
        *,
        budget_tokens: int = 4000,
        hints: Iterable[str] | None = None,
    ) -> list[ContextCandidate]:
        remaining = budget_tokens
        candidates: list[ContextCandidate] = []
        for file in self.find_relevant_files(goal, hints, limit=50):
            estimate = _token_estimate(file)
            if estimate > remaining:
                continue
            remaining -= estimate
            candidates.append(
                ContextCandidate(
                    path=file.path,
                    title=file.path,
                    reason="; ".join(file.reasons[:4]),
                    score=file.score,
                    token_estimate=estimate,
                    freshness=file.freshness,
                    confidence=file.confidence,
                    evidence=file.evidence,
                    metadata={"roles": [role.value for role in file.roles]},
                    trust_level=TrustLevel.WORKSPACE_UNTRUSTED,
                    source="project_index_query",
                )
            )
            if remaining <= 0:
                break
        return candidates

    def explain_project_structure(self) -> dict[str, object]:
        summary = self.store.load_summary()
        entrypoints = [entry.to_dict() for entry in self.store.query_entrypoints()[:20]]
        configs = [fact.to_dict() for fact in self.store.query_config_facts()[:50]]
        return {
            "summary": summary.to_dict(),
            "entrypoints": entrypoints,
            "config_facts": configs,
            "limitations": summary.limitations,
            "trust_level": TrustLevel.WORKSPACE_UNTRUSTED.value,
        }

    def get_entrypoints(self):
        return self.store.query_entrypoints()

    def get_config_facts(self):
        return self.store.query_config_facts()

    @staticmethod
    def _score_file(
        file: FileRecord,
        terms: list[str],
        symbol_paths: set[str],
        entrypoints: set[str],
        doc_paths: set[str],
        tests: set[str],
    ) -> tuple[float, list[str]]:
        score = 0.0
        reasons: list[str] = []
        lowered_path = file.path.lower()
        for term in terms:
            if term and term in lowered_path:
                score += 2.0
                reasons.append(f"path mentions '{term}'")
        if file.path in symbol_paths:
            score += 3.0
            reasons.append("symbol match")
        if file.path in doc_paths:
            score += 1.4
            reasons.append("documentation mention")
        if file.path in entrypoints:
            score += 1.2
            reasons.append("entrypoint proximity")
        if file.path in tests:
            score += 1.1
            reasons.append("test proximity")
        if FileRole.SOURCE in file.roles:
            score += 0.4
        if FileRole.TEST in file.roles and any(term in {"test", "verify", "verification", "测试", "验证"} for term in terms):
            score += 1.2
            reasons.append("test-related goal")
        elif FileRole.TEST in file.roles:
            score -= 0.3
        if FileRole.CONFIG in file.roles and any(term in {"config", "配置", "settings", "runtime"} for term in terms):
            score += 1.0
            reasons.append("config-related goal")
        if file.freshness != FreshnessStatus.FRESH:
            score *= 0.7
            reasons.append(f"freshness={file.freshness.value}")
        return score, list(dict.fromkeys(reasons))


def _terms(goal: str, hints: Iterable[str]) -> list[str]:
    text = " ".join([goal, *[str(item) for item in hints]]).lower()
    terms = re.findall(r"[a-zA-Z_][a-zA-Z0-9_]{2,}|[\u4e00-\u9fff]{2,}", text)
    stop = {"the", "and", "for", "with", "this", "that", "from", "into", "实现", "完整", "需求"}
    return [term for term in dict.fromkeys(terms) if term not in stop][:24]


def _token_estimate(file: RelevantFileCandidate) -> int:
    base = 80 + len(file.path.split("/")) * 8
    if FileRole.SOURCE in file.roles:
        base += 160
    if FileRole.DOC in file.roles:
        base += 120
    if FileRole.CONFIG in file.roles:
        base += 100
    return base
