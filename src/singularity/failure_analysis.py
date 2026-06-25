from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from typing import Any
from uuid import uuid4

from singularity.execution_outcome import ExecutionOutcome, ExecutionOutcomeStatus
from singularity.model import (
    ContentBlock,
    ModelBudget,
    ModelMessage,
    ModelPreferences,
    ModelPurpose,
    ModelRole,
    ModelTurnRequest,
    ModelTurnStatus,
    ToolChoiceMode,
    ToolChoicePolicy,
)
from singularity.observability.models import TraceEventType, TraceSeverity

SUMMARY_LIMIT = 700
TAIL_LIMIT = 400


@dataclass(frozen=True)
class FailureAnalysisRequest:
    request_id: str
    run_id: str
    session_id: str
    task_id: str
    phase_id: str
    failure_source: str
    failure_summary: str
    failure_sources: list[dict[str, Any]]
    context_references: list[str] = field(default_factory=list)
    recent_tail: list[dict[str, Any]] = field(default_factory=list)
    verification_log_refs: list[str] = field(default_factory=list)
    changed_files: list[str] = field(default_factory=list)
    evidence_refs: list[str] = field(default_factory=list)
    metadata: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_planner(
        cls,
        planner: Any,
        context: Any,
        *,
        failure_source: str,
        outcome: ExecutionOutcome | None = None,
        turn: int | None = None,
    ) -> "FailureAnalysisRequest":
        state = getattr(planner, "state", None)
        evidence = getattr(planner, "evidence", None)
        task_id = str(getattr(state, "task_id", "") or getattr(planner, "task_id", "") or "")
        session_id = str(getattr(state, "session_id", "") or getattr(planner, "session_id", "") or "")
        phase_id = str(getattr(state, "current_phase", "") or "failure_analysis")
        failure_sources = _failure_sources(evidence, outcome=outcome)
        summary = _failure_summary(failure_sources, outcome=outcome)
        return cls(
            request_id=f"failure_analysis_{uuid4().hex[:12]}",
            run_id=str(getattr(context, "run_id", "") or session_id or task_id),
            session_id=session_id,
            task_id=task_id,
            phase_id=phase_id,
            failure_source=failure_source,
            failure_summary=summary,
            failure_sources=failure_sources,
            context_references=_context_references(context, evidence),
            recent_tail=_recent_tail(context),
            verification_log_refs=_verification_log_refs(evidence),
            changed_files=_changed_files(evidence),
            evidence_refs=_evidence_refs(evidence),
            metadata={"turn": turn} if turn is not None else {},
        )

    @property
    def has_failure(self) -> bool:
        return bool(self.failure_sources or self.failure_summary)

    @property
    def fingerprint(self) -> str:
        payload: dict[str, Any] = {
            "source": self.failure_source,
            "summary": self.failure_summary,
            "refs": self.failure_evidence_refs,
            "failures": self.failure_sources,
        }
        return json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)

    @property
    def failure_evidence_refs(self) -> list[str]:
        refs: list[str] = []
        for source in self.failure_sources:
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
            "evidence_refs": self.evidence_refs[-30:],
        }

    def to_dict(self) -> dict[str, Any]:
        return {
            "request_id": self.request_id,
            "run_id": self.run_id,
            "session_id": self.session_id,
            "task_id": self.task_id,
            "phase_id": self.phase_id,
            "failure_source": self.failure_source,
            "failure_summary": self.failure_summary,
            "failure_sources": self.failure_sources,
            "context_references": self.context_references,
            "recent_tail": self.recent_tail,
            "verification_log_refs": self.verification_log_refs,
            "changed_files": self.changed_files,
            "evidence_refs": self.evidence_refs,
            "metadata": self.metadata,
        }


@dataclass(frozen=True)
class FailureAnalysisResult:
    analysis_id: str
    request_id: str
    root_cause: str
    failure_category: str
    affected_files: list[str]
    evidence_refs: list[str]
    repair_strategy: str
    next_actions: list[str]
    verification_plan: list[str]
    confidence: float
    needs_user_input: bool
    blocked_reason: str | None = None
    raw_response_ref: str | None = None

    @classmethod
    def from_model_payload(
        cls,
        payload: dict[str, Any],
        *,
        request: FailureAnalysisRequest,
        raw_response_ref: str | None = None,
    ) -> "FailureAnalysisResult":
        affected = _strings(payload.get("affected_files")) or list(request.changed_files)
        return cls(
            analysis_id=str(payload.get("analysis_id") or f"failure_{uuid4().hex[:12]}"),
            request_id=request.request_id,
            root_cause=_text(payload.get("root_cause") or request.failure_summary),
            failure_category=_text(payload.get("failure_category") or "unknown_failure"),
            affected_files=affected[:20],
            evidence_refs=(_strings(payload.get("evidence_refs")) or request.evidence_refs)[:30],
            repair_strategy=_text(payload.get("repair_strategy") or "repair_then_verify"),
            next_actions=_strings(payload.get("next_actions"))[:12],
            verification_plan=_strings(payload.get("verification_plan"))[:12],
            confidence=_confidence(payload.get("confidence")),
            needs_user_input=bool(payload.get("needs_user_input")),
            blocked_reason=payload.get("blocked_reason"),
            raw_response_ref=raw_response_ref,
        )

    @classmethod
    def blocked(
        cls,
        *,
        request: FailureAnalysisRequest,
        reason: str,
        category: str = "failure_analysis_unavailable",
    ) -> "FailureAnalysisResult":
        return cls(
            analysis_id=f"failure_{uuid4().hex[:12]}",
            request_id=request.request_id,
            root_cause=reason,
            failure_category=category,
            affected_files=list(request.changed_files),
            evidence_refs=list(request.evidence_refs),
            repair_strategy="blocked",
            next_actions=[reason],
            verification_plan=[],
            confidence=0.0,
            needs_user_input=True,
            blocked_reason=reason,
        )

    def to_dict(self) -> dict[str, Any]:
        root = {
            "description": self.root_cause,
            "evidence": self.evidence_refs,
            "confidence": self.confidence,
        }
        return {
            "analysis_id": self.analysis_id,
            "request_id": self.request_id,
            "root_cause": root,
            "root_cause_text": self.root_cause,
            "failure_category": self.failure_category,
            "failure_type": self.failure_category,
            "affected_files": self.affected_files,
            "suspect_files": self.affected_files,
            "evidence_refs": self.evidence_refs,
            "repair_strategy": self.repair_strategy,
            "next_actions": self.next_actions,
            "verification_plan": self.verification_plan,
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
            "raw_response_ref": self.raw_response_ref,
        }


@dataclass(frozen=True)
class RepairActionCandidate:
    candidate_id: str
    action_type: str
    target_file: str | None
    rationale: str
    tool_hints: list[str]
    verification_ref: str | None = None
    confidence: float = 0.5

    def to_dict(self) -> dict[str, Any]:
        return {
            "candidate_id": self.candidate_id,
            "action_type": self.action_type,
            "target_file": self.target_file,
            "rationale": self.rationale,
            "tool_hints": self.tool_hints,
            "verification_ref": self.verification_ref,
            "confidence": self.confidence,
        }


@dataclass(frozen=True)
class RepairPlan:
    plan_id: str
    analysis_id: str
    strategy: str
    summary: str
    action_candidates: list[RepairActionCandidate]
    next_actions: list[str]
    verification_plan: list[str]
    evidence_refs: list[str]
    confidence: float
    needs_user_input: bool = False
    blocked_reason: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "plan_id": self.plan_id,
            "analysis_id": self.analysis_id,
            "strategy": self.strategy,
            "summary": self.summary,
            "action_candidates": [item.to_dict() for item in self.action_candidates],
            "steps": [item.to_dict() for item in self.action_candidates],
            "next_actions": self.next_actions,
            "next_verification": {"commands": self.verification_plan},
            "verification_plan": self.verification_plan,
            "evidence_refs": self.evidence_refs,
            "confidence": self.confidence,
            "needs_user_input": self.needs_user_input,
            "blocked_reason": self.blocked_reason,
        }


class FailureAnalyzer:
    def __init__(self, *, model_runner: Any, trace: Any | None = None) -> None:
        self.model_runner = model_runner
        self.trace = trace

    def analyze(self, request: FailureAnalysisRequest) -> FailureAnalysisResult:
        self._record("requested", request=request)
        model_request = self._model_request(request)
        result = self.model_runner.run_turn(model_request)
        if result.status != ModelTurnStatus.SUCCESS or result.assistant_message is None:
            reason = (
                result.error.message
                if result.error is not None
                else "failure_analysis_model_request_failed"
            )
            self._record("failed", request=request, error=reason)
            return FailureAnalysisResult.blocked(request=request, reason=reason)
        try:
            payload = _json_payload(result.assistant_message.text)
        except ValueError as exc:
            self._record("failed", request=request, error=str(exc))
            return FailureAnalysisResult.blocked(
                request=request,
                reason=f"failure_analysis_invalid_json: {exc}",
            )
        analysis = FailureAnalysisResult.from_model_payload(
            payload,
            request=request,
            raw_response_ref=result.raw_response_ref,
        )
        self._record("completed", request=request, analysis=analysis)
        return analysis

    def _model_request(self, request: FailureAnalysisRequest) -> ModelTurnRequest:
        payload = request.to_model_payload()
        prompt = (
            "Analyze this structured failure summary and produce a repair plan. "
            "Use only the supplied summaries, references, recent tail, verification "
            "log refs, and changed file names. Do not ask for raw logs unless a "
            "reference is missing. Return JSON only with keys: root_cause, "
            "failure_category, affected_files, evidence_refs, repair_strategy, "
            "next_actions, verification_plan, confidence, needs_user_input, "
            "blocked_reason.\n\n"
            + json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str)
        )
        return ModelTurnRequest(
            request_id=request.request_id,
            run_id=request.run_id,
            session_id=request.session_id,
            task_id=request.task_id,
            phase_id="failure_analysis",
            action_id=request.request_id,
            purpose=ModelPurpose.FAILURE_ANALYSIS,
            messages=[
                ModelMessage(
                    role=ModelRole.USER,
                    content=[ContentBlock.from_text(prompt)],
                )
            ],
            tools=[],
            tool_choice=ToolChoicePolicy(mode=ToolChoiceMode.NONE, max_tool_calls=0),
            model_preferences=ModelPreferences(json_mode=True, max_output_tokens=1200),
            budget=ModelBudget(max_retries=1, max_output_tokens=1200),
            context_metadata={
                "input_policy": "structured_failure_summaries_only",
                "repair_planning": True,
            },
        )

    def _record(
        self,
        status: str,
        *,
        request: FailureAnalysisRequest,
        analysis: FailureAnalysisResult | None = None,
        error: str | None = None,
    ) -> None:
        event = f"failure_analysis_{status}"
        payload: dict[str, Any] = {
            "request_id": request.request_id,
            "failure_source": request.failure_source,
            "evidence_refs": request.evidence_refs,
            "changed_files": request.changed_files,
        }
        if analysis is not None:
            payload["analysis"] = analysis.to_dict()
        if error:
            payload["error"] = error
        if self.trace is None:
            return
        if hasattr(self.trace, "emit"):
            event_type = {
                "requested": TraceEventType.FAILURE_ANALYSIS_REQUESTED,
                "completed": TraceEventType.FAILURE_ANALYSIS_COMPLETED,
                "failed": TraceEventType.FAILURE_ANALYSIS_FAILED,
            }[status]
            self.trace.emit(
                event_type,
                component="failure_analysis",
                summary=f"Failure analysis {status}.",
                payload=payload,
                ids={
                    "run_id": request.run_id,
                    "session_id": request.session_id,
                    "task_id": request.task_id,
                    "phase_id": "failure_analysis",
                    "action_id": request.request_id,
                },
                severity=TraceSeverity.ERROR if status == "failed" else TraceSeverity.INFO,
            )
        elif hasattr(self.trace, "record"):
            self.trace.record(event, payload)


class RepairPlanner:
    blocked_categories = {
        "approval_required",
        "permission_denied",
        "policy_blocked",
        "policy_denied",
        "risk_escalated",
        "sandbox_required",
        "missing_information",
        "user_input_required",
    }

    def plan(self, analysis: FailureAnalysisResult) -> RepairPlan:
        blocked = (
            analysis.blocked_reason
            or (
                analysis.failure_category
                if analysis.failure_category in self.blocked_categories
                else None
            )
        )
        if blocked or analysis.needs_user_input:
            return RepairPlan(
                plan_id=f"repair_{uuid4().hex[:12]}",
                analysis_id=analysis.analysis_id,
                strategy="blocked",
                summary=analysis.repair_strategy or analysis.root_cause,
                action_candidates=[],
                next_actions=analysis.next_actions,
                verification_plan=analysis.verification_plan,
                evidence_refs=analysis.evidence_refs,
                confidence=analysis.confidence,
                needs_user_input=True,
                blocked_reason=blocked or "user_input_required",
            )
        candidates = _action_candidates(analysis)
        return RepairPlan(
            plan_id=f"repair_{uuid4().hex[:12]}",
            analysis_id=analysis.analysis_id,
            strategy=analysis.repair_strategy or "repair_then_verify",
            summary=analysis.root_cause,
            action_candidates=candidates,
            next_actions=analysis.next_actions,
            verification_plan=analysis.verification_plan,
            evidence_refs=analysis.evidence_refs,
            confidence=analysis.confidence,
        )

    def to_replan_signal(
        self,
        *,
        request: FailureAnalysisRequest,
        analysis: FailureAnalysisResult,
        plan: RepairPlan,
    ) -> dict[str, Any]:
        return {
            "error_code": analysis.failure_category or "repair_planned",
            "failure_fingerprint": request.fingerprint,
            "verification_failed": True,
            "repair_plan_id": plan.plan_id,
            "analysis_id": analysis.analysis_id,
            "action_hints": [item.to_dict() for item in plan.action_candidates],
            "needs_user_input": plan.needs_user_input,
        }

    @staticmethod
    def blocked_outcome(plan: RepairPlan) -> ExecutionOutcome:
        return ExecutionOutcome(
            status=ExecutionOutcomeStatus.USER_INPUT_REQUIRED,
            source="failure_analysis",
            reason=plan.blocked_reason or "failure_analysis_requires_user_input",
            error_code="failure_analysis_user_input_required",
            next_action="ask_user",
            observation_summary=plan.summary,
            retry_allowed=False,
            metadata={"repair_plan": plan.to_dict()},
        )


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


def _safe_outcome(outcome: ExecutionOutcome) -> dict[str, Any]:
    payload = outcome.to_dict()
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


def _action_candidates(analysis: FailureAnalysisResult) -> list[RepairActionCandidate]:
    actions = analysis.next_actions or [analysis.repair_strategy]
    files: list[str | None] = list(analysis.affected_files) or [None]
    candidates: list[RepairActionCandidate] = []
    for index, action in enumerate(actions[:6]):
        lowered = action.lower()
        action_type = "analyze"
        tool_hints = ["read_file", "search_text"]
        if any(marker in lowered for marker in ("patch", "edit", "fix", "repair", "修改", "修复")):
            action_type = "edit"
            tool_hints = ["read_file", "apply_patch", "write_file", "inspect_diff"]
        elif any(marker in lowered for marker in ("verify", "rerun", "test", "pytest", "验证", "测试")):
            action_type = "verify"
            tool_hints = ["run_verification", "get_verification_result"]
        elif any(marker in lowered for marker in ("read", "inspect", "open", "查", "读")):
            action_type = "inspect"
        candidates.append(
            RepairActionCandidate(
                candidate_id=f"candidate_{uuid4().hex[:12]}",
                action_type=action_type,
                target_file=files[min(index, len(files) - 1)],
                rationale=action,
                tool_hints=tool_hints,
                verification_ref=analysis.verification_plan[0] if analysis.verification_plan else None,
                confidence=analysis.confidence,
            )
        )
    return candidates


def _json_payload(text: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        match = re.search(r"\{.*\}", text, flags=re.DOTALL)
        if not match:
            raise ValueError("model response did not contain a JSON object")
        value = json.loads(match.group(0))
    if not isinstance(value, dict):
        raise ValueError("model response JSON was not an object")
    return value


def _trim_dict(value: dict[str, Any]) -> dict[str, Any]:
    return {
        str(key): _limit(item, SUMMARY_LIMIT) if isinstance(item, str) else item
        for key, item in value.items()
        if key not in {"raw_output", "raw_stdout", "raw_stderr", "content"}
    }


def _strings(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    if isinstance(value, list | tuple | set):
        return [str(item) for item in value if item is not None]
    return [str(value)]


def _text(value: Any) -> str:
    return _limit(str(value or ""), SUMMARY_LIMIT)


def _confidence(value: Any) -> float:
    try:
        return max(0.0, min(1.0, float(value)))
    except (TypeError, ValueError):
        return 0.5


def _limit(value: Any, limit: int) -> str:
    text = str(value or "")
    return text if len(text) <= limit else text[:limit] + "...[truncated]"


def _append_unique(values: list[str], value: Any) -> None:
    if value is None:
        return
    text = str(value)
    if text and text not in values:
        values.append(text)
