from __future__ import annotations

from pathlib import Path
from typing import Any

from miniharness.observability.models import TraceEventType, TraceSeverity
from miniharness.review.critic import ModelCritic
from miniharness.review.decision import ReviewDecisionEngine
from miniharness.review.evidence import collect_review_evidence, to_bounded_plain
from miniharness.review.findings import RuleFindingCollector
from miniharness.review.models import (
    ReviewDecisionAction,
    ReviewEvidence,
    ReviewFinding,
    ReviewReport,
    ReviewStage,
    ReviewTarget,
)
from miniharness.review.policy import extract_policy_context


class ReviewRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        trace: Any | None = None,
        project_index_runtime: Any | None = None,
        policy_runtime: Any | None = None,
        model_runtime: Any | None = None,
        memory_runtime: Any | None = None,
        planner: Any | None = None,
        enable_model_critic: bool = True,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.trace = trace
        self.project_index_runtime = project_index_runtime
        self.policy_runtime = policy_runtime
        self.model_runtime = model_runtime
        self.memory_runtime = memory_runtime
        self.planner = planner
        self.enable_model_critic = enable_model_critic
        self.finding_collector = RuleFindingCollector()
        self.decision_engine = ReviewDecisionEngine()
        self.cancellation_token: Any | None = None

    def pre_edit_review(
        self,
        *,
        intent: Any | None = None,
        plan: Any | None = None,
        patch: Any | None = None,
        validation: Any | None = None,
        code_impact: Any | None = None,
        test_impact: Any | None = None,
        policy_observation: dict[str, Any] | None = None,
        trace_summary: Any | None = None,
        task_id: str | None = None,
        plan_id: str | None = None,
    ) -> ReviewReport:
        context = self._context(
            intent=intent,
            plan=plan,
            patch=patch,
            validation=validation,
            code_impact=code_impact,
            test_impact=test_impact,
            policy_observation=policy_observation,
        )
        target = ReviewTarget(
            stage=ReviewStage.PRE_EDIT,
            task_id=task_id or _attr(intent, "task_id"),
            plan_id=plan_id or _attr(plan, "id"),
            edit_intent_id=_attr(intent, "id") or context.get("intent_id"),
            edit_plan_id=_attr(plan, "id") or context.get("edit_plan_id"),
            patch_id=_attr(patch, "id") or context.get("patch_id"),
            patch_digest=_attr(patch, "digest") or context.get("patch_digest"),
            policy_decision_id=(policy_observation or {}).get("decision_id") if isinstance(policy_observation, dict) else None,
        )
        evidence = collect_review_evidence(
            intent=intent,
            edit_plan=plan,
            patch=patch,
            validation=validation,
            code_impact=code_impact,
            test_impact=test_impact,
            policy_observation=policy_observation,
            trace_summary=trace_summary,
        )
        return self._review(target=target, evidence=evidence, context=context)

    def post_patch_review(
        self,
        *,
        edit_result: Any | None = None,
        mutation_result: Any | None = None,
        verification_plan: Any | None = None,
        code_impact: Any | None = None,
        test_impact: Any | None = None,
        policy_observation: dict[str, Any] | None = None,
        trace_summary: Any | None = None,
    ) -> ReviewReport:
        context = self._context(
            edit_result=edit_result,
            mutation_result=mutation_result,
            verification_plan=verification_plan,
            code_impact=code_impact,
            test_impact=test_impact,
            policy_observation=policy_observation,
        )
        edit_payload = to_bounded_plain(edit_result)
        edit_payload = edit_payload if isinstance(edit_payload, dict) else {}
        target = ReviewTarget(
            stage=ReviewStage.POST_PATCH,
            task_id=_planner_attr(self.planner, "task_id"),
            plan_id=(edit_payload.get("edit_plan_id") or edit_payload.get("plan_id")),
            edit_intent_id=edit_payload.get("intent_id"),
            edit_plan_id=edit_payload.get("edit_plan_id"),
            patch_id=edit_payload.get("patch_candidate_id"),
            patch_digest=edit_payload.get("patch_digest"),
            edit_result_id=edit_payload.get("edit_result_id"),
            changeset_id=edit_payload.get("changeset_id"),
            transaction_id=edit_payload.get("transaction_id"),
            policy_decision_id=(policy_observation or {}).get("decision_id") if isinstance(policy_observation, dict) else None,
        )
        evidence = collect_review_evidence(
            edit_result=edit_result,
            mutation_result=mutation_result,
            verification_plan=verification_plan,
            code_impact=code_impact,
            test_impact=test_impact,
            policy_observation=policy_observation,
            trace_summary=trace_summary,
        )
        return self._review(target=target, evidence=evidence, context=context)

    def post_verification_review(
        self,
        *,
        plan: Any | None = None,
        results: Any | None = None,
        assessment: Any | None = None,
        verification: Any | None = None,
        observation: Any | None = None,
        policy_observation: dict[str, Any] | None = None,
        trace_summary: Any | None = None,
    ) -> ReviewReport:
        verification_payload = _verification_payload(
            verification=verification,
            observation=observation,
            plan=plan,
            results=results,
            assessment=assessment,
        )
        context = self._context(
            verification=verification_payload,
            policy_observation=policy_observation,
        )
        plan_payload = verification_payload.get("plan") if isinstance(verification_payload.get("plan"), dict) else {}
        target = ReviewTarget(
            stage=ReviewStage.POST_VERIFICATION,
            task_id=_planner_attr(self.planner, "task_id"),
            verification_id=plan_payload.get("verification_plan_id") or verification_payload.get("verification_plan_id"),
            changeset_id=plan_payload.get("changeset_id"),
            transaction_id=plan_payload.get("transaction_id"),
            policy_decision_id=(policy_observation or {}).get("decision_id") if isinstance(policy_observation, dict) else None,
        )
        evidence = collect_review_evidence(
            verification=verification_payload,
            policy_observation=policy_observation,
            trace_summary=trace_summary,
        )
        return self._review(target=target, evidence=evidence, context=context)

    def final_review(
        self,
        *,
        task_state: Any | None = None,
        task_plan: Any | None = None,
        evidence_ledger: Any | None = None,
        trace_summary: Any | None = None,
    ) -> ReviewReport:
        ledger = to_bounded_plain(evidence_ledger)
        ledger = ledger if isinstance(ledger, dict) else {}
        latest_verification = None
        if isinstance(ledger.get("verification_results"), list) and ledger["verification_results"]:
            latest_verification = ledger["verification_results"][-1]
        context = self._context(
            task_state=task_state,
            task_plan=task_plan,
            evidence_ledger=ledger,
            verification=latest_verification,
        )
        target = ReviewTarget(
            stage=ReviewStage.FINAL,
            task_id=_attr(task_state, "task_id") or _planner_attr(self.planner, "task_id"),
            plan_id=_attr(task_plan, "plan_id"),
        )
        evidence = collect_review_evidence(
            task_state=task_state,
            task_plan=task_plan,
            evidence_ledger=ledger,
            trace_summary=trace_summary,
        )
        return self._review(target=target, evidence=evidence, context=context)

    def health(self) -> dict[str, Any]:
        return {
            "status": "ok",
            "workspace_root": str(self.workspace_root),
            "model_critic_enabled": self.enable_model_critic,
            "has_model_runtime": self.model_runtime is not None,
        }

    def _review(
        self,
        *,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> ReviewReport:
        self._throw_if_cancelled()
        started_ids = self._emit_started(target=target, evidence=evidence)
        self._throw_if_cancelled()
        findings = self.finding_collector.collect(target=target, evidence=evidence, context=context)
        rule_decision = self.decision_engine.decide(target=target, findings=findings, context=context)
        report = ReviewReport(
            target=target,
            input_summary=self._input_summary(target=target, evidence=evidence, context=context),
            evidence=evidence,
            findings=findings,
            decision=rule_decision,
            next_actions=list(rule_decision.next_actions),
            trace_event_ids=started_ids,
        )
        if self.enable_model_critic:
            self._throw_if_cancelled()
            critic = ModelCritic(self.model_runtime)
            outcome = critic.review(
                report,
                bundle={"target": target.model_dump(mode="json"), "context": context},
                request_context=self._critic_request_context(target),
            )
            self._throw_if_cancelled()
            report.model_critic_status = outcome.status
            report.model_critic_error = outcome.error
            if outcome.status != "ok":
                report.evidence.append(
                    ReviewEvidence(
                        source=outcome.status,
                        summary=f"Model critic status: {outcome.status}.",
                        payload={
                            "status": outcome.status,
                            "error": outcome.error,
                        },
                        payload_hash=_stable_review_hash(
                            {"status": outcome.status, "error": outcome.error}
                        ),
                        trust_level="model_derived",
                    )
                )
            if outcome.findings:
                report.findings.extend(outcome.findings)
                report.decision = self.decision_engine.decide(target=target, findings=report.findings, context=context)
                report.next_actions = list(report.decision.next_actions)
        else:
            report.model_critic_status = "disabled"

        report.trace_event_ids.extend(self._emit_findings(report))
        report.trace_event_ids.extend(self._emit_decision(report))
        report.trace_event_ids.extend(self._emit_completed(report))
        self._record_planner(report)
        self._record_memory(report)
        return report

    def _context(self, **values: Any) -> dict[str, Any]:
        context: dict[str, Any] = {}
        for key, value in values.items():
            if value is None:
                continue
            plain = to_bounded_plain(value)
            if key == "edit_result" and isinstance(plain, dict):
                context.update(
                    {
                        "changed_files": plain.get("changed_files") or [],
                        "code_impact": plain.get("code_impact"),
                        "test_impact": plain.get("test_impact"),
                        "transaction_id": plain.get("transaction_id"),
                    }
                )
            elif key == "mutation_result" and isinstance(plain, dict):
                context.setdefault("changed_files", plain.get("affected_files") or plain.get("changed_files") or [])
                context.setdefault("transaction_id", plain.get("transaction_id"))
            elif key == "verification" and isinstance(plain, dict):
                context["verification"] = plain
                assessment = plain.get("completion_assessment") if isinstance(plain.get("completion_assessment"), dict) else {}
                context["verification_status"] = assessment.get("status") or plain.get("status")
                context.update(_verification_check_groups(plain))
            elif key == "policy_observation" and isinstance(plain, dict):
                context[key] = plain
                context.update(extract_policy_context(plain))
            elif isinstance(plain, dict):
                context[key] = plain
                if key == "validation":
                    context["validation"] = plain
                elif key == "code_impact":
                    context["code_impact"] = plain
                elif key == "test_impact":
                    context["test_impact"] = plain
            else:
                context[key] = plain
        return context

    def _input_summary(
        self,
        *,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> str:
        changed_files = context.get("changed_files") or []
        status = context.get("verification_status") or context.get("policy_outcome") or ""
        suffix = f" changed_files={len(changed_files)}" if changed_files else ""
        status_part = f" status={status}" if status else ""
        return f"{target.stage.value} review with {len(evidence)} evidence item(s).{suffix}{status_part}"

    def _emit_started(self, *, target: ReviewTarget, evidence: list[ReviewEvidence]) -> list[str]:
        return self._emit(
            TraceEventType.REVIEW_STARTED,
            summary=f"Review started for {target.stage.value}.",
            payload={
                "target": target.model_dump(mode="json"),
                "evidence_count": len(evidence),
                "evidence_hashes": [item.payload_hash for item in evidence],
            },
            severity=TraceSeverity.INFO,
            target=target,
        )

    def _emit_findings(self, report: ReviewReport) -> list[str]:
        event_ids: list[str] = []
        for finding in report.findings:
            event_ids.extend(
                self._emit(
                    TraceEventType.REVIEW_FINDING,
                    summary=finding.title,
                    payload=finding.model_dump(mode="json"),
                    severity=_severity(finding),
                    target=report.target,
                )
            )
        return event_ids

    def _emit_decision(self, report: ReviewReport) -> list[str]:
        return self._emit(
            TraceEventType.REVIEW_DECISION,
            summary=f"Review decision: {report.decision.action.value}.",
            payload=report.decision.model_dump(mode="json"),
            severity=TraceSeverity.WARNING
            if report.decision.action != ReviewDecisionAction.ACCEPT
            else TraceSeverity.INFO,
            target=report.target,
        )

    def _emit_completed(self, report: ReviewReport) -> list[str]:
        return self._emit(
            TraceEventType.REVIEW_COMPLETED,
            summary=f"Review completed for {report.target.stage.value}.",
            payload={
                "review_id": report.review_id,
                "decision": report.decision.action.value,
                "finding_count": len(report.findings),
                "blocking_count": len(report.blocking_findings),
                "model_critic_status": report.model_critic_status,
            },
            severity=TraceSeverity.INFO,
            target=report.target,
        )

    def _emit(
        self,
        event_type: TraceEventType,
        *,
        summary: str,
        payload: dict[str, Any],
        severity: TraceSeverity,
        target: ReviewTarget,
    ) -> list[str]:
        if self.trace is None:
            return []
        ids = {
            "session_id": _planner_attr(self.planner, "session_id"),
            "task_id": target.task_id or _planner_attr(self.planner, "task_id"),
            "phase_id": getattr(getattr(self.planner, "state", None), "current_phase", None),
            "action_id": target.edit_result_id or target.patch_id or target.verification_id,
            "transaction_id": target.transaction_id,
            "verification_id": target.verification_id,
            "policy_decision_id": target.policy_decision_id,
        }
        if hasattr(self.trace, "emit"):
            event = self.trace.emit(
                event_type,
                runtime="review",
                summary=summary,
                payload=payload,
                ids=ids,
                severity=severity,
            )
            event_id = getattr(event, "event_id", None)
            return [event_id] if event_id else []
        if hasattr(self.trace, "record"):
            self.trace.record("review", {"event_type": event_type.value, "summary": summary, **payload, **ids})
        return []

    def _record_planner(self, report: ReviewReport) -> None:
        if self.planner is None or not hasattr(self.planner, "record_review_observation"):
            return
        self.planner.record_review_observation(report.model_dump(mode="json"))

    def _record_memory(self, report: ReviewReport) -> None:
        if self.memory_runtime is None or not hasattr(self.memory_runtime, "ingest_review_report"):
            return
        try:
            self.memory_runtime.ingest_review_report(report)
        except Exception as exc:
            report.evidence.append(
                ReviewEvidence(
                    source="memory_ingest",
                    summary="Memory ingest failed; review result was preserved.",
                    payload={"type": type(exc).__name__, "message": str(exc)},
                    payload_hash=_stable_review_hash(
                        {"type": type(exc).__name__, "message": str(exc)}
                    ),
                    trust_level="trusted_runtime",
                )
            )
            self._emit(
                TraceEventType.REVIEW_FINDING,
                summary="Review memory ingest failed open.",
                payload={"type": type(exc).__name__, "message": str(exc)},
                severity=TraceSeverity.WARNING,
                target=report.target,
            )

    def _throw_if_cancelled(self) -> None:
        token = getattr(self, "cancellation_token", None)
        if token is not None and hasattr(token, "throw_if_cancelled"):
            token.throw_if_cancelled()

    def _critic_request_context(self, target: ReviewTarget) -> dict[str, Any]:
        return {
            "run_id": getattr(self.trace, "run_id", None),
            "session_id": _planner_attr(self.planner, "session_id"),
            "task_id": target.task_id or _planner_attr(self.planner, "task_id"),
            "phase_id": getattr(getattr(self.planner, "state", None), "current_phase", None) or target.stage.value,
            "action_id": target.edit_result_id or target.patch_id or target.verification_id,
        }


def _severity(finding: ReviewFinding) -> TraceSeverity:
    mapping = {
        "critical": TraceSeverity.CRITICAL,
        "error": TraceSeverity.ERROR,
        "warning": TraceSeverity.WARNING,
        "info": TraceSeverity.INFO,
    }
    return mapping.get(finding.severity.value, TraceSeverity.INFO)


def _attr(value: Any, name: str) -> Any:
    if value is None:
        return None
    if hasattr(value, name):
        return getattr(value, name)
    if isinstance(value, dict):
        return value.get(name)
    return None


def _planner_attr(planner: Any | None, name: str) -> Any:
    return getattr(planner, name, None) if planner is not None else None


def _verification_payload(
    *,
    verification: Any | None,
    observation: Any | None,
    plan: Any | None,
    results: Any | None,
    assessment: Any | None,
) -> dict[str, Any]:
    if verification is not None:
        payload = to_bounded_plain(verification)
        return payload if isinstance(payload, dict) else {"value": payload}
    if observation is not None:
        payload = to_bounded_plain(observation)
        if isinstance(payload, dict) and isinstance(payload.get("verification"), dict):
            return payload["verification"]
        return payload if isinstance(payload, dict) else {"value": payload}
    payload: dict[str, Any] = {}
    if plan is not None:
        payload["plan"] = to_bounded_plain(plan)
    if results is not None:
        payload["results"] = to_bounded_plain(results)
        payload["failed_checks"] = [
            item
            for item in payload.get("results", [])
            if isinstance(item, dict) and item.get("status") in {"failed", "blocked", "timeout", "flaky"}
        ]
    if assessment is not None:
        payload["completion_assessment"] = to_bounded_plain(assessment)
    return payload


def _verification_check_groups(verification: dict[str, Any]) -> dict[str, list[str]]:
    failed = verification.get("failed_checks") or []
    statuses = verification.get("check_status") or []
    groups = {
        "failed_required_checks": set(),
        "blocked_required_checks": set(),
        "flaky_required_checks": set(),
    }
    for item in [*failed, *statuses]:
        if not isinstance(item, dict) or not item.get("check_id"):
            continue
        check_id = str(item["check_id"])
        status = str(item.get("status") or "").lower()
        failure_type = str(item.get("failure_type") or "").lower()
        if status == "flaky" or failure_type == "flaky_failure":
            groups["flaky_required_checks"].add(check_id)
        elif status == "blocked" or failure_type in {
            "missing_command",
            "check_blocked",
            "check_review_required",
            "check_policy_denied",
            "inconclusive_result",
            "sandbox_limitation",
            "sandbox_violation",
        }:
            groups["blocked_required_checks"].add(check_id)
        elif status in {"failed", "timeout"}:
            groups["failed_required_checks"].add(check_id)
    return {key: sorted(value) for key, value in groups.items()}


def _stable_review_hash(payload: dict[str, Any]) -> str:
    import hashlib
    import json

    return hashlib.sha256(
        json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str).encode("utf-8")
    ).hexdigest()
