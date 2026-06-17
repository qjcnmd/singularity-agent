from __future__ import annotations

from pathlib import Path
from typing import Any
from uuid import uuid4

from miniharness.planner.budget import BudgetController
from miniharness.planner.context import PlannerContextRenderer
from miniharness.planner.finalizer import Finalizer
from miniharness.planner.models import (
    ActionKind,
    ActionStatus,
    AgentAction,
    AuthorizationDecision,
    EvidenceLedger,
    ExecutionBudget,
    FinalReport,
    ReplanDecision,
    ReplanDecisionKind,
    RiskDecisionKind,
    RiskLevel,
    TaskPhase,
    TaskPlan,
    TaskState,
    TaskStatus,
)
from miniharness.planner.policy import PlannerPolicy, READ_TOOLS, MUTATION_TOOLS, VERIFICATION_TOOLS
from miniharness.planner.replanner import Replanner
from miniharness.planner.risk import RiskEscalator
from miniharness.planner.store import PlannerStore
from miniharness.tools.models import ToolResult, ToolSpec
from miniharness.trace import TraceWriter


class PlannerRuntime:
    def __init__(
        self,
        workspace_root: Path | str,
        *,
        session_id: str | None = None,
        task_id: str | None = None,
        trace: TraceWriter | None = None,
        store: PlannerStore | None = None,
    ) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.session_id = session_id or uuid4().hex
        self.task_id = task_id or self.session_id
        self.trace = trace
        self.store = store or PlannerStore(self.workspace_root)
        self.policy = PlannerPolicy()
        self.risk = RiskEscalator()
        self.replanner = Replanner()
        self.renderer = PlannerContextRenderer()
        self.finalizer = Finalizer()
        self.state: TaskState | None = None
        self.plan: TaskPlan | None = None
        self.evidence = EvidenceLedger()
        self.budget = ExecutionBudget()
        self.final_report: FinalReport | None = None
        self.actions: dict[str, AgentAction] = {}

    def start_task(
        self,
        user_goal: str,
        *,
        constraints: list[str] | None = None,
        assumptions: list[str] | None = None,
    ) -> TaskState:
        self.state = TaskState(
            task_id=self.task_id,
            session_id=self.session_id,
            user_goal=user_goal,
            normalized_goal=" ".join(user_goal.split()),
            constraints=constraints or [],
            assumptions=assumptions or [],
            status=TaskStatus.UNDERSTANDING_TASK,
            current_phase="understanding_task",
        )
        if self._is_read_only_goal(user_goal):
            self.state.completion_criteria.required_files_inspected = (
                self._requires_workspace_evidence(user_goal)
            )
            self.state.completion_criteria.required_changes_applied = False
            self.state.completion_criteria.required_verifications_passed = False
        self.plan = self._default_plan(self.state.task_id)
        self.evidence = EvidenceLedger(assumptions=list(self.state.assumptions))
        self.budget = ExecutionBudget()
        self._persist()
        self._record_event(decision="start_task", reason="Task initialized.")
        return self.state

    def step(self) -> AgentAction:
        state = self._state()
        plan = self._plan()
        self._auto_advance_before_step()
        BudgetController(self.budget).record_model_turn()
        phase = plan.phase(state.current_phase)
        action = AgentAction(
            kind=phase.allowed_actions[0],
            intent=phase.purpose,
            phase_id=phase.phase_id,
            preconditions=phase.entry_conditions,
            allowed_tools=phase.allowed_tools,
            expected_evidence=phase.required_evidence,
            risk_level=state.risk_level,
            status=ActionStatus.ALLOWED,
        )
        self.actions[action.action_id] = action
        self._persist()
        self._record_event(
            action=action,
            decision="step",
            reason="Planner selected next allowed action.",
        )
        return action

    def authorize_tool_call(
        self,
        *,
        tool_name: str,
        tool_call_id: str | None,
        spec: ToolSpec,
        arguments: dict[str, Any] | None = None,
    ) -> AuthorizationDecision:
        state = self._state()
        plan = self._plan()
        phase = plan.phase(state.current_phase)
        normalized_args = self.policy.normalize_arguments(arguments or {})
        if not self.policy.is_allowed(phase=phase, tool_name=tool_name, spec=spec):
            decision = AuthorizationDecision(
                allowed=False,
                error_code="action_not_allowed",
                reason=f"{tool_name} is not allowed in phase {phase.phase_id}.",
            )
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision="deny",
                reason=decision.reason,
            )
            return decision

        risk = self.risk.evaluate_action(
            tool_name=tool_name,
            arguments=normalized_args,
            changed_files=self._changed_files(),
        )
        if risk.decision != RiskDecisionKind.CONTINUE:
            state.status = TaskStatus.NEEDS_REVIEW
            state.risk_level = risk.risk_level
            state.blocked_reasons.extend(risk.reasons)
            self.evidence.risks.append(risk.to_dict())
            self._persist()
            self._record_event(
                action_id=tool_call_id,
                action_kind=self.policy.action_for_tool(tool_name).value,
                decision=risk.decision.value,
                reason="; ".join(risk.reasons),
            )
            return AuthorizationDecision(
                allowed=False,
                error_code="risk_escalated",
                reason="; ".join(risk.reasons),
                risk_decision=risk.decision,
            )

        action = AgentAction(
            kind=self.policy.action_for_tool(tool_name),
            intent=f"Use {tool_name}",
            phase_id=phase.phase_id,
            preconditions=phase.entry_conditions,
            allowed_tools=[tool_name],
            expected_evidence=self.policy.expected_evidence(tool_name),
            risk_level=risk.risk_level,
            status=ActionStatus.ALLOWED,
        )
        self.actions[action.action_id] = action
        self._record_event(
            action=action,
            decision="allow",
            reason=f"{tool_name} is allowed in phase {phase.phase_id}.",
        )
        return AuthorizationDecision(allowed=True, action=action)

    def update_from_tool_result(
        self,
        *,
        tool_call_id: str | None,
        tool_name: str,
        result: ToolResult | dict[str, Any],
        action_id: str | None = None,
    ) -> None:
        state = self._state()
        BudgetController(self.budget).record_tool_call()
        payload = result.model_dump(mode="json") if isinstance(result, ToolResult) else result
        content = payload.get("content")
        self.evidence.tool_results.append(
            {
                "tool_call_id": tool_call_id,
                "tool_name": tool_name,
                "action_id": action_id,
                "ok": payload.get("ok"),
                "error_code": payload.get("error_code"),
            }
        )
        if payload.get("ok") is False and payload.get("error_code"):
            self.replan({"error_code": payload.get("error_code")})
        if tool_name == "read_file" and isinstance(content, dict):
            self.evidence.add_unique_file(str(content.get("path") or ""))
        elif tool_name == "list_files" and isinstance(content, dict):
            root = content.get("root")
            if root:
                self.evidence.add_unique_file(str(root))
        elif tool_name == "search_text" and isinstance(content, dict):
            self.evidence.search_results.extend(content.get("matches") or [])
        elif tool_name.startswith("workspace_") and isinstance(content, dict):
            self.update_from_mutation(content, tool_call_id=tool_call_id)
        elif tool_name in {"run_command", "start_process", "stop_process"} and isinstance(content, dict):
            self.update_from_command(content, tool_call_id=tool_call_id)
        elif "verification" in tool_name and isinstance(content, dict):
            self.update_from_verification(content, tool_call_id=tool_call_id)

        if action_id and action_id in self.actions:
            self.actions[action_id].status = ActionStatus.SUCCEEDED if payload.get("ok") else ActionStatus.FAILED
            self.actions[action_id].result_ref = tool_call_id
        self._maybe_advance_after_tool(tool_name)
        state.touch()
        self._persist()
        self._record_event(
            action_id=action_id,
            action_kind=self.actions[action_id].kind.value if action_id in self.actions else None,
            decision="tool_result",
            reason=f"Recorded result for {tool_name}.",
            evidence_refs=[tool_call_id] if tool_call_id else [],
        )

    def update_from_mutation(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        payload = self._content_payload(result)
        if payload.get("mutation_status") not in {None, "preview"}:
            changed_files = list(payload.get("changed_files") or [])
            BudgetController(self.budget).record_mutation(changed_files=len(changed_files))
            transaction_id = payload.get("transaction_id")
            if transaction_id and any(
                change.get("transaction_id") == transaction_id
                for change in self.evidence.applied_changes
            ):
                return
            entry = {
                "tool_call_id": tool_call_id,
                "transaction_id": transaction_id,
                "changeset_id": payload.get("changeset_id"),
                "changed_files": changed_files,
                "status": payload.get("mutation_status"),
                "artifact_path": payload.get("artifact_path"),
            }
            self.evidence.applied_changes.append(entry)
            self._append_unique(self._state().linked_transactions, transaction_id)
            self._state().status = TaskStatus.RUNNING_VERIFICATION
            self._state().current_phase = "running_verification"
            self._plan().current_phase = "running_verification"
        if payload.get("error_code"):
            self.replan({"error_code": payload.get("error_code")})
        self._persist()

    def update_from_command(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        payload = self._content_payload(result)
        command = payload.get("command_result") if isinstance(payload.get("command_result"), dict) else payload
        if isinstance(command, dict):
            command_id = command.get("command_id")
            if command_id and any(
                existing.get("command_id") == command_id
                for existing in self.evidence.command_results
            ):
                return
            BudgetController(self.budget).record_command()
            command = {**command, "tool_call_id": tool_call_id}
            self.evidence.command_results.append(command)
            self._append_unique(self._state().linked_commands, command_id)
            for failure in command.get("parsed_failures") or []:
                self.evidence.parsed_failures.append(failure)
            if command.get("semantic_status") not in {None, "succeeded", "SUCCEEDED"}:
                self.evidence.unresolved_failures.append(command)
        self._persist()

    def update_from_verification(
        self, result: Any, *, tool_call_id: str | None = None
    ) -> None:
        payload = self._content_payload(result)
        verification = payload.get("verification") if isinstance(payload.get("verification"), dict) else payload
        if not isinstance(verification, dict):
            return
        plan_payload = verification.get("plan") if isinstance(verification.get("plan"), dict) else {}
        plan_id = plan_payload.get("verification_plan_id")
        if plan_id and any(
            (existing.get("plan") or {}).get("verification_plan_id") == plan_id
            for existing in self.evidence.verification_results
        ):
            return
        verification = {**verification, "tool_call_id": tool_call_id}
        self.evidence.verification_results.append(verification)
        for status in verification.get("check_status") or []:
            self._append_unique(self._state().linked_verifications, status.get("check_id"))
            if status.get("status") in {"failed", "blocked", "timeout", "flaky"}:
                self.evidence.unresolved_failures.append(status)
        assessment = verification.get("completion_assessment") or {}
        self._state().final_assessment = assessment
        if assessment.get("status") in {"ready", "ready_with_warnings"}:
            self.evidence.unresolved_failures.clear()
            self._state().status = TaskStatus.FINALIZING
            self._state().current_phase = "finalizing"
            self._plan().current_phase = "finalizing"
        elif assessment.get("status") in {"blocked", "failed"}:
            self._state().status = TaskStatus.REPAIRING_FAILURES
            self._state().current_phase = "repairing_failures"
            self._plan().current_phase = "repairing_failures"
        self._persist()

    def record_policy_observation(self, observation: dict[str, Any]) -> None:
        state = self._state()
        payload = {
            "outcome": observation.get("outcome"),
            "runtime": observation.get("runtime"),
            "operation": observation.get("operation"),
            "reason": observation.get("reason"),
            "risk_level": observation.get("risk_level"),
            "resource": observation.get("resource"),
            "decision_id": observation.get("decision_id"),
        }
        if payload not in self.evidence.policy_observations:
            self.evidence.policy_observations.append(payload)
        if payload["outcome"] in {"deny", "require_review", "sandbox_required", "escalate"}:
            self.evidence.unresolved_failures.append({"policy": payload})
            state.status = TaskStatus.NEEDS_REVIEW
            if payload["reason"]:
                self._append_unique(state.blocked_reasons, payload["reason"])
        self._persist()
        self._record_event(
            decision="policy_observation",
            reason=str(payload.get("reason") or "Policy observation recorded."),
            evidence_refs=[str(payload.get("decision_id"))] if payload.get("decision_id") else [],
            extra={"policy_observation": payload},
        )

    def replan(self, signal: dict[str, Any]) -> ReplanDecision:
        state = self._state()
        fingerprint = signal.get("failure_fingerprint")
        if fingerprint:
            count = BudgetController(self.budget).record_failure(str(fingerprint))
            if count >= self.budget.max_repeated_failures:
                state.status = TaskStatus.BLOCKED
                state.blocked_reasons.append("repeated_failure")
                decision = ReplanDecision(
                    decision=ReplanDecisionKind.ASK_USER,
                    reason="Repeated failure budget exceeded.",
                    next_action=ActionKind.ASK_USER,
                )
                self._persist()
                self._record_event(
                    decision="replan",
                    reason=decision.reason,
                    replan_decision=decision.to_dict(),
                )
                return decision
        decision = self.replanner.decide(signal)
        if decision.decision == ReplanDecisionKind.READ_FRESH_FILE:
            state.status = TaskStatus.INSPECTING_WORKSPACE
            state.current_phase = "inspecting_workspace"
            self._plan().current_phase = "inspecting_workspace"
        elif decision.decision == ReplanDecisionKind.REPAIR_FAILURE:
            state.status = TaskStatus.REPAIRING_FAILURES
            state.current_phase = "repairing_failures"
            self._plan().current_phase = "repairing_failures"
        elif decision.decision == ReplanDecisionKind.REQUIRE_REVIEW:
            state.status = TaskStatus.NEEDS_REVIEW
        self._persist()
        self._record_event(
            decision="replan",
            reason=decision.reason,
            replan_decision=decision.to_dict(),
        )
        return decision

    def assess_completion(self) -> dict[str, Any]:
        state = self._state()
        unmet: list[str] = []
        if state.completion_criteria.required_files_inspected and not self.evidence.inspected_files:
            unmet.append("required_files_inspected")
        if state.completion_criteria.required_changes_applied and not self.evidence.applied_changes:
            unmet.append("required_changes_applied")
        verification_status = state.final_assessment.get("status")
        if (
            state.completion_criteria.required_verifications_passed
            and verification_status not in {"ready", "ready_with_warnings"}
        ):
            unmet.append("required_verifications_passed")
        if state.completion_criteria.unresolved_failures_empty and self.evidence.unresolved_failures:
            unmet.append("unresolved_failures_empty")
        if self.evidence.external_changes:
            unmet.append("workspace_health_acceptable")
        if self.evidence.risks and state.risk_level != RiskLevel.LOW:
            unmet.append("risks_acknowledged")
        status = TaskStatus.COMPLETED if not unmet else TaskStatus.BLOCKED
        assessment = {"status": status.value, "unmet": unmet}
        if unmet:
            state.status = TaskStatus.BLOCKED
            state.blocked_reasons = sorted(set([*state.blocked_reasons, *unmet]))
        self._persist()
        self._record_event(
            decision="assess_completion",
            reason="Completion criteria assessed.",
            completion_assessment=assessment,
        )
        return assessment

    def finalize(self) -> FinalReport:
        report = self.finalizer.build(state=self._state(), evidence=self.evidence)
        self.final_report = report
        if report.status == TaskStatus.COMPLETED:
            self._state().status = TaskStatus.COMPLETED
            self._state().completion_criteria.final_report_ready = True
        else:
            self._state().status = report.status
        self._state().touch()
        self._persist()
        self._record_event(
            decision="finalize",
            reason="Final report generated from ledger evidence.",
            completion_assessment=report.verification_summary,
        )
        return report

    def interrupt(self, reason: str = "interrupted") -> TaskState:
        state = self._state()
        state.status = TaskStatus.INTERRUPTED
        state.blocked_reasons.append(reason)
        state.touch()
        self._persist()
        self._record_event(decision="interrupt", reason=reason)
        return state

    def resume(
        self,
        session_id: str,
        *,
        workspace_health: dict[str, Any] | None = None,
    ) -> "PlannerRuntime":
        state, plan, evidence, budget, final_report = self.store.load(session_id)
        self.session_id = session_id
        self.task_id = state.task_id
        self.state = state
        self.plan = plan
        self.evidence = evidence
        self.budget = budget
        self.final_report = final_report
        if workspace_health and (
            workspace_health.get("status") == "conflicted"
            or workspace_health.get("external_changes")
        ):
            self.state.status = TaskStatus.NEEDS_REVIEW
            self.state.current_phase = "inspecting_workspace"
            self.plan.current_phase = "inspecting_workspace"
            self.state.blocked_reasons.append("workspace conflict on resume")
            self.evidence.external_changes.extend(workspace_health.get("external_changes") or [])
        elif self.state.status == TaskStatus.INTERRUPTED:
            self.state.status = TaskStatus.RECOVERING
        self._persist()
        self._record_event(decision="resume", reason="Planner state resumed.")
        return self

    def abort(self, reason: str = "aborted") -> TaskState:
        state = self._state()
        state.status = TaskStatus.FAILED
        state.blocked_reasons.append(reason)
        state.touch()
        self._persist()
        self._record_event(decision="abort", reason=reason)
        return state

    def planner_context_message(self) -> dict[str, str]:
        return {
            "role": "system",
            "content": self.renderer.render(
                state=self._state(),
                plan=self._plan(),
                evidence=self.evidence,
            ),
        }

    def filtered_tools(self, tools: list[dict[str, Any]]) -> list[dict[str, Any]]:
        allowed = set(self._plan().phase(self._state().current_phase).allowed_tools)
        return [
            tool
            for tool in tools
            if tool.get("function", {}).get("name") in allowed
        ]

    def _maybe_advance_after_tool(self, tool_name: str) -> None:
        state = self._state()
        plan = self._plan()
        if state.status == TaskStatus.UNDERSTANDING_TASK:
            state.status = TaskStatus.INSPECTING_WORKSPACE
            state.current_phase = "inspecting_workspace"
            plan.current_phase = "inspecting_workspace"
        if state.current_phase == "inspecting_workspace" and self.evidence.inspected_files:
            state.status = TaskStatus.PLANNING_CHANGES
            state.current_phase = "planning_changes"
            plan.current_phase = "planning_changes"
        elif state.current_phase == "planning_changes" and tool_name in READ_TOOLS:
            state.status = TaskStatus.APPLYING_CHANGES
            state.current_phase = "applying_changes"
            plan.current_phase = "applying_changes"

    def _auto_advance_before_step(self) -> None:
        state = self._state()
        plan = self._plan()
        if state.current_phase == "planning_changes" and self.evidence.inspected_files:
            state.status = TaskStatus.APPLYING_CHANGES
            state.current_phase = "applying_changes"
            plan.current_phase = "applying_changes"
        if state.current_phase == "applying_changes" and self.evidence.applied_changes:
            state.status = TaskStatus.RUNNING_VERIFICATION
            state.current_phase = "running_verification"
            plan.current_phase = "running_verification"
        if (
            state.current_phase == "running_verification"
            and state.final_assessment.get("status") in {"ready", "ready_with_warnings"}
        ):
            state.status = TaskStatus.FINALIZING
            state.current_phase = "finalizing"
            plan.current_phase = "finalizing"

    def _default_plan(self, task_id: str) -> TaskPlan:
        phases = [
            TaskPhase(
                phase_id="understanding_task",
                name="Understand Task",
                purpose="Normalize the user goal and identify constraints.",
                allowed_tools=sorted(READ_TOOLS),
                allowed_actions=[
                    ActionKind.INSPECT_WORKSPACE,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                    ActionKind.ANALYZE_ISSUE,
                ],
                required_evidence=["goal"],
            ),
            TaskPhase(
                phase_id="inspecting_workspace",
                name="Inspect Workspace",
                purpose="Read only the files needed to understand the task.",
                allowed_tools=sorted(READ_TOOLS),
                allowed_actions=[
                    ActionKind.INSPECT_WORKSPACE,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                    ActionKind.ANALYZE_ISSUE,
                ],
                required_evidence=["inspected_files"],
            ),
            TaskPhase(
                phase_id="planning_changes",
                name="Plan Changes",
                purpose="Propose a small changeset from inspected evidence.",
                allowed_tools=sorted(READ_TOOLS),
                allowed_actions=[
                    ActionKind.ANALYZE_ISSUE,
                    ActionKind.PROPOSE_CHANGE_SET,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                ],
                required_evidence=["inspected_files"],
            ),
            TaskPhase(
                phase_id="applying_changes",
                name="Apply Changes",
                purpose="Apply mutations only through Workspace Mutation Runtime.",
                allowed_tools=sorted(MUTATION_TOOLS | READ_TOOLS),
                allowed_actions=[
                    ActionKind.APPLY_MUTATION,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.SEARCH_CODE,
                ],
                required_evidence=["applied_changes"],
            ),
            TaskPhase(
                phase_id="running_verification",
                name="Run Verification",
                purpose="Run planned verification through VerificationRuntime.",
                allowed_tools=sorted(VERIFICATION_TOOLS | {"workspace_health"}),
                allowed_actions=[ActionKind.RUN_VERIFICATION, ActionKind.ANALYZE_ISSUE],
                required_evidence=["verification_results"],
            ),
            TaskPhase(
                phase_id="repairing_failures",
                name="Repair Failures",
                purpose="Repair failures using parsed evidence and bounded retries.",
                allowed_tools=sorted(MUTATION_TOOLS | READ_TOOLS | VERIFICATION_TOOLS),
                allowed_actions=[
                    ActionKind.PARSE_FAILURE,
                    ActionKind.REPAIR_CHANGE,
                    ActionKind.READ_RELEVANT_FILES,
                    ActionKind.RUN_VERIFICATION,
                ],
                required_evidence=["parsed_failures"],
            ),
            TaskPhase(
                phase_id="finalizing",
                name="Finalize",
                purpose="Generate a final report from runtime evidence.",
                allowed_tools=["get_verification_result", "workspace_health"],
                allowed_actions=[ActionKind.FINALIZE, ActionKind.ANALYZE_ISSUE],
                required_evidence=["final_report"],
            ),
        ]
        return TaskPlan(
            plan_id=f"plan_{uuid4().hex[:12]}",
            task_id=task_id,
            phases=phases,
            current_phase="understanding_task",
        )

    def _changed_files(self) -> list[str]:
        changed: set[str] = set()
        for change in self.evidence.applied_changes:
            for path in change.get("changed_files") or []:
                changed.add(str(path))
        return sorted(changed)

    def _persist(self) -> None:
        if self.state is None or self.plan is None:
            return
        self.state.touch()
        self.store.save(
            state=self.state,
            plan=self.plan,
            evidence=self.evidence,
            budget=self.budget,
            final_report=self.final_report,
        )

    def _record_event(
        self,
        *,
        action: AgentAction | None = None,
        action_id: str | None = None,
        action_kind: str | None = None,
        decision: str | None = None,
        reason: str | None = None,
        evidence_refs: list[str] | None = None,
        replan_decision: dict[str, Any] | None = None,
        completion_assessment: dict[str, Any] | None = None,
        extra: dict[str, Any] | None = None,
    ) -> None:
        if self.state is None:
            return
        self.store.append_event(
            self.state.session_id,
            task_id=self.state.task_id,
            phase=self.state.current_phase,
            action_id=action.action_id if action else action_id,
            action_kind=action.kind.value if action else action_kind,
            decision=decision,
            reason=reason,
            evidence_refs=evidence_refs,
            budget_state=self.budget.to_dict(),
            risk_level=self.state.risk_level.value,
            replan_decision=replan_decision,
            completion_assessment=completion_assessment,
            extra=extra,
        )
        if self.trace is not None:
            payload = {
                    "task_id": self.state.task_id,
                    "session_id": self.state.session_id,
                    "phase": self.state.current_phase,
                    "action_id": action.action_id if action else action_id,
                    "action_kind": action.kind.value if action else action_kind,
                    "decision": decision,
                    "reason": reason,
                    "evidence_refs": evidence_refs or [],
                    "budget_state": self.budget.to_dict(),
                    "risk_level": self.state.risk_level.value,
                    "replan_decision": replan_decision,
                    "completion_assessment": completion_assessment,
                }
            payload.update(extra or {})
            self.trace.record("planner", payload)

    def _state(self) -> TaskState:
        if self.state is None:
            raise RuntimeError("Planner task has not started.")
        return self.state

    def _plan(self) -> TaskPlan:
        if self.plan is None:
            raise RuntimeError("Planner task has not started.")
        return self.plan

    @staticmethod
    def _content_payload(result: Any) -> dict[str, Any]:
        if isinstance(result, dict) and "content" in result and isinstance(result["content"], dict):
            return result["content"]
        return result if isinstance(result, dict) else {}

    @staticmethod
    def _append_unique(values: list[str], value: Any) -> None:
        if value is None:
            return
        text = str(value)
        if text not in values:
            values.append(text)

    @staticmethod
    def _is_read_only_goal(user_goal: str) -> bool:
        lowered = user_goal.lower()
        read_markers = {
            "read",
            "inspect",
            "summarize",
            "explain",
            "find",
            "list",
            "say",
            "阅读",
            "总结",
            "说明",
            "解释",
            "查找",
            "看看",
        }
        write_markers = {
            "change",
            "modify",
            "implement",
            "fix",
            "add",
            "delete",
            "refactor",
            "upgrade",
            "修改",
            "实现",
            "修复",
            "新增",
            "删除",
            "升级",
        }
        return any(marker in lowered for marker in read_markers) and not any(
            marker in lowered for marker in write_markers
        )

    @staticmethod
    def _requires_workspace_evidence(user_goal: str) -> bool:
        lowered = user_goal.lower()
        markers = {
            "read",
            "inspect",
            "summarize",
            "explain",
            "find",
            "list",
            "file",
            "readme",
            "阅读",
            "总结",
            "说明",
            "解释",
            "查找",
            "看看",
            "文件",
        }
        return any(marker in lowered for marker in markers)
