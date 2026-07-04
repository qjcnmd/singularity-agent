from __future__ import annotations

import re
from collections.abc import Iterable

from singularity.code_index.models import (
    ContextCandidate,
    Evidence,
    FileRecord,
    FileRole,
    FreshnessStatus,
    RelevantFileCandidate,
    TrustLevel,
)
from singularity.code_index.store import ProjectIndexStore

DEFAULT_RELEVANT_FILE_LIMIT = 20
DEFAULT_SYMBOL_LIMIT = 50
CONTEXT_BUDGET_TOKENS = 4000
SYMBOL_SEARCH_LIMIT = 200
DOC_SEARCH_LIMIT = 50
QUERY_PREFIX_TERM_COUNT = 3
CONTEXT_FILE_LIMIT = 50
CONFIDENCE_CEILING = 0.95
PATH_TERM_SCORE = 2.0
SYMBOL_MATCH_SCORE = 3.0
DOC_MENTION_SCORE = 1.4
ENTRYPOINT_SCORE = 1.2
TEST_PROXIMITY_SCORE = 1.1
SOURCE_ROLE_SCORE = 0.4
TEST_GOAL_SCORE = 1.2
UNRELATED_TEST_PENALTY = 0.3
CONFIG_GOAL_SCORE = 1.0
STALE_FRESHNESS_MULTIPLIER = 0.7
CONTEXT_REASON_LIMIT = 4
STRUCTURE_ENTRYPOINT_LIMIT = 20
STRUCTURE_CONFIG_LIMIT = 50
MAX_QUERY_TERMS = 24
TOKEN_BASE_COST = 80
TOKEN_PATH_PART_COST = 8
TOKEN_SOURCE_ROLE_COST = 160
TOKEN_DOC_ROLE_COST = 120
TOKEN_CONFIG_ROLE_COST = 100


class ProjectIndexQueryService:
    def __init__(self, store: ProjectIndexStore) -> None:
        self.store = store

    def find_relevant_files(
        self,
        goal: str,
        hints: Iterable[str] | None = None,
        *,
        limit: int = DEFAULT_RELEVANT_FILE_LIMIT,
    ) -> list[RelevantFileCandidate]:
        terms = _terms(goal, hints or [])
        files = self.store.all_files()
        symbols = (
            self.store.query_symbols(
                " ".join(terms[:QUERY_PREFIX_TERM_COUNT]) if terms else "",
                limit=SYMBOL_SEARCH_LIMIT,
            )
            if terms
            else []
        )
        symbol_paths = {symbol.path for symbol in symbols}
        entrypoints = {entry.path for entry in self.store.query_entrypoints()}
        docs = self.store.query_docs(" ".join(terms[:QUERY_PREFIX_TERM_COUNT]), limit=DOC_SEARCH_LIMIT) if terms else []
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
                    confidence=min(file.confidence, CONFIDENCE_CEILING),
                    evidence=[
                        Evidence(
                            source="project_index_query",
                            path=file.path,
                            description="Relevant file scoring from path, symbols, docs, tests, and entrypoints.",
                        )
                    ],
                    trust_level=TrustLevel.COMPONENT_GENERATED,
                    source="project_index_query",
                )
            )
        return sorted(
            candidates,
            key=lambda item: (-item.score, -item.confidence, item.path),
        )[:limit]

    def find_symbols(self, query: str, *, limit: int = DEFAULT_SYMBOL_LIMIT):
        return self.store.query_symbols(query, limit=limit)

    def get_context_candidates(
        self,
        goal: str,
        *,
        budget_tokens: int = CONTEXT_BUDGET_TOKENS,
        hints: Iterable[str] | None = None,
    ) -> list[ContextCandidate]:
        remaining = budget_tokens
        candidates: list[ContextCandidate] = []
        for file in self.find_relevant_files(goal, hints, limit=CONTEXT_FILE_LIMIT):
            estimate = _token_estimate(file)
            if estimate > remaining:
                continue
            remaining -= estimate
            candidates.append(
                ContextCandidate(
                    path=file.path,
                    title=file.path,
                    reason="; ".join(file.reasons[:CONTEXT_REASON_LIMIT]),
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
        entrypoints = [entry.to_dict() for entry in self.store.query_entrypoints()[:STRUCTURE_ENTRYPOINT_LIMIT]]
        configs = [fact.to_dict() for fact in self.store.query_config_facts()[:STRUCTURE_CONFIG_LIMIT]]
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
                score += PATH_TERM_SCORE
                reasons.append(f"path mentions '{term}'")
        if file.path in symbol_paths:
            score += SYMBOL_MATCH_SCORE
            reasons.append("symbol match")
        if file.path in doc_paths:
            score += DOC_MENTION_SCORE
            reasons.append("documentation mention")
        if file.path in entrypoints:
            score += ENTRYPOINT_SCORE
            reasons.append("entrypoint proximity")
        if file.path in tests:
            score += TEST_PROXIMITY_SCORE
            reasons.append("test proximity")
        if FileRole.SOURCE in file.roles:
            score += SOURCE_ROLE_SCORE
        if FileRole.TEST in file.roles and any(term in {"test", "verify", "verification", "测试", "验证"} for term in terms):
            score += TEST_GOAL_SCORE
            reasons.append("test-related goal")
        elif FileRole.TEST in file.roles:
            score -= UNRELATED_TEST_PENALTY
        if FileRole.CONFIG in file.roles and any(term in {"config", "配置", "settings", "component"} for term in terms):
            score += CONFIG_GOAL_SCORE
            reasons.append("config-related goal")
        if file.freshness != FreshnessStatus.FRESH:
            score *= STALE_FRESHNESS_MULTIPLIER
            reasons.append(f"freshness={file.freshness.value}")
        return score, list(dict.fromkeys(reasons))


def _terms(goal: str, hints: Iterable[str]) -> list[str]:
    text = " ".join([goal, *[str(item) for item in hints]]).lower()
    terms = re.findall(r"[a-zA-Z_][a-zA-Z0-9_]{2,}|[\u4e00-\u9fff]{2,}", text)
    stop = {"the", "and", "for", "with", "this", "that", "from", "into", "实现", "完整", "需求"}
    return [term for term in dict.fromkeys(terms) if term not in stop][:MAX_QUERY_TERMS]


def _token_estimate(file: RelevantFileCandidate) -> int:
    base = TOKEN_BASE_COST + len(file.path.split("/")) * TOKEN_PATH_PART_COST
    if FileRole.SOURCE in file.roles:
        base += TOKEN_SOURCE_ROLE_COST
    if FileRole.DOC in file.roles:
        base += TOKEN_DOC_ROLE_COST
    if FileRole.CONFIG in file.roles:
        base += TOKEN_CONFIG_ROLE_COST
    return base
