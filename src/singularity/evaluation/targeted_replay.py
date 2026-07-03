from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.agent_loop import AgentLoop, AgentLoopStatus
from singularity.command import CommandExecutor
from singularity.edit import EditExecutor
from singularity.instructions import PromptAssemblyPipeline
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.model import ModelRunner
from singularity.planner import Planner
from singularity.policy import PolicyConfig, PolicyEngine
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.verification import VerificationRunner
from singularity.verification.contract import VerificationContract
from singularity.workspace import WorkspaceMutationManager

TARGETED_REPLAY_SCHEMA_VERSION = "evaluation.targeted_failure_replay/v1"


@dataclass(frozen=True)
class TargetedFailureReplayResult:
    status: str
    agent_completed: bool
    entered_agent_loop: bool
    failure_trigger: str
    failure_analysis_request_count: int
    failure_analysis_result_count: int
    repair_plan_count: int
    repair_contract_count: int
    repair_attempt_count: int
    repair_execution_count: int
    repairing_failures_seen: bool
    verification_contract_satisfaction: dict[str, Any]
    repair_scope: dict[str, Any]
    final_report_status: str
    trace_path: str
    phase_history: list[str] = field(default_factory=list)
    planner_status_history: list[dict[str, str]] = field(default_factory=list)
    repair_contract_summary: dict[str, Any] = field(default_factory=dict)
    repairing_failures_evidence: dict[str, Any] = field(default_factory=dict)
    trace_refs: dict[str, Any] = field(default_factory=dict)
    report_paths: dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        payload = {
            "schema_version": TARGETED_REPLAY_SCHEMA_VERSION,
            "status": self.status,
            "agent_completed": self.agent_completed,
            "entered_agent_loop": self.entered_agent_loop,
            "failure_trigger": self.failure_trigger,
            "failure_analysis_request_count": self.failure_analysis_request_count,
            "failure_analysis_result_count": self.failure_analysis_result_count,
            "repair_plan_count": self.repair_plan_count,
            "repair_contract_count": self.repair_contract_count,
            "repair_attempt_count": self.repair_attempt_count,
            "repair_execution_count": self.repair_execution_count,
            "repairing_failures_seen": self.repairing_failures_seen,
            "verification_contract_satisfaction": self.verification_contract_satisfaction,
            "repair_scope": self.repair_scope,
            "final_report_status": self.final_report_status,
            "trace_path": self.trace_path,
            "phase_history": list(self.phase_history),
            "planner_status_history": list(self.planner_status_history),
            "repair_contract_summary": dict(self.repair_contract_summary),
            "repairing_failures_evidence": dict(self.repairing_failures_evidence),
            "trace_refs": dict(self.trace_refs),
        }
        if self.report_paths:
            payload["report_paths"] = dict(self.report_paths)
        return payload


class TargetedFailureReplayRunner:
    """Run a deterministic failure-repair smoke through the real AgentLoop."""

    def __init__(self, *, workspace_root: Path | str, max_turns: int = 6) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.max_turns = max_turns

    def run(self, *, output_dir: Path | str) -> TargetedFailureReplayResult:
        result = self.run_smoke()
        output_path = Path(output_dir).resolve(strict=False)
        output_path.mkdir(parents=True, exist_ok=True)
        json_path = output_path / "targeted_replay_result.json"
        markdown_path = output_path / "targeted_replay_result.md"
        result = TargetedFailureReplayResult(
            **{
                **result.__dict__,
                "report_paths": {"json": str(json_path), "markdown": str(markdown_path)},
            }
        )
        json_path.write_text(
            json.dumps(result.to_dict(), ensure_ascii=False, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        markdown_path.write_text(_targeted_replay_markdown(result), encoding="utf-8")
        return result

    def run_smoke(self) -> TargetedFailureReplayResult:
        workspace = self.workspace_root
        workspace.mkdir(parents=True, exist_ok=True)
        fixture_target = workspace / "quicksort.py"
        if fixture_target.exists():
            fixture_target.unlink()
        (workspace / "README.md").write_text("targeted repair replay fixture\n", encoding="utf-8")
        planner = Planner(workspace, session_id="targeted_replay", task_id="targeted_replay")
        trace = JsonlTraceRecorder.create(workspace)
        planner.trace = trace
        policy = PolicyEngine(
            PolicyConfig(workspace_root=workspace)
        )
        provider = _ScriptedProvider(
            _tool("call_read", "read_file", {"path": "README.md"}),
            _tool(
                "call_create_bad",
                "write_file",
                {
                    "path": "quicksort.py",
                    "content": "print(1 / 0)\n",
                    "mode": "create",
                    "reason": "seed deterministic verification failure",
                },
            ),
            _tool(
                "call_verify_bad",
                "run_verification",
                {
                    "changed_files": ["quicksort.py"],
                    "task_intent": "verify quicksort script",
                    "smoke_commands": [["python", "quicksort.py"]],
                },
            ),
            _analysis_response(
                root_cause="quicksort.py raises ZeroDivisionError during smoke verification.",
                failure_category="unit_test_failure",
                affected_files=["quicksort.py"],
                evidence_refs=["call_verify_bad"],
                repair_strategy="replace the failing smoke target with a real quicksort implementation",
                next_actions=[
                    "Patch quicksort.py to remove the division by zero.",
                    "Rerun python quicksort.py through VerificationRunner.",
                ],
                verification_plan=["python quicksort.py"],
                confidence=0.91,
            ),
            _tool(
                "call_repair",
                "write_file",
                {
                    "path": "quicksort.py",
                    "content": QUICK_SORT,
                    "mode": "overwrite",
                    "reason": "repair failing smoke target",
                },
            ),
            _tool(
                "call_verify_fixed",
                "run_verification",
                {
                    "changed_files": ["quicksort.py"],
                    "task_intent": "verify quicksort script",
                    "smoke_commands": [["python", "quicksort.py"]],
                },
            ),
        )
        tools = ToolRegistry(workspace)
        mutation_manager = WorkspaceMutationManager(
            workspace,
            trace=trace,
            planner=planner,
            policy_engine=policy,
        )
        register_mutation_tools(tools, mutation_manager)
        register_edit_tools(
            tools,
            EditExecutor(workspace, mutation_manager=mutation_manager, trace=trace, planner=planner),
        )
        command_executor = CommandExecutor(workspace, trace=trace, planner=planner, policy_engine=policy)
        verification_runner = VerificationRunner(
            workspace,
            command_executor=command_executor,
            trace=trace,
            planner=planner,
            policy_engine=policy,
        )
        register_verification_tools(tools, verification_runner)
        model_runner = ModelRunner.from_chat_provider(provider, tool_registry=tools, trace=trace)
        agent = AgentLoop(
            provider=provider,
            model_runner=model_runner,
            tools=tools,
            trace=trace,
            console=Console(file=None, force_terminal=False),
            max_turns=self.max_turns,
            planner=planner,
            tool_executor=ToolExecutor(
                registry=tools,
                policy=ToolPolicy.coding_agent(),
                trace=trace,
                workspace_root=workspace,
                planner=planner,
                policy_engine=policy,
            ),
            tool_protocol=ToolProtocolEngine(
                registry=tools,
                trace=trace,
                state_store=ToolProtocolStateStore(_tool_protocol_state_path(workspace, trace)),
            ),
            prompt_assembly=PromptAssemblyPipeline(workspace_root=workspace, trace=trace),
        )
        agent_result = agent.run("repair quicksort.py after a failing verification and verify it")
        planner_status_history = _planner_status_history(trace.path, planner)
        phase_history = _phase_history(planner_status_history)
        repairing_evidence = _repairing_failures_evidence(
            trace.path,
            planner_status_history=planner_status_history,
            final_phase=getattr(planner.state, "current_phase", ""),
            final_status=getattr(getattr(planner.state, "status", None), "value", ""),
        )
        final_report_status = (
            planner.final_report.status.value
            if planner.final_report is not None
            else getattr(getattr(planner.state, "status", None), "value", "")
        )
        failure_analysis_request_ids = _trace_event_request_ids(trace.path, "failure_analysis_completed")
        failure_analysis_ids = _failure_analysis_ids_for_requests(
            planner.evidence.failure_analyses,
            failure_analysis_request_ids,
        )
        failure_analyzer_repair_plans = _repair_plans_for_analysis_ids(
            planner.evidence.repair_plans,
            failure_analysis_ids,
        )
        return TargetedFailureReplayResult(
            status=agent_result.status.value,
            agent_completed=agent_result.status == AgentLoopStatus.COMPLETED,
            entered_agent_loop=bool(provider.calls),
            failure_trigger=_failure_trigger(planner),
            failure_analysis_request_count=_trace_event_count(trace.path, "failure_analysis_requested"),
            failure_analysis_result_count=_failure_analyzer_result_count(
                planner.evidence.failure_analyses,
                request_ids=failure_analysis_request_ids,
            ),
            repair_plan_count=_authoritative_repair_plan_count(failure_analyzer_repair_plans),
            repair_contract_count=_repair_contract_count(failure_analyzer_repair_plans),
            repair_attempt_count=_repair_attempt_count(failure_analyzer_repair_plans),
            repair_execution_count=_repair_execution_count(
                failure_analyzer_repair_plans,
                planner.evidence.verification_results,
            ),
            repairing_failures_seen=bool(repairing_evidence.get("seen")),
            verification_contract_satisfaction=planner.assess_verification_contract_satisfaction().to_dict(),
            repair_scope=_repair_scope(failure_analyzer_repair_plans, planner.evidence.verification_results),
            final_report_status=final_report_status,
            trace_path=str(trace.path),
            phase_history=phase_history,
            planner_status_history=planner_status_history,
            repair_contract_summary=_repair_contract_summary(_latest_repair_contract(failure_analyzer_repair_plans)),
            repairing_failures_evidence=repairing_evidence,
            trace_refs=_trace_refs(trace.path),
        )


class _ScriptedProvider:
    def __init__(self, *responses: dict[str, Any]) -> None:
        self.responses = list(responses)
        self.calls: list[dict[str, Any]] = []

    def chat(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
        tool_choice: Any = None,
    ) -> dict[str, Any]:
        self.calls.append({"messages": messages, "tools": tools, "tool_choice": tool_choice})
        return self.responses.pop(0)


def _analysis_response(**payload: Any) -> dict[str, Any]:
    payload.setdefault("needs_user_input", False)
    return _assistant(json.dumps(payload))


def _assistant(content: str) -> dict[str, Any]:
    return {"choices": [{"message": {"role": "assistant", "content": content}}]}


def _tool(call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return {
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": json.dumps(arguments)},
                        }
                    ],
                }
            }
        ]
    }


def _failure_trigger(planner: Planner) -> str:
    for item in planner.evidence.verification_results:
        if (item.get("completion_assessment") or {}).get("status") == "failed":
            return "verification_failed"
    return "completion_rejected"


def _repair_contract_count(repair_plans: list[dict[str, Any]]) -> int:
    return sum(1 for plan in repair_plans if isinstance(plan.get("repair_contract"), dict))


def _authoritative_repair_plan_count(repair_plans: list[dict[str, Any]]) -> int:
    return _repair_contract_count(repair_plans)


def _failure_analyzer_result_count(
    failure_analyses: list[dict[str, Any]],
    *,
    request_ids: set[str],
) -> int:
    return sum(
        1
        for item in failure_analyses
        if isinstance(item, dict) and str(item.get("request_id") or "") in request_ids
    )


def _failure_analysis_ids_for_requests(
    failure_analyses: list[dict[str, Any]],
    request_ids: set[str],
) -> set[str]:
    return {
        str(item.get("analysis_id"))
        for item in failure_analyses
        if isinstance(item, dict)
        and str(item.get("request_id") or "") in request_ids
        and item.get("analysis_id")
    }


def _repair_plans_for_analysis_ids(
    repair_plans: list[dict[str, Any]],
    analysis_ids: set[str],
) -> list[dict[str, Any]]:
    return [
        plan
        for plan in repair_plans
        if isinstance(plan, dict) and str(plan.get("analysis_id") or "") in analysis_ids
    ]


def _repair_attempt_count(repair_plans: list[dict[str, Any]]) -> int:
    return sum(
        1
        for plan in repair_plans
        if isinstance(plan.get("repair_contract"), dict)
        and not plan.get("needs_user_input")
        and not plan.get("blocked_reason")
    )


def _repair_execution_count(
    repair_plans: list[dict[str, Any]],
    verification_results: list[dict[str, Any]],
) -> int:
    return int(_repair_attempt_count(repair_plans) > 0 and _latest_verification_passed(verification_results))


def _latest_verification_passed(verification_results: list[dict[str, Any]]) -> bool:
    if not verification_results:
        return False
    return (verification_results[-1].get("completion_assessment") or {}).get("status") in {
        "ready",
        "ready_with_warnings",
    }


def _repair_scope(
    repair_plans: list[dict[str, Any]],
    verification_results: list[dict[str, Any]],
) -> dict[str, Any]:
    contract = _latest_repair_contract(repair_plans)
    target_files = {str(item) for item in contract.get("target_files") or []}
    changed_files: set[str] = set()
    for result in verification_results:
        for path in result.get("changed_files") or []:
            changed_files.add(str(path))
    verification_contract = VerificationContract.from_dict(
        contract.get("verification_contract") or {}
    )
    command_scope_ok = verification_contract.is_command_allowed(["python", "quicksort.py"])
    return {
        "target_files": sorted(target_files),
        "changed_files_observed": sorted(changed_files),
        "target_file_scope_ok": bool(target_files and target_files <= {"quicksort.py"}),
        "verification_command_scope_ok": command_scope_ok,
    }


def _latest_repair_contract(repair_plans: list[dict[str, Any]]) -> dict[str, Any]:
    for plan in reversed(repair_plans):
        contract = plan.get("repair_contract")
        if isinstance(contract, dict):
            return contract
    return {}


def _repair_contract_summary(contract: dict[str, Any]) -> dict[str, Any]:
    verification_plan = [str(item) for item in contract.get("verification_plan") or []]
    candidates = contract.get("action_candidates") or []
    return {
        "contract_id": str(contract.get("contract_id") or ""),
        "target_files": [str(item) for item in contract.get("target_files") or []],
        "action_count": len(candidates) if isinstance(candidates, list) else 0,
        "verification_plan": verification_plan[:5],
        "verification_step_count": len(
            ((contract.get("verification_contract") or {}).get("steps") or [])
            if isinstance(contract.get("verification_contract"), dict)
            else []
        ),
        "needs_user_input": bool(contract.get("needs_user_input")),
        "blocked_reason": str(contract.get("blocked_reason") or ""),
    }


def _planner_status_history(path: Path, planner: Planner) -> list[dict[str, str]]:
    history: list[dict[str, str]] = []
    for event in _trace_events(path):
        if event.get("event") != "planner":
            continue
        data = event.get("data") if isinstance(event.get("data"), dict) else {}
        phase = str(data.get("phase") or "")
        decision = str(data.get("decision") or "")
        status = _status_for_phase(phase)
        if phase:
            _append_history(
                history,
                {
                    "current_phase": phase,
                    "status": status,
                    "decision": decision,
                    "source": "planner_trace",
                },
            )
    state = getattr(planner, "state", None)
    if state is not None:
        status = getattr(getattr(state, "status", None), "value", "")
        phase = str(getattr(state, "current_phase", "") or "")
        _append_history(
            history,
            {
                "current_phase": phase,
                "status": status or _status_for_phase(phase),
                "decision": "final_state",
                "source": "planner_state",
            },
        )
    return history[-20:]


def _append_history(history: list[dict[str, str]], item: dict[str, str]) -> None:
    if not item.get("current_phase"):
        return
    if history and all(history[-1].get(key) == item.get(key) for key in ("current_phase", "status", "decision")):
        return
    history.append(item)


def _phase_history(planner_status_history: list[dict[str, str]]) -> list[str]:
    phases: list[str] = []
    for item in planner_status_history:
        phase = item.get("current_phase")
        if phase and (not phases or phases[-1] != phase):
            phases.append(phase)
    return phases[-20:]


def _repairing_failures_evidence(
    path: Path,
    *,
    planner_status_history: list[dict[str, str]],
    final_phase: str,
    final_status: str,
) -> dict[str, Any]:
    trace_event_count = _trace_event_count(path, "repair_signal_consumed")
    trace_phase_event_count = _trace_repairing_phase_event_count(path)
    planner_seen = any(
        item.get("current_phase") == "repairing_failures"
        or item.get("status") == "repairing_failures"
        for item in planner_status_history
    )
    final_state_seen = final_phase == "repairing_failures" or final_status == "repairing_failures"
    sources: list[str] = []
    if planner_seen:
        sources.append("planner_history")
    if final_state_seen:
        sources.append("planner_final_state")
    if trace_phase_event_count:
        sources.append("trace_event")
    return {
        "seen": bool(planner_seen or final_state_seen or trace_phase_event_count),
        "sources": sources,
        "trace_repair_signal_consumed_count": trace_event_count,
        "trace_repairing_phase_event_count": trace_phase_event_count,
    }


def _trace_refs(path: Path) -> dict[str, Any]:
    events = _trace_events(path)
    return {
        "jsonl_path": str(path),
        "event_count": len(events),
        "failure_analysis_event_count": sum(
            1 for event in events if str(event.get("event") or "").startswith("failure_analysis_")
        ),
        "repair_event_count": sum(
            1
            for event in events
            if str(event.get("event") or "") in {"repair_signal_consumed"}
            or str(event.get("event") or "").startswith("repair_")
        ),
        "planner_event_count": sum(1 for event in events if event.get("event") == "planner"),
    }


def _trace_events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    events: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            events.append(event)
    return events


def _trace_event_count(path: Path, event_name: str) -> int:
    count = 0
    for event in _trace_events(path):
        if event.get("event") == event_name or event.get("event_type") == event_name:
            count += 1
    return count


def _trace_event_request_ids(path: Path, event_name: str) -> set[str]:
    request_ids: set[str] = set()
    for event in _trace_events(path):
        if event.get("event") != event_name and event.get("event_type") != event_name:
            continue
        data = event.get("data") if isinstance(event.get("data"), dict) else {}
        payload = event.get("payload") if isinstance(event.get("payload"), dict) else {}
        request_id = str(data.get("request_id") or payload.get("request_id") or "")
        if request_id:
            request_ids.add(request_id)
    return request_ids


def _trace_repairing_phase_event_count(path: Path) -> int:
    count = 0
    for event in _trace_events(path):
        data = event.get("data") if isinstance(event.get("data"), dict) else {}
        payload = event.get("payload") if isinstance(event.get("payload"), dict) else {}
        phase = (
            data.get("phase")
            or data.get("current_phase")
            or payload.get("phase")
            or event.get("phase_id")
        )
        if phase == "repairing_failures":
            count += 1
    return count


def _status_for_phase(phase: str) -> str:
    return phase if phase else ""


def _targeted_replay_markdown(result: TargetedFailureReplayResult) -> str:
    payload = result.to_dict()
    repair_contract = payload.get("repair_contract_summary") or {}
    trace_refs = payload.get("trace_refs") or {}
    return "\n".join(
        [
            "# Targeted Failure Replay",
            "",
            f"- status: `{payload['status']}`",
            f"- agent_completed: `{payload['agent_completed']}`",
            f"- entered_agent_loop: `{payload['entered_agent_loop']}`",
            f"- failure_trigger: `{payload['failure_trigger']}`",
            f"- repairing_failures_seen: `{payload['repairing_failures_seen']}`",
            f"- failure_analysis_request_count: {payload['failure_analysis_request_count']}",
            f"- failure_analysis_result_count: {payload['failure_analysis_result_count']}",
            f"- repair_plan_count: {payload['repair_plan_count']}",
            f"- repair_contract_count: {payload['repair_contract_count']}",
            f"- repair_attempt_count: {payload['repair_attempt_count']}",
            f"- repair_execution_count: {payload['repair_execution_count']}",
            f"- verification_contract_satisfied: `{(payload.get('verification_contract_satisfaction') or {}).get('satisfied')}`",
            f"- repair_target_files: `{', '.join(repair_contract.get('target_files') or []) or '-'}`",
            f"- trace_jsonl: `{trace_refs.get('jsonl_path', '')}`",
            f"- trace_event_count: {trace_refs.get('event_count', 0)}",
            "",
        ]
    )


def _tool_protocol_state_path(workspace: Path, trace: Any) -> Path:
    run_dir = getattr(getattr(trace, "store", None), "run_dir", None)
    if run_dir is not None:
        return Path(run_dir) / "tool_protocol.sqlite3"
    run_id = str(getattr(trace, "run_id", "default"))
    return workspace / ".singularity" / "runs" / run_id / "tool_protocol.sqlite3"


QUICK_SORT = """\
def quicksort(values):
    if len(values) <= 1:
        return values
    pivot = values[0]
    tail = values[1:]
    return quicksort([item for item in tail if item <= pivot]) + [pivot] + quicksort([item for item in tail if item > pivot])


if __name__ == "__main__":
    assert quicksort([3, 1, 2]) == [1, 2, 3]
    print("ok")
"""
