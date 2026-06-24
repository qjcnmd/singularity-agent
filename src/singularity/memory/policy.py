from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum

from singularity.memory.models import (
    Confidence,
    MemoryCandidate,
    MemoryScope,
    MemorySource,
    MemoryStatus,
    MemoryType,
)


class AdmissionAction(str, Enum):
    ACCEPT = "accept"
    QUARANTINE = "quarantine"
    REJECT = "reject"


@dataclass(frozen=True)
class AdmissionDecision:
    action: AdmissionAction
    candidate: MemoryCandidate
    reasons: list[str] = field(default_factory=list)


class MemoryPolicy:
    stable_sources = {
        MemorySource.TRACE,
        MemorySource.FINAL_REPORT,
        MemorySource.REVIEW,
        MemorySource.VERIFICATION,
        MemorySource.ROLLBACK,
        MemorySource.USER,
        MemorySource.MANUAL,
        MemorySource.HUMAN_FILE,
    }
    long_term_scopes = {
        MemoryScope.WORKSPACE,
        MemoryScope.PROJECT,
        MemoryScope.USER_PREFERENCE,
        MemoryScope.TOOL_EXECUTOR,
    }

    def evaluate(self, candidate: MemoryCandidate) -> AdmissionDecision:
        sanitized = _redact_candidate(candidate)
        reasons: list[str] = []
        if _looks_temporary(sanitized.body):
            return AdmissionDecision(
                AdmissionAction.REJECT,
                sanitized.with_status(MemoryStatus.REJECTED, reason="temporary state"),
                ["temporary or one-time environment state is not long-term memory"],
            )
        evidence_sources = {item.source for item in sanitized.provenance.evidence}
        stable_evidence = evidence_sources & self.stable_sources
        if sanitized.scope in self.long_term_scopes and not stable_evidence:
            reasons.append("stable evidence is required for long-term memory")
        if sanitized.source == MemorySource.MODEL and not stable_evidence:
            reasons.append("model-only memory cannot become active")
        if _looks_like_failure_guess(sanitized.body) and not (
            evidence_sources & {MemorySource.VERIFICATION, MemorySource.REVIEW, MemorySource.ROLLBACK}
        ):
            reasons.append("failure guess is quarantined until concrete evidence exists")
        if sanitized.scope == MemoryScope.USER_PREFERENCE or sanitized.type == MemoryType.USER_PREFERENCE:
            if sanitized.source not in {MemorySource.USER, MemorySource.MANUAL} and MemorySource.MANUAL not in evidence_sources:
                reasons.append("user preference memory requires explicit user source or manual acceptance")
        if reasons:
            return AdmissionDecision(
                AdmissionAction.QUARANTINE,
                sanitized.with_status(MemoryStatus.QUARANTINED, reason="; ".join(reasons)),
                reasons,
            )
        if sanitized.confidence == Confidence.LOW and sanitized.source == MemorySource.MODEL:
            return AdmissionDecision(
                AdmissionAction.QUARANTINE,
                sanitized.with_status(MemoryStatus.QUARANTINED, reason="low-confidence model source"),
                ["low-confidence model source"],
            )
        return AdmissionDecision(AdmissionAction.ACCEPT, sanitized, ["evidence gate passed"])


SECRET_PATTERNS = [
    re.compile(r"(?i)\b(api[_-]?key|token|password|secret)\s*=\s*['\"]?[^'\"\s]+"),
    re.compile(r"(?i)\b(authorization:\s*bearer)\s+[A-Za-z0-9._~+/=-]+"),
    re.compile(r"\bsk-[A-Za-z0-9_-]{6,}\b"),
]

TEMP_PATTERNS = [
    re.compile(r"(?i)\b(current|temporary|temp|tmp)\b.*\b(state|path|directory|dir)\b"),
    re.compile(r"(?i)(AppData\\Local\\Temp|/tmp/|\\Temp\\|/var/folders/)"),
]

GUESS_PATTERNS = [
    re.compile(r"(?i)\b(probably|maybe|may|might|could be|guess|guessed|suspect|unverified)\b"),
]


def _redact_candidate(candidate: MemoryCandidate) -> MemoryCandidate:
    body = candidate.body
    title = candidate.title
    for pattern in SECRET_PATTERNS:
        body = pattern.sub(lambda match: _redact_secret_match(match.group(0)), body)
        title = pattern.sub(lambda match: _redact_secret_match(match.group(0)), title)
    if body == candidate.body and title == candidate.title:
        return candidate
    payload = candidate.to_dict()
    payload["body"] = body
    payload["title"] = title
    payload.setdefault("metadata", {})["redacted"] = True
    return MemoryCandidate.from_dict(payload)


def _redact_secret_match(value: str) -> str:
    if "=" in value:
        return f"{value.split('=', 1)[0]}=[REDACTED]"
    if ":" in value:
        return f"{value.split(':', 1)[0]}: [REDACTED]"
    return "[REDACTED]"


def _looks_temporary(text: str) -> bool:
    return any(pattern.search(text) for pattern in TEMP_PATTERNS)


def _looks_like_failure_guess(text: str) -> bool:
    return any(pattern.search(text) for pattern in GUESS_PATTERNS)


def contains_secret_like_content(text: str) -> bool:
    return any(pattern.search(text) for pattern in SECRET_PATTERNS)
