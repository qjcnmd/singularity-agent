from __future__ import annotations

import json
from typing import Any

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

from .request import FailureAnalysisRequest
from .result import FailureAnalysisResult, _json_payload


class FailureAnalyzer:
    def __init__(self, *, model_runner: Any, trace: Any | None = None) -> None:
        self.model_runner = model_runner
        self.trace = trace

    def analyze(self, request: FailureAnalysisRequest) -> FailureAnalysisResult:
        self._record("requested", request=request)
        sandbox_blocker = _sandbox_backend_blocker(request)
        if sandbox_blocker is not None:
            analysis = FailureAnalysisResult.blocked(
                request=request,
                reason=sandbox_blocker,
                category="sandbox_limitation",
                affected_files=[],
            )
            self._record("completed", request=request, analysis=analysis)
            return analysis
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
                category="failure_analysis_invalid_json",
            )
        try:
            analysis = FailureAnalysisResult.from_model_payload(
                payload,
                request=request,
                raw_response_ref=result.raw_response_ref,
            )
        except ValueError as exc:
            self._record("failed", request=request, error=str(exc))
            return FailureAnalysisResult.blocked(
                request=request,
                reason=f"failure_analysis_schema_invalid: {exc}",
                category="failure_analysis_schema_invalid",
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


def _sandbox_backend_blocker(request: FailureAnalysisRequest) -> str | None:
    for source in request.failure_sources:
        evidence = source.get("evidence") if isinstance(source, dict) else {}
        evidence_payload = evidence if isinstance(evidence, dict) else {}
        capability = evidence_payload.get("capability_summary")
        capability_payload = capability if isinstance(capability, dict) else {}
        backend_unavailable = (
            source.get("error_code") in {"backend_unavailable", "sandbox_unavailable"}
            or evidence_payload.get("sandbox_status") == "backend_unavailable"
            or evidence_payload.get("enforcement_status") == "backend_unavailable"
            or capability_payload.get("backend_status") == "backend_unavailable"
        )
        if source.get("failure_type") == "sandbox_limitation" and backend_unavailable:
            return "sandbox backend unavailable: run elevated sandbox setup before verification can proceed"
    return None
