from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from rich.console import Console

from singularity.agent_loop import AgentLoop, AgentLoopStatus
from singularity.command import CommandExecutor
from singularity.edit import EditExecutor
from singularity.failure_analysis import VerificationContract
from singularity.instructions import PromptAssemblyPipeline
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.model import ModelRunner
from singularity.planner import Planner
from singularity.policy import ApprovalMode, PolicyConfig, PolicyEngine, SecurityMode
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.verification import VerificationRunner
from singularity.workspace import WorkspaceMutationManager


TARGETED_REPLAY_SCHEMA_VERSION = "evaluation.targeted_failure_replay/v1"


@dataclass(frozen=True)
class TargetedFailureReplayResult:
    status: str
    completed: bool
    entered_agent_loop: bool
    agent_loop_ref: str
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
    model_visible_objects: list[str] = field(default_factory=list)
    evaluator_internal_objects: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema_version": TARGETED_REPLAY_SCHEMA_VERSION,
            "status": self.status,
            "completed": self.completed,
            "entered_agent_loop": self.entered_agent_loop,
            "agent_loop_ref": self.agent_loop_ref,
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
            "model_visible_objects": list(self.model_visible_objects),
            "evaluator_internal_objects": list(self.evaluator_internal_objects),
        }


class TargetedFailureReplayRunner:
    """Run a deterministic failure-repair smoke through the real AgentLoop."""

    def __init__(self, *, workspace_root: Path | str, max_turns: int = 6) -> None:
        self.workspace_root = Path(workspace_root).resolve(strict=False)
        self.max_turns = max_turns

    def run_smoke(self) -> TargetedFailureReplayResult:
        workspace = self.workspace_root
        workspace.mkdir(parents=True, exist_ok=True)
        (workspace / "README.md").write_text("targeted repair replay fixture\n", encoding="utf-8")
        planner = Planner(workspace, session_id="targeted_replay", task_id="targeted_replay")
        trace = JsonlTraceRecorder.create(workspace)
        planner.trace = trace
        policy = PolicyEngine(
            PolicyConfig(
                workspace_root=workspace,
                approval_mode=ApprovalMode.AUTO_SAFE,
                security_mode=SecurityMode.COMPAT,
            )
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
            policy_engine=policy,
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
        final_report_status = (
            planner.final_report.status.value
            if planner.final_report is not None
            else getattr(getattr(planner.state, "status", None), "value", "")
        )
        return TargetedFailureReplayResult(
            status=agent_result.status.value,
            completed=agent_result.status == AgentLoopStatus.COMPLETED,
            entered_agent_loop=bool(provider.calls),
            agent_loop_ref="AgentLoop.run",
            failure_trigger=_failure_trigger(planner),
            failure_analysis_request_count=_trace_event_count(trace.path, "failure_analysis_requested"),
            failure_analysis_result_count=_failure_analyzer_result_count(planner.evidence.failure_analyses),
            repair_plan_count=_authoritative_repair_plan_count(planner.evidence.repair_plans),
            repair_contract_count=_repair_contract_count(planner.evidence.repair_plans),
            repair_attempt_count=_repair_attempt_count(planner.evidence.repair_plans),
            repair_execution_count=_repair_execution_count(planner.evidence.repair_plans, planner.evidence.verification_results),
            repairing_failures_seen=_trace_event_count(trace.path, "repair_signal_consumed") > 0,
            verification_contract_satisfaction=planner.assess_verification_contract_satisfaction().to_dict(),
            repair_scope=_repair_scope(planner.evidence.repair_plans, planner.evidence.verification_results),
            final_report_status=final_report_status,
            trace_path=str(trace.path),
            model_visible_objects=[
                "FailureAnalysisRequest.to_model_payload",
                "RepairContract projected through PlannerContextRenderer",
                "VerificationContract command steps",
            ],
            evaluator_internal_objects=[
                "FailureCaseRecord",
                "FailureCaseReplayRunner.extract",
                "failure_cases.json",
            ],
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


def _failure_analyzer_result_count(failure_analyses: list[dict[str, Any]]) -> int:
    return sum(1 for item in failure_analyses if isinstance(item, dict) and item.get("request_id"))


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


def _trace_event_count(path: Path, event_name: str) -> int:
    if not path.exists():
        return 0
    count = 0
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("event") == event_name or event.get("event_type") == event_name:
            count += 1
    return count


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
