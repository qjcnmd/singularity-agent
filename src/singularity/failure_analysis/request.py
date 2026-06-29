from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.execution_outcome import ExecutionOutcome

from ._shared import (
    SUMMARY_LIMIT,
    TAIL_LIMIT,
    _append_unique,
    _limit,
    _normalize_workspace_path,
    _paths_from_failure_source,
    _trim_dict,
)


@dataclass(frozen=True)
class FailureAnalysisRequest:
    request_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    workspace_root: str
    failure_source: str
    failure_summary: str
    failure_sources: list[dict[str, Any]]
    context_references: list[str] = field(default_factory=list)
    recent_tail: list[dict[str, Any]] = field(default_factory=list)
    verification_log_refs: list[str] = field(default_factory=list)
    changed_files: list[str] = field(default_factory=list)
    evidence_refs: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)
    risk_points: list[dict[str, Any]] = field(default_factory=list)
    repair_policy: dict[str, Any] | None = None
    verification_strategies: list[dict[str, Any]] = field(default_factory=list)

    @classmethod
    def from_planner(
        cls,
        planner: Any,
        context: Any,
        *,
        failure_source: str,
        outcome: ExecutionOutcome | None = None,
        turn: int | None = None,
    ) -> FailureAnalysisRequest:
        state = getattr(planner, "state", None)
        evidence = getattr(planner, "evidence", None)
        task_id = str(getattr(state, "task_id", "") or getattr(planner, "task_id", "") or "")
        session_id = str(getattr(state, "session_id", "") or getattr(planner, "session_id", "") or "")
        phase_id = str(getattr(state, "current_phase", "") or "failure_analysis")
        failure_sources = _failure_sources(evidence, outcome=outcome)
        summary = _failure_summary(failure_sources, outcome=outcome)
        risk_points = list(getattr(state, "risk_points", None) or [])
        repair_policy = getattr(state, "repair_policy", None)
        verification_strategies = list(getattr(state, "verification_strategies", None) or [])
        return cls(
            request_id=f"failure_analysis_{uuid4().hex[:12]}",
            run_id=str(getattr(context, "run_id", "") or session_id or task_id),
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            workspace_root=str(getattr(planner, "workspace_root", "") or ""),
            failure_source=failure_source,
            failure_summary=summary,
            failure_sources=failure_sources,
            context_references=_context_references(context, evidence),
            recent_tail=_recent_tail(context),
            verification_log_refs=_verification_log_refs(evidence),
            changed_files=_changed_files(evidence),
            evidence_refs=_evidence_refs(evidence),
            metadata={"turn": turn} if turn is not None else {},
            risk_points=risk_points,
            repair_policy=repair_policy if isinstance(repair_policy, dict) else None,
            verification_strategies=verification_strategies,
        )

    @property
    def has_failure(self) -> bool:
        return bool(self.failure_sources or self.failure_summary)

    @property
    def fingerprint(self) -> str:
        payload: dict[str, Any] = {
            "summary": self.failure_summary,
            "failures": _fingerprint_sources(self.failure_sources),
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    @property
    def failure_evidence_refs(self) -> list[str]:
        refs: list[str] = []
        for source in self.failure_sources:
            _append_unique(refs, source.get("outcome_ref"))
            _append_unique(refs, source.get("tool_call_id"))
            _append_unique(refs, source.get("command_id"))
            _append_unique(refs, source.get("check_id"))
            evidence = source.get("evidence")
            if isinstance(evidence, dict):
                _append_unique(refs, evidence.get("command_id"))
                _append_unique(refs, evidence.get("artifact_ref"))
                _append_unique(refs, evidence.get("artifact_path"))
            assessment = source.get("completion_assessment")
            if isinstance(assessment, dict):
                for check_id in assessment.get("failed_checks") or []:
                    _append_unique(refs, check_id)
        return refs or self.evidence_refs

    @property
    def allowed_target_files(self) -> list[str]:
        files: list[str] = []
        for path in self.changed_files:
            _append_unique(files, _normalize_workspace_path(path, workspace_root=self.workspace_root))
        for source in self.failure_sources:
            for path in _paths_from_failure_source(source):
                _append_unique(files, _normalize_workspace_path(path, workspace_root=self.workspace_root))
        return files

    def to_model_payload(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "failure_source": self.failure_source,
            "failure_summary": self.failure_summary,
            "failure_sources": self.failure_sources[-8:],
            "context_references": self.context_references[-20:],
            "recent_tail": self.recent_tail[-8:],
            "verification_log_refs": self.verification_log_refs[-10:],
            "changed_files": self.changed_files[-30:],
            "allowed_target_files": self.allowed_target_files[-30:],
            "evidence_refs": self.evidence_refs[-30:],
            "risk_points": self.risk_points[-10:],
            "repair_policy": self.repair_policy,
            "verification_strategies": self.verification_strategies[-10:],
        }

    def to_dict(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "workspace_root": self.workspace_root,
            "failure_source": self.failure_source,
            "failure_summary": self.failure_summary,
            "failure_sources": self.failure_sources,
            "context_references": self.context_references,
            "recent_tail": self.recent_tail,
            "verification_log_refs": self.verification_log_refs,
            "changed_files": self.changed_files,
            "evidence_refs": self.evidence_refs,
            "metadata": self.metadata,
            "risk_points": self.risk_points,
            "repair_policy": self.repair_policy,
            "verification_strategies": self.verification_strategies,
        }

def _failure_sources(evidence: Any, *, outcome: ExecutionOutcome | None) -> list[dict[str, Any]]:
    sources: list[dict[str, Any]] = []
    if outcome is not None:
        sources.append({"kind": "execution_outcome", **_safe_outcome(outcome)})
    if evidence is None:
        return sources
    for item in list(getattr(evidence, "tool_results", []) or [])[-5:]:
        if item.get("ok") is False or item.get("error_code"):
            sources.append({"kind": "tool_result", **_trim_dict(item)})
    for item in list(getattr(evidence, "command_results", []) or [])[-5:]:
        if item.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}:
            sources.append({"kind": "command_observation", **_command_summary(item)})
    for item in list(getattr(evidence, "edit_results", []) or [])[-5:]:
        if item.get("error_code") or item.get("status") in {"failed", "blocked"}:
            sources.append({"kind": "edit_result", **_trim_dict(item)})
    latest_verification = (
        getattr(evidence, "verification_results", [])[-1]
        if getattr(evidence, "verification_results", None)
        else None
    )
    if isinstance(latest_verification, dict):
        assessment = latest_verification.get("completion_assessment") or {}
        if assessment.get("status") in {"failed", "blocked", "needs_review"}:
            sources.append(
                {
                    "kind": "verification_assessment",
                    "completion_assessment": _trim_dict(assessment),
                    "check_status": latest_verification.get("check_status") or [],
                }
            )
        for result in latest_verification.get("results") or []:
            if not isinstance(result, dict) or result.get("status") not in {
                "failed",
                "blocked",
                "timeout",
                "flaky",
            }:
                continue
            result_evidence = result.get("evidence") or {}
            sources.append(
                {
                    "kind": "verification_result",
                    "check_id": result.get("check_id"),
                    "status": result.get("status"),
                    "failure_type": result.get("failure_type"),
                    "evidence": _verification_evidence_summary(result_evidence),
                    "repair_hints": result.get("repair_hints") or [],
                }
            )
    for item in list(getattr(evidence, "review_results", []) or [])[-3:]:
        decision = item.get("decision") if isinstance(item, dict) else {}
        if isinstance(decision, dict) and decision.get("action") in {
            "repair",
            "reject",
            "needs_human_approval",
        }:
            sources.append({"kind": "review_observation", **_trim_dict(item)})
    for item in list(getattr(evidence, "unresolved_failures", []) or [])[-5:]:
        sources.append({"kind": "unresolved_failure", **_trim_dict(item)})
    return sources[-12:]


def _failure_summary(
    sources: list[dict[str, Any]],
    *,
    outcome: ExecutionOutcome | None,
) -> str:
    if outcome is not None and (outcome.observation_summary or outcome.reason):
        return _limit(outcome.observation_summary or outcome.reason, SUMMARY_LIMIT)
    if not sources:
        return ""
    first = sources[-1]
    return _limit(str(first.get("failure_type") or first.get("error_code") or first), SUMMARY_LIMIT)


def _fingerprint_sources(sources: list[dict[str, Any]]) -> list[dict[str, Any]]:
    fingerprints: list[dict[str, Any]] = []
    seen: set[str] = set()
    for source in sources:
        item = _fingerprint_source(source)
        key = json.dumps(item, ensure_ascii=False, sort_keys=True, default=str)
        if key in seen:
            continue
        seen.add(key)
        fingerprints.append(item)
    return fingerprints


def _fingerprint_source(source: dict[str, Any]) -> dict[str, Any]:
    evidence_raw = source.get("evidence")
    evidence: dict[str, Any] = evidence_raw if isinstance(evidence_raw, dict) else {}
    assessment_raw = source.get("completion_assessment")
    assessment: dict[str, Any] = assessment_raw if isinstance(assessment_raw, dict) else {}
    parsed_messages: list[str] = []
    for parsed in evidence.get("parsed_failures") or []:
        if isinstance(parsed, dict):
            _append_unique(parsed_messages, parsed.get("message"))
    repair_targets: list[str] = []
    for path in _paths_from_failure_source(source):
        _append_unique(repair_targets, _normalize_workspace_path(path, workspace_root=""))
    return {
        "kind": source.get("kind"),
        "outcome_ref": source.get("outcome_ref"),
        "status": source.get("status") or assessment.get("status"),
        "error_code": source.get("error_code"),
        "failure_type": source.get("failure_type"),
        "exit_code": source.get("exit_code") or evidence.get("exit_code"),
        "command_preview": _stable_command_preview(
            source.get("command_preview") or evidence.get("command")
        ),
        "parsed_messages": parsed_messages[:5],
        "repair_targets": repair_targets[:5],
    }


def _stable_command_preview(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, list | tuple):
        return " ".join(str(item) for item in value)
    text = str(value)
    text = text.replace("\\", "/")
    return re.sub(r"[A-Za-z]:/[^\\s\"']+", "<path>", text)


def _safe_outcome(outcome: ExecutionOutcome) -> dict[str, Any]:
    payload = outcome.to_dict()
    payload["outcome_ref"] = f"execution_outcome:{payload.get('error_code') or payload.get('status')}"
    payload["metadata"] = _trim_dict(payload.get("metadata") or {})
    return payload


def _command_summary(command: dict[str, Any]) -> dict[str, Any]:
    return {
        "command_id": command.get("command_id"),
        "command_preview": command.get("shell")
        or " ".join(str(item) for item in command.get("argv") or []),
        "exit_code": command.get("exit_code"),
        "status": command.get("semantic_status") or command.get("execution_status"),
        "stdout_preview": _limit(command.get("stdout_excerpt") or command.get("stdout") or "", SUMMARY_LIMIT),
        "stderr_preview": _limit(command.get("stderr_excerpt") or command.get("stderr") or "", SUMMARY_LIMIT),
        "output_ref": command.get("artifact_path") or command.get("output_ref"),
        "policy_decision_id": command.get("policy_decision_id"),
        "parsed_failures": command.get("parsed_failures") or [],
    }


def _verification_evidence_summary(evidence: dict[str, Any]) -> dict[str, Any]:
    return {
        "command_id": evidence.get("command_id"),
        "command": evidence.get("command"),
        "exit_code": evidence.get("exit_code"),
        "output_excerpt": _limit(evidence.get("output_excerpt") or "", SUMMARY_LIMIT),
        "stdout_excerpt": _limit(evidence.get("stdout_excerpt") or "", SUMMARY_LIMIT),
        "stderr_excerpt": _limit(evidence.get("stderr_excerpt") or "", SUMMARY_LIMIT),
        "artifact_ref": evidence.get("artifact_ref") or evidence.get("artifact_path"),
        "parsed_failures": (evidence.get("parsed_failures") or [])[:8],
        "sandbox_status": evidence.get("sandbox_status"),
        "sandbox_violations": evidence.get("sandbox_violations") or [],
        "capability_summary": evidence.get("capability_summary") or {},
    }


def _context_references(context: Any, evidence: Any) -> list[str]:
    refs: list[str] = []
    for observation in list(getattr(context, "tool_observations", []) or [])[-8:]:
        _append_unique(refs, getattr(observation, "id", None))
        for ref in getattr(observation, "source_refs", []) or []:
            _append_unique(refs, getattr(ref, "ref_id", None))
    if evidence is not None:
        for ref in _evidence_refs(evidence):
            _append_unique(refs, ref)
    return refs


def _recent_tail(context: Any) -> list[dict[str, Any]]:
    tail: list[dict[str, Any]] = []
    for observation in list(getattr(context, "tool_observations", []) or [])[-6:]:
        tail.append(
            {
                "source": "tool_observation",
                "tool_name": getattr(observation, "tool_name", None),
                "tool_call_id": getattr(observation, "tool_call_id", None),
                "ok": getattr(observation, "ok", None),
                "error_code": getattr(observation, "error_code", None),
                "preview": _limit(getattr(observation, "preview", "") or "", TAIL_LIMIT),
            }
        )
    try:
        messages = context.messages(persist=False)
    except Exception:
        messages = []
    for message in list(messages or [])[-4:]:
        tail.append(
            {
                "source": "message",
                "role": message.get("role"),
                "tool_call_id": message.get("tool_call_id"),
                "content_preview": _limit(message.get("content") or "", TAIL_LIMIT),
            }
        )
    return tail[-8:]


def _verification_log_refs(evidence: Any) -> list[str]:
    refs: list[str] = []
    if evidence is None:
        return refs
    for verification in list(getattr(evidence, "verification_results", []) or [])[-3:]:
        if not isinstance(verification, dict):
            continue
        for result in verification.get("results") or []:
            evidence_payload = result.get("evidence") if isinstance(result, dict) else {}
            if isinstance(evidence_payload, dict):
                _append_unique(refs, evidence_payload.get("artifact_ref"))
                _append_unique(refs, evidence_payload.get("artifact_path"))
                _append_unique(refs, evidence_payload.get("command_id"))
    return refs


def _changed_files(evidence: Any) -> list[str]:
    changed: list[str] = []
    if evidence is None:
        return changed
    for change in getattr(evidence, "applied_changes", []) or []:
        for path in change.get("changed_files") or []:
            _append_unique(changed, path)
    return changed


def _evidence_refs(evidence: Any) -> list[str]:
    refs: list[str] = []
    if evidence is None:
        return refs
    for result in getattr(evidence, "tool_results", []) or []:
        _append_unique(refs, result.get("tool_call_id"))
    for command in getattr(evidence, "command_results", []) or []:
        _append_unique(refs, command.get("command_id"))
    for verification in getattr(evidence, "verification_results", []) or []:
        for status in verification.get("check_status") or []:
            _append_unique(refs, status.get("check_id"))
    for review in getattr(evidence, "review_results", []) or []:
        _append_unique(refs, review.get("review_id"))
    for change in getattr(evidence, "applied_changes", []) or []:
        _append_unique(refs, change.get("transaction_id"))
        _append_unique(refs, change.get("changeset_id"))
    return refs
