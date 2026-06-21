from __future__ import annotations

from typing import Any

from singularity.memory.models import (
    Confidence,
    MemoryCandidate,
    MemoryEvidenceRef,
    MemoryScope,
    MemorySource,
    MemoryType,
    Provenance,
    new_memory_id,
    _now,
)


class MemoryExtractor:
    def from_trace_summary(self, trace_summary: Any) -> list[MemoryCandidate]:
        payload = _plain(trace_summary)
        candidates: list[MemoryCandidate] = []
        facts = []
        if isinstance(payload, dict):
            facts = list(payload.get("stable_facts") or payload.get("verified_facts") or [])
        for index, fact in enumerate(facts):
            fact_payload = fact if isinstance(fact, dict) else {"summary": str(fact)}
            summary = str(fact_payload.get("summary") or fact_payload.get("body") or "")
            if not summary:
                continue
            candidates.append(
                _candidate(
                    source=MemorySource.TRACE,
                    type=_type_from_text(summary),
                    title=str(fact_payload.get("title") or _title(summary)),
                    body=summary,
                    evidence_ref=str(fact_payload.get("ref_id") or f"trace_fact_{index}"),
                    evidence_summary="trace summary stable fact",
                    tools=list(fact_payload.get("tools") or []),
                    paths=list(fact_payload.get("paths") or []),
                    modules=list(fact_payload.get("modules") or []),
                )
            )
        return candidates

    def from_final_report(self, final_report: Any) -> list[MemoryCandidate]:
        payload = _plain(final_report)
        if not isinstance(payload, dict):
            return []
        candidates: list[MemoryCandidate] = []
        verification = payload.get("verification_summary") or {}
        passed = verification.get("passed_checks") or []
        status = verification.get("status")
        files = list(payload.get("files_changed") or [])
        if status in {"ready", "ready_with_warnings", "passed", "completed"} or passed:
            body = "Final report verified task with checks: " + ", ".join(map(str, passed or [status]))
            candidates.append(
                _candidate(
                    source=MemorySource.FINAL_REPORT,
                    type=MemoryType.VERIFICATION_FACT,
                    title="Verified task outcome",
                    body=body,
                    evidence_ref=str(payload.get("run_id") or payload.get("task_id") or "final_report"),
                    evidence_summary="final report verification summary",
                    paths=files,
                    confidence=Confidence.HIGH,
                    last_verified_at=_now(),
                )
            )
        rollback = payload.get("rollback_status") or payload.get("shutdown_summary") or {}
        if isinstance(rollback, dict):
            candidates.extend(self.from_rollback(rollback))
        return candidates

    def from_review_report(self, report: Any) -> list[MemoryCandidate]:
        payload = _plain(report)
        if not isinstance(payload, dict):
            return []
        findings = list(payload.get("findings") or [])
        candidates: list[MemoryCandidate] = []
        for finding in findings:
            if not isinstance(finding, dict):
                continue
            title = str(finding.get("title") or "Review caution")
            evidence = list(finding.get("evidence") or finding.get("evidence_ids") or [])
            location = finding.get("location") if isinstance(finding.get("location"), dict) else {}
            body = " ".join(
                part
                for part in [
                    title,
                    str(finding.get("recommendation") or ""),
                    " ".join(str(item) for item in evidence),
                ]
                if part
            )
            candidates.append(
                _candidate(
                    source=MemorySource.REVIEW,
                    type=MemoryType.CAUTION,
                    title=title,
                    body=body,
                    evidence_ref=str(finding.get("finding_id") or new_memory_id("review_finding")),
                    evidence_summary="review finding",
                    confidence=Confidence.MEDIUM,
                    paths=[str(location.get("path"))] if location.get("path") else [],
                    tags=[str(finding.get("category") or "")],
                )
            )
        return candidates

    def from_verification_result(self, result: Any) -> list[MemoryCandidate]:
        payload = _plain(result)
        if not isinstance(payload, dict):
            return []
        if "verification" in payload and isinstance(payload["verification"], dict):
            return self.from_verification_observation(payload)
        status = str(payload.get("status") or "").lower()
        evidence = payload.get("evidence") if isinstance(payload.get("evidence"), dict) else {}
        command = str(evidence.get("command") or payload.get("command") or "")
        output = str(evidence.get("output_excerpt") or payload.get("message") or "")
        check_id = str(payload.get("check_id") or payload.get("id") or new_memory_id("check"))
        kind = str(payload.get("kind") or "")
        failure_type = str(payload.get("failure_type") or "")
        if status in {"passed", "succeeded"}:
            body = f"Verification passed for {kind or check_id}: {command}".strip()
            return [
                _candidate(
                    source=MemorySource.VERIFICATION,
                    type=MemoryType.TEST_COMMAND if "test" in kind or "pytest" in command else MemoryType.VERIFICATION_FACT,
                    title="Verified command",
                    body=body,
                    evidence_ref=check_id,
                    evidence_summary="verification passed",
                    confidence=Confidence.VERIFIED,
                    tools=_tools_from_command(command),
                    last_verified_at=_now(),
                )
            ]
        if status in {"failed", "blocked", "timeout", "flaky"} or failure_type:
            body = f"Verification {status or 'failed'} ({failure_type or 'unknown'}): {command} {output}".strip()
            return [
                _candidate(
                    source=MemorySource.VERIFICATION,
                    type=MemoryType.FAILURE_LESSON,
                    title=f"Verification failure: {failure_type or status or kind}",
                    body=body,
                    evidence_ref=check_id,
                    evidence_summary="verification failure evidence",
                    confidence=Confidence.MEDIUM,
                    tools=_tools_from_command(command),
                    error_types=[failure_type] if failure_type else [],
                )
            ]
        return []

    def from_verification_observation(self, observation: Any) -> list[MemoryCandidate]:
        payload = _plain(observation)
        verification = payload.get("verification") if isinstance(payload, dict) else {}
        if not isinstance(verification, dict):
            return []
        candidates: list[MemoryCandidate] = []
        for result in list(verification.get("results") or []):
            candidates.extend(self.from_verification_result(result))
        for result in list(verification.get("failed_checks") or []):
            if not any(
                candidate.provenance.evidence
                and candidate.provenance.evidence[0].ref_id == str(result.get("check_id"))
                for candidate in candidates
            ):
                candidates.extend(self.from_verification_result(result))
        return candidates

    def from_rollback(self, rollback: Any) -> list[MemoryCandidate]:
        payload = _plain(rollback)
        if not isinstance(payload, dict):
            return []
        code = payload.get("error_code") or payload.get("failure_code") or payload.get("reason")
        message = payload.get("message") or payload.get("reason") or payload.get("summary")
        conflicts = list(payload.get("conflicts") or payload.get("rollback_conflicts") or [])
        warnings = list(payload.get("warnings") or [])
        if not (code or message or conflicts or warnings):
            return []
        body = "; ".join(
            str(part)
            for part in [
                f"error_code={code}" if code else "",
                message or "",
                f"conflicts={', '.join(map(str, conflicts))}" if conflicts else "",
                f"warnings={', '.join(map(str, warnings))}" if warnings else "",
            ]
            if part
        )
        return [
            _candidate(
                source=MemorySource.ROLLBACK,
                type=MemoryType.FAILURE_LESSON,
                title=f"Rollback lesson: {code or 'rollback'}",
                body=body,
                evidence_ref=str(payload.get("rollback_id") or payload.get("transaction_id") or "rollback"),
                evidence_summary="rollback reason",
                confidence=Confidence.HIGH,
                paths=[str(item) for item in conflicts],
                error_types=[str(code)] if code else [],
            )
        ]


def _candidate(
    *,
    source: MemorySource,
    type: MemoryType,
    title: str,
    body: str,
    evidence_ref: str,
    evidence_summary: str,
    scope: MemoryScope = MemoryScope.PROJECT,
    confidence: Confidence = Confidence.MEDIUM,
    paths: list[str] | None = None,
    tools: list[str] | None = None,
    modules: list[str] | None = None,
    error_types: list[str] | None = None,
    tags: list[str] | None = None,
    last_verified_at: str | None = None,
) -> MemoryCandidate:
    return MemoryCandidate(
        id=new_memory_id("cand"),
        scope=scope,
        type=type,
        source=source,
        title=title[:120],
        body=body,
        confidence=confidence,
        provenance=Provenance(
            evidence=[
                MemoryEvidenceRef(
                    source=source,
                    ref_id=evidence_ref,
                    summary=evidence_summary,
                )
            ]
        ),
        paths=paths or [],
        tools=tools or [],
        modules=modules or _modules_from_paths(paths or []),
        error_types=error_types or [],
        tags=[tag for tag in (tags or []) if tag],
        last_verified_at=last_verified_at,
    )


def _type_from_text(text: str) -> MemoryType:
    lowered = text.lower()
    if "pytest" in lowered or "test" in lowered:
        return MemoryType.TEST_COMMAND
    if "build" in lowered:
        return MemoryType.BUILD_COMMAND
    return MemoryType.LESSON


def _tools_from_command(command: str) -> list[str]:
    tools = []
    lowered = command.lower()
    for tool in ("pytest", "ruff", "mypy", "tsc", "eslint", "vitest", "npm", "pnpm"):
        if tool in lowered:
            tools.append(tool)
    return tools


def _modules_from_paths(paths: list[str]) -> list[str]:
    modules = []
    for path in paths:
        parts = path.replace("\\", "/").split("/")
        if "src" in parts and len(parts) > parts.index("src") + 1:
            modules.append(parts[parts.index("src") + 1])
        elif parts:
            modules.append(parts[0])
    return sorted(set(modules))


def _title(text: str) -> str:
    return text.strip().splitlines()[0][:80] if text.strip() else "Memory candidate"


def _plain(value: Any) -> Any:
    if value is None:
        return {}
    if hasattr(value, "model_dump"):
        return value.model_dump(mode="json")
    if hasattr(value, "to_dict"):
        return value.to_dict()
    if isinstance(value, dict):
        return value
    return {"value": str(value)}
