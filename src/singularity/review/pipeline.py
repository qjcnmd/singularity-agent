from __future__ import annotations

import time
from pathlib import Path
from typing import Any

from singularity.observability.models import TraceEventType, TraceSeverity
from singularity.review.critic import ModelCritic
from singularity.review.decision import ReviewDecisionEngine
from singularity.review.evidence import collect_review_evidence, to_bounded_plain
from singularity.review.findings import RuleFindingCollector
from singularity.review.models import (
    ReviewDecisionAction,
    ReviewEvidence,
    ReviewFinding,
    ReviewReport,
    ReviewStage,
    ReviewTarget,
)
from singularity.review.policy import extract_policy_context


class ReviewPipeline:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        trace: Any | None = None,
        project_index: Any | None = None,
        policy_engine: Any | None = None,
        model_runner: Any | None = None,
        memory_pipeline: Any | None = None,
        planner: Any | None = None,
        enable_model_critic: bool = True,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.trace = trace
        self.project_index = project_index
        self.policy_engine = policy_engine
        self.model_runner = model_runner
        self.memory_pipeline = memory_pipeline
        self.planner = planner
        self.enable_model_critic = enable_model_critic
        self.finding_collector = RuleFindingCollector()
        self.decision_engine = ReviewDecisionEngine()
        self.cancellation_token: Any | None = None
        self._critic_cache: dict[str, dict[str, Any]] = {}
        self._pre_edit_by_patch_digest: dict[str, dict[str, Any]] = {}
        self._latest_post_verification_critic: dict[str, Any] | None = None

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
        return self._run_stage_review(
            target=target,
            context=context,
            evidence_inputs={
                "intent": intent,
                "edit_plan": plan,
                "patch": patch,
                "validation": validation,
                "code_impact": code_impact,
                "test_impact": test_impact,
                "policy_observation": policy_observation,
                "trace_summary": trace_summary,
            },
        )

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
        return self._run_stage_review(
            target=target,
            context=context,
            evidence_inputs={
                "edit_result": edit_result,
                "mutation_result": mutation_result,
                "verification_plan": verification_plan,
                "code_impact": code_impact,
                "test_impact": test_impact,
                "policy_observation": policy_observation,
                "trace_summary": trace_summary,
            },
        )

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
        return self._run_stage_review(
            target=target,
            context=context,
            evidence_inputs={
                "verification": verification_payload,
                "policy_observation": policy_observation,
                "trace_summary": trace_summary,
            },
        )

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
        return self._run_stage_review(
            target=target,
            context=context,
            evidence_inputs={
                "task_state": task_state,
                "task_plan": task_plan,
                "evidence_ledger": ledger,
                "trace_summary": trace_summary,
            },
        )

    def health(self) -> dict[str, Any]:
        return {
            "status": "ok",
            "workspace_root": str(self.workspace_root),
            "model_critic_enabled": self.enable_model_critic,
            "has_model_runner": self.model_runner is not None,
        }

    def _run_stage_review(
        self,
        *,
        target: ReviewTarget,
        context: dict[str, Any],
        evidence_inputs: dict[str, Any],
    ) -> ReviewReport:
        evidence = collect_review_evidence(**evidence_inputs)
        return self._review(target=target, evidence=evidence, context=context)

    def _review(
        self,
        *,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> ReviewReport:
        self._throw_if_cancelled()
        started = time.perf_counter()
        critic_duration_ms = 0
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
        critic_reused = False
        critic_skipped_reason = ""
        critic_reuse_skip_reason = ""
        cached_outcome = self._cached_critic_outcome(
            target=target,
            evidence=evidence,
            context=context,
            rule_decision=rule_decision,
        )
        if cached_outcome is not None:
            critic_reused = True
            critic_skipped_reason = str(cached_outcome.get("reason") or "cached_critic_outcome")
            report.model_critic_status = str(cached_outcome.get("status") or "reused")
            report.model_critic_error = cached_outcome.get("error")
            report.metadata.update(_review_output_metadata(cached_outcome))
            report.metadata["critic_reused"] = True
            report.metadata["critic_skipped_reason"] = critic_skipped_reason
            report.metadata["critic_source_review_id"] = cached_outcome.get("review_id")
            report.metadata["critic_source_status"] = str(
                cached_outcome.get("source_status")
                or cached_outcome.get("original_status")
                or cached_outcome.get("status")
                or ""
            )
            cached_findings = cached_outcome.get("findings") or []
            if isinstance(cached_findings, list):
                report.findings.extend(
                    finding if isinstance(finding, ReviewFinding) else ReviewFinding.model_validate(finding)
                    for finding in cached_findings
                    if isinstance(finding, ReviewFinding | dict)
                )
                report.decision = self.decision_engine.decide(target=target, findings=report.findings, context=context)
                report.next_actions = list(report.decision.next_actions)
        elif self.enable_model_critic:
            critic_reuse_skip_reason = self._critic_reuse_skip_reason(
                target=target,
                context=context,
                rule_decision=rule_decision,
            )
            self._throw_if_cancelled()
            critic = ModelCritic(self.model_runner)
            critic_started = time.perf_counter()
            outcome = critic.review(
                report,
                bundle={"target": target.model_dump(mode="json"), "context": context},
                request_context=self._critic_request_context(target),
            )
            critic_duration_ms = int((time.perf_counter() - critic_started) * 1000)
            self._throw_if_cancelled()
            report.model_critic_status = outcome.status
            report.model_critic_error = outcome.error
            report.metadata.update(_review_output_metadata(outcome.metadata))
            if outcome.status != "ok":
                evidence_payload = {
                    "status": outcome.status,
                    "error": outcome.error,
                    **_review_output_metadata(outcome.metadata),
                }
                report.evidence.append(
                    ReviewEvidence(
                        source=outcome.status,
                        summary=f"Model critic status: {outcome.status}.",
                        payload=evidence_payload,
                        payload_hash=_stable_review_hash(evidence_payload),
                        trust_level="model_derived",
                    )
                )
            if outcome.findings:
                report.findings.extend(outcome.findings)
                report.decision = self.decision_engine.decide(target=target, findings=report.findings, context=context)
                report.next_actions = list(report.decision.next_actions)
            self._store_critic_outcome(report, evidence=evidence, context=context)
        else:
            report.model_critic_status = "disabled"

        if not critic_reused:
            report.metadata.setdefault("critic_reused", False)
            report.metadata.setdefault("critic_skipped_reason", "")
            report.metadata.setdefault("critic_reuse_skip_reason", critic_reuse_skip_reason)
            report.metadata.setdefault("critic_source_status", report.model_critic_status)
        if target.stage == ReviewStage.PRE_EDIT and report.decision.action == ReviewDecisionAction.ACCEPT:
            self._store_pre_edit_reference(report, evidence=evidence, context=context)
        report.trace_event_ids.extend(self._emit_findings(report))
        report.trace_event_ids.extend(self._emit_decision(report))
        report.trace_event_ids.extend(
            self._emit_completed(
                report,
                duration_ms=int((time.perf_counter() - started) * 1000),
                critic_duration_ms=critic_duration_ms,
                critic_reused=critic_reused,
                critic_skipped_reason=critic_skipped_reason,
                critic_reuse_skip_reason=critic_reuse_skip_reason,
                critic_source_status=str(report.metadata.get("critic_source_status") or ""),
                review_output_metadata=_review_output_metadata(report.metadata),
            )
        )
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
                        "edit_ok": plain.get("ok"),
                        "edit_status": plain.get("status"),
                    }
                )
            elif key == "mutation_result" and isinstance(plain, dict):
                context.setdefault("changed_files", plain.get("affected_files") or plain.get("changed_files") or [])
                context.setdefault("transaction_id", plain.get("transaction_id"))
                context["mutation_ok"] = plain.get("ok")
                context["mutation_status"] = plain.get("status")
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

    def _emit_completed(
        self,
        report: ReviewReport,
        *,
        duration_ms: int,
        critic_duration_ms: int,
        critic_reused: bool,
        critic_skipped_reason: str,
        critic_reuse_skip_reason: str,
        critic_source_status: str,
        review_output_metadata: dict[str, Any],
    ) -> list[str]:
        return self._emit(
            TraceEventType.REVIEW_COMPLETED,
            summary=f"Review completed for {report.target.stage.value}.",
            payload={
                "review_id": report.review_id,
                "decision": report.decision.action.value,
                "finding_count": len(report.findings),
                "blocking_count": len(report.blocking_findings),
                "model_critic_status": report.model_critic_status,
                "review_stage": report.target.stage.value,
                "duration_ms": duration_ms,
                "critic_duration_ms": critic_duration_ms,
                "critic_reused": critic_reused,
                "critic_skipped_reason": critic_skipped_reason,
                "critic_reuse_skip_reason": critic_reuse_skip_reason,
                "critic_source_status": critic_source_status,
                **review_output_metadata,
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
            "action_id": _review_action_id(target),
            "transaction_id": target.transaction_id,
            "verification_id": target.verification_id,
            "policy_decision_id": target.policy_decision_id,
        }
        if hasattr(self.trace, "emit"):
            event = self.trace.emit(
                event_type,
                component="review",
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
        if self.memory_pipeline is None or not hasattr(self.memory_pipeline, "ingest_review_report"):
            return
        try:
            self.memory_pipeline.ingest_review_report(report)
        except Exception as exc:
            report.evidence.append(
                ReviewEvidence(
                    source="memory_ingest",
                    summary="Memory ingest failed; review result was preserved.",
                    payload={"type": type(exc).__name__, "message": str(exc)},
                    payload_hash=_stable_review_hash(
                        {"type": type(exc).__name__, "message": str(exc)}
                    ),
                    trust_level="trusted_component",
                )
            )
            self._emit(
                TraceEventType.REVIEW_FINDING,
                summary="Review memory ingest failed open.",
                payload={"type": type(exc).__name__, "message": str(exc)},
                severity=TraceSeverity.WARNING,
                target=report.target,
            )

    def _cached_critic_outcome(
        self,
        *,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
        rule_decision: Any,
    ) -> dict[str, Any] | None:
        if not self.enable_model_critic:
            return None
        cache_key = self._critic_cache_key(target=target, evidence=evidence, context=context)
        cached = self._critic_cache.get(cache_key)
        if cached is not None and _critic_status_reusable(cached):
            return {**cached, "reason": "identical_review_evidence"}
        if target.stage == ReviewStage.FINAL:
            if getattr(rule_decision, "action", None) != ReviewDecisionAction.ACCEPT:
                return None
            post_verification = self._post_verification_critic_for_final(context)
            if post_verification is not None:
                return {**post_verification, "reason": "post_verification_evidence_unchanged"}
            return None
        if target.stage != ReviewStage.POST_PATCH or not target.patch_digest:
            return None
        if not self._post_patch_can_reuse_pre_edit_critic(context):
            return None
        pre_edit = self._pre_edit_by_patch_digest.get(target.patch_digest)
        if pre_edit is None or not _critic_status_reusable(pre_edit):
            return None
        if sorted(str(path) for path in pre_edit.get("changed_files") or []) != _changed_files_from_context(context):
            return None
        return {**pre_edit, "reason": "pre_edit_evidence_unchanged"}

    def _critic_reuse_skip_reason(
        self,
        *,
        target: ReviewTarget,
        context: dict[str, Any],
        rule_decision: Any,
    ) -> str:
        if target.stage == ReviewStage.FINAL:
            return self._final_reuse_skip_reason(context, rule_decision=rule_decision)
        if target.stage != ReviewStage.POST_PATCH:
            return "stage_not_reusable"
        if not target.patch_digest:
            return "missing_patch_digest"
        if not self._post_patch_can_reuse_pre_edit_critic(context):
            return "risk_or_result_requires_review"
        pre_edit = self._pre_edit_by_patch_digest.get(target.patch_digest)
        if pre_edit is None:
            return "pre_edit_reference_missing"
        if not _critic_status_reusable(pre_edit):
            return "pre_edit_status_not_reusable"
        if sorted(str(path) for path in pre_edit.get("changed_files") or []) != _changed_files_from_context(context):
            return "changed_files_changed"
        return "cache_key_miss"

    def _store_critic_outcome(
        self,
        report: ReviewReport,
        *,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> None:
        if report.model_critic_status != "ok":
            return
        cache_key = self._critic_cache_key(target=report.target, evidence=evidence, context=context)
        self._critic_cache[cache_key] = {
            "review_id": report.review_id,
            "status": report.model_critic_status,
            "source_status": report.model_critic_status,
            "error": report.model_critic_error,
            **_review_output_metadata(report.metadata),
            "findings": [
                finding.model_dump(mode="json")
                for finding in report.findings
                if finding.source == "model_critic"
            ],
        }
        if report.target.stage == ReviewStage.POST_VERIFICATION and self._post_verification_can_seed_final_critic(
            report,
            context=context,
        ):
            verification = context.get("verification") if isinstance(context.get("verification"), dict) else {}
            self._latest_post_verification_critic = {
                "review_id": report.review_id,
                "task_id": report.target.task_id,
                "status": "reused",
                "original_status": report.model_critic_status,
                "source_status": report.model_critic_status,
                "error": report.model_critic_error,
                "verification_digest": _stable_review_hash(_final_verification_reuse_context(verification)),
                **_review_output_metadata(report.metadata),
                "findings": [
                    finding.model_dump(mode="json")
                    for finding in report.findings
                    if finding.source == "model_critic"
                ],
            }
        elif report.target.stage == ReviewStage.POST_VERIFICATION:
            self._latest_post_verification_critic = None

    def _store_pre_edit_reference(
        self,
        report: ReviewReport,
        *,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> None:
        if not report.target.patch_digest:
            return
        if not self.enable_model_critic or report.model_critic_status != "ok":
            return
        if not self._pre_edit_can_seed_post_patch_critic(context):
            return
        self._pre_edit_by_patch_digest[report.target.patch_digest] = {
            "review_id": report.review_id,
            "status": "reused",
            "original_status": report.model_critic_status,
            "source_status": report.model_critic_status,
            "error": report.model_critic_error,
            "evidence_hashes": [item.payload_hash for item in evidence],
            "changed_files": list(context.get("changed_files") or _changed_files_from_context(context)),
            **_review_output_metadata(report.metadata),
            "findings": [
                finding.model_dump(mode="json")
                for finding in report.findings
                if finding.source == "model_critic"
            ],
        }

    def _critic_cache_key(
        self,
        *,
        target: ReviewTarget,
        evidence: list[ReviewEvidence],
        context: dict[str, Any],
    ) -> str:
        return _stable_review_hash(
            {
                "stage": target.stage.value,
                "target": target.model_dump(mode="json"),
                "evidence_hashes": [item.payload_hash for item in evidence],
                "review_context": _review_reuse_context(context),
            }
        )

    def _pre_edit_can_seed_post_patch_critic(self, context: dict[str, Any]) -> bool:
        validation = context.get("validation") if isinstance(context.get("validation"), dict) else {}
        if validation.get("ok") is False or validation.get("requires_review") is True:
            return False
        if validation.get("issues"):
            return False
        return _risk_level(context) in {"", "low", "none"}

    def _post_patch_can_reuse_pre_edit_critic(self, context: dict[str, Any]) -> bool:
        mutation_ok = context.get("mutation_ok")
        if mutation_ok is False:
            return False
        edit_ok = context.get("edit_ok")
        if edit_ok is False:
            return False
        mutation_status = str(context.get("mutation_status") or "").lower()
        if mutation_status and mutation_status not in {"applied", "ok", "success", "succeeded"}:
            return False
        edit_status = str(context.get("edit_status") or "").lower()
        if edit_status and edit_status not in {"applied", "ok", "success", "succeeded"}:
            return False
        policy_outcome = str(context.get("policy_outcome") or "").lower()
        if policy_outcome in {"require_review", "ask_user", "escalate", "deny"}:
            return False
        verification = context.get("verification") if isinstance(context.get("verification"), dict) else {}
        status = str(context.get("verification_status") or verification.get("status") or "").lower()
        if status in {"failed", "blocked", "needs_review"}:
            return False
        return _risk_level(context) in {"", "low", "none"}

    def _post_verification_can_seed_final_critic(
        self,
        report: ReviewReport,
        *,
        context: dict[str, Any],
    ) -> bool:
        if report.decision.action != ReviewDecisionAction.ACCEPT:
            return False
        if report.blocking_findings:
            return False
        if any(finding.source == "model_critic" and finding.blocking for finding in report.findings):
            return False
        verification = context.get("verification") if isinstance(context.get("verification"), dict) else {}
        return _final_verification_reusable(verification)

    def _post_verification_critic_for_final(self, context: dict[str, Any]) -> dict[str, Any] | None:
        cached = self._latest_post_verification_critic
        if cached is None or not _critic_status_reusable(cached):
            return None
        cached_task_id = str(cached.get("task_id") or "")
        current_task_id = str(_task_id_from_context(context) or "")
        if cached_task_id and current_task_id and cached_task_id != current_task_id:
            return None
        verification = context.get("verification") if isinstance(context.get("verification"), dict) else {}
        if not _final_verification_reusable(verification):
            return None
        digest = _stable_review_hash(_final_verification_reuse_context(verification))
        if cached.get("verification_digest") != digest:
            return None
        return cached

    def _final_reuse_skip_reason(self, context: dict[str, Any], *, rule_decision: Any) -> str:
        if getattr(rule_decision, "action", None) != ReviewDecisionAction.ACCEPT:
            return "final_rule_decision_not_accept"
        verification = context.get("verification") if isinstance(context.get("verification"), dict) else {}
        if not _final_verification_reusable(verification):
            return "post_verification_not_reusable"
        cached = self._latest_post_verification_critic
        if cached is None:
            return "post_verification_reference_missing"
        if not _critic_status_reusable(cached):
            return "post_verification_status_not_reusable"
        cached_task_id = str(cached.get("task_id") or "")
        current_task_id = str(_task_id_from_context(context) or "")
        if cached_task_id and current_task_id and cached_task_id != current_task_id:
            return "post_verification_task_changed"
        digest = _stable_review_hash(_final_verification_reuse_context(verification))
        if cached.get("verification_digest") != digest:
            return "post_verification_evidence_changed"
        return "cache_key_miss"

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
            "action_id": _review_action_id(target),
        }


def _review_action_id(target: ReviewTarget) -> str | None:
    return (
        target.edit_result_id
        or target.patch_id
        or target.verification_id
        or (target.plan_id if target.stage == ReviewStage.FINAL else None)
        or (target.task_id if target.stage == ReviewStage.FINAL else None)
    )


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


def _review_reuse_context(context: dict[str, Any]) -> dict[str, Any]:
    return {
        "changed_files": _changed_files_from_context(context),
        "risk_level": _risk_level(context),
        "policy_outcome": context.get("policy_outcome"),
        "verification_status": context.get("verification_status"),
        "edit_ok": context.get("edit_ok"),
        "edit_status": context.get("edit_status"),
        "mutation_ok": context.get("mutation_ok"),
        "mutation_status": context.get("mutation_status"),
    }


def _changed_files_from_context(context: dict[str, Any]) -> list[str]:
    changed = context.get("changed_files") or []
    if not changed and isinstance(context.get("validation"), dict):
        changed = (
            context["validation"].get("changed_files")
            or context["validation"].get("affected_files")
            or context["validation"].get("touched_paths")
            or []
        )
    if not changed and isinstance(context.get("edit_result"), dict):
        changed = context["edit_result"].get("changed_files") or []
    if not changed and isinstance(context.get("patch"), dict):
        changed = context["patch"].get("changed_files") or context["patch"].get("touched_paths") or []
    return sorted(str(path) for path in changed)


def _risk_level(context: dict[str, Any]) -> str:
    for key in ("code_impact", "test_impact"):
        payload = context.get(key)
        if isinstance(payload, dict) and payload.get("risk_level"):
            return str(payload["risk_level"]).lower()
    return ""


def _critic_status_reusable(outcome: dict[str, Any]) -> bool:
    status = str(outcome.get("source_status") or outcome.get("original_status") or outcome.get("status") or "")
    return status == "ok"


def _task_id_from_context(context: dict[str, Any]) -> str | None:
    task_state = context.get("task_state") if isinstance(context.get("task_state"), dict) else {}
    return task_state.get("task_id")


def _final_verification_reusable(verification: dict[str, Any]) -> bool:
    assessment = verification.get("completion_assessment") if isinstance(verification.get("completion_assessment"), dict) else {}
    status = str(assessment.get("status") or verification.get("status") or "").lower()
    if status not in {"ready", "ready_with_warnings"}:
        return False
    groups = _verification_check_groups(verification)
    if groups["failed_required_checks"] or groups["blocked_required_checks"] or groups["flaky_required_checks"]:
        return False
    return not verification.get("failed_checks")


def _final_verification_reuse_context(verification: dict[str, Any]) -> dict[str, Any]:
    assessment = verification.get("completion_assessment") if isinstance(verification.get("completion_assessment"), dict) else {}
    plan = verification.get("plan") if isinstance(verification.get("plan"), dict) else {}
    return {
        "verification_plan_id": plan.get("verification_plan_id") or verification.get("verification_plan_id"),
        "changeset_id": plan.get("changeset_id"),
        "transaction_id": plan.get("transaction_id"),
        "status": assessment.get("status") or verification.get("status"),
        "check_status": [
            {
                "check_id": item.get("check_id"),
                "status": item.get("status"),
                "failure_type": item.get("failure_type"),
            }
            for item in verification.get("check_status") or []
            if isinstance(item, dict)
        ],
    }


def _review_output_metadata(payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "output_mode": str(payload.get("output_mode") or ""),
        "schema_validation_passed": bool(payload.get("schema_validation_passed")),
        "retry_count": int(payload.get("retry_count") or 0),
        "retry_reason": str(payload.get("retry_reason") or "none"),
        "fallback_reason": str(payload.get("fallback_reason") or ""),
    }


def _stable_review_hash(payload: dict[str, Any]) -> str:
    import hashlib
    import json

    return hashlib.sha256(
        json.dumps(payload, ensure_ascii=False, sort_keys=True, default=str).encode("utf-8")
    ).hexdigest()
