import json
from pathlib import Path
from typing import Any

from singularity.agent_loop import AgentLoopStatus
from singularity.command import CommandExecutor
from singularity.context import ContextManager
from singularity.edit import EditExecutor
from singularity.model import ModelError, ModelErrorKind
from singularity.planner import Planner, TaskStatus
from singularity.policy import (
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicyEngine,
    RiskLevel,
)
from singularity.policy.permissions import PermissionProfile, PermissionProfileName
from singularity.run_controller import RunLifecycleStatus
from singularity.tools import ToolRegistry
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.verification import VerificationRunner
from singularity.workspace import WorkspaceMutationManager
from tests.agent_loop_helpers import make_agent_session


class FakeProvider:
    def __init__(self, *responses: Any) -> None:
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
        response = self.responses.pop(0)
        if isinstance(response, BaseException):
            raise response
        return response


class DenyMutationPolicyEngine:
    def __init__(self, workspace_root: Path) -> None:
        self.config = PolicyConfig(workspace_root=workspace_root)
        self.requests: list[PolicyRequest] = []

    def evaluate(self, request: PolicyRequest) -> PolicyDecision:
        self.requests.append(request)
        outcome = (
            DecisionOutcome.DENY
            if request.operation in {OperationKind.CREATE_FILE, OperationKind.MUTATE_FILE}
            else DecisionOutcome.ALLOW
        )
        return PolicyDecision(
            request_id=request.request_id,
            outcome=outcome,
            reason=f"{outcome.value} from task outcome test",
            risk_level=RiskLevel.MEDIUM,
        )

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        return self.evaluate(request)

    def register_grant(self, _grant: Any) -> None:
        return None


def test_premature_final_then_quicksort_smoke_completes(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        assistant("done too early"),
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create",
            "write_file",
            {
                "path": "quicksort.py",
                "content": QUICK_SORT,
                "mode": "create",
                "reason": "create deterministic quicksort smoke target",
            },
        ),
        tool(
            "call_verify",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        assistant("done with evidence"),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.COMPLETED
    assert "status: completed" in result.final_answer
    assert (tmp_path / "quicksort.py").exists()
    assert len(provider.calls) == 5
    assert len(provider.responses) == 1
    rejected = planner.evidence.task_outcomes[0]
    assert rejected["status"] == "replan_required"
    assert rejected["error_code"] == "completion_rejected"
    assert rejected["next_action"] == "continue"
    assert rejected["retry_allowed"] is True
    latest = planner.evidence.verification_results[-1]
    smoke = next(item for item in latest["results"] if item["kind"] == "verification_smoke")
    assert smoke["evidence"]["exit_code"] == 0
    assert "ok" in smoke["evidence"]["stdout_excerpt"]
    assert smoke["evidence"]["stderr_excerpt"] == ""


def test_ready_verification_finalizes_without_extra_model_turn(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create",
            "write_file",
            {
                "path": "quicksort.py",
                "content": QUICK_SORT,
                "mode": "create",
                "reason": "create deterministic quicksort smoke target",
            },
        ),
        tool(
            "call_verify",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=4)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.COMPLETED
    assert result.turn == 4
    assert "status: completed" in result.final_answer
    assert len(provider.calls) == 4
    assert planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "ready"


def test_malformed_tool_args_record_retryable_outcome(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        raw_tool("call_bad", "read_file", "{not json"),
        assistant("still done too early"),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=2)

    result = agent.run("inspect then change code")

    assert result.status == AgentLoopStatus.MAX_TURNS_EXCEEDED
    assert result.error_code == "max_turns_exceeded"
    assert len(provider.calls) == 2
    assert [item["status"] for item in planner.evidence.task_outcomes[:2]] == [
        "retryable",
        "replan_required",
    ]
    assert planner.evidence.task_outcomes[-1]["status"] == "blocked"
    assert planner.evidence.task_outcomes[-1]["error_code"] == "max_turns_exceeded"
    retryable = next(item for item in planner.evidence.task_outcomes if item["status"] == "retryable")
    assert retryable["error_code"] in {"invalid_json", "bad_arguments_json"}
    assert retryable["next_action"] == "retry"
    assert retryable["retry_allowed"] is True


def test_verification_failure_replans_instead_of_completing(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        analysis_response(
            root_cause="quicksort.py raises ZeroDivisionError during smoke verification.",
            failure_category="unit_test_failure",
            affected_files=["quicksort.py"],
            evidence_refs=["call_verify"],
            repair_strategy="patch the failing file and rerun the smoke command",
            next_actions=["Patch quicksort.py.", "Rerun python quicksort.py."],
            verification_plan=["python quicksort.py"],
            confidence=0.9,
        ),
        assistant("done despite failing verification"),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=5)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "repair_budget_exceeded"
    assert planner.state is not None
    assert planner.state.current_phase == "repairing_failures"
    assert planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "failed"
    rejected = next(
        item
        for item in reversed(planner.evidence.task_outcomes)
        if item.get("error_code") == "completion_rejected"
    )
    assert rejected["status"] == "replan_required"
    assert rejected["error_code"] == "completion_rejected"
    assert rejected["next_action"] == "continue"
    assert rejected["retry_allowed"] is True
    blocked = planner.evidence.task_outcomes[-1]
    assert blocked["status"] == "blocked"
    assert blocked["error_code"] == "repair_budget_exceeded"
    assert blocked["retry_allowed"] is False
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "unit_test_failure"
    assert planner.evidence.repair_plans[-1]["strategy"] == "patch the failing file and rerun the smoke command"


def test_tool_failure_then_verification_failure_replans_repairs_and_finalizes(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        raw_tool("call_bad_args", "read_file", "{not json"),
        tool("call_read", "read_file", {"path": "README.md"}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        analysis_response(
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
        tool(
            "call_repair",
            "write_file",
            {
                "path": "quicksort.py",
                "content": QUICK_SORT,
                "mode": "overwrite",
                "reason": "repair failing smoke target",
            },
        ),
        tool(
            "call_verify_fixed",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.COMPLETED
    assert result.turn == 6
    assert "status: completed" in result.final_answer
    assert (tmp_path / "quicksort.py").read_text(encoding="utf-8") == QUICK_SORT
    assert any(
        item["status"] == "retryable"
        and item["error_code"] in {"invalid_json", "bad_arguments_json"}
        for item in planner.evidence.task_outcomes
    )
    assert planner.evidence.verification_results[-2]["completion_assessment"]["status"] == "failed"
    assert planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "ready"
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "unit_test_failure"
    assert planner.evidence.repair_plans[-1]["strategy"] == (
        "replace the failing smoke target with a real quicksort implementation"
    )
    assert provider.calls[4]["tools"] == []
    repair_context = json.dumps(provider.calls[5]["messages"], ensure_ascii=False)
    assert "repair_plan" in repair_context
    assert "repair_contract" in repair_context
    assert "target_files" in repair_context
    assert "allowed_tool_names" in repair_context
    assert "ZeroDivisionError" in repair_context
    contract = planner.evidence.repair_plans[-1]["repair_contract"]
    repair_tool_names = {item["function"]["name"] for item in provider.calls[5]["tools"]}
    assert repair_tool_names <= set(contract["allowed_tool_names"])
    assert {"run_verification", "write_file"} <= repair_tool_names
    trace_events = [
        json.loads(line)["event"]
        for trace_path in (tmp_path / ".singularity" / "runs").glob("*.jsonl")
        for line in trace_path.read_text(encoding="utf-8").splitlines()
    ]
    assert "failure_analysis_requested" in trace_events
    assert "failure_analysis_completed" in trace_events
    assert "repair_contract_validation" in trace_events
    assert "repair_signal_consumed" in trace_events
    assert planner.state is not None
    assert planner.state.status == TaskStatus.COMPLETED


def test_unrepairable_verification_failure_blocks_with_user_input_required(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        analysis_response(
            root_cause="The verification output does not identify a safe repair target.",
            failure_category="missing_information",
            affected_files=[],
            evidence_refs=["call_verify_bad"],
            repair_strategy="ask the user for the intended behavior before editing",
            next_actions=["Ask for the expected quicksort.py behavior."],
            verification_plan=[],
            confidence=0.2,
            needs_user_input=True,
            blocked_reason="missing expected behavior",
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "failure_analysis_user_input_required"
    assert planner.state is not None
    assert planner.state.status == TaskStatus.BLOCKED
    assert planner.evidence.failure_analyses[-1]["needs_user_input"] is True
    assert planner.evidence.repair_plans[-1]["blocked_reason"] == "missing expected behavior"


def test_invalid_analyzer_json_blocks_without_repairing(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        assistant("not json"),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "failure_analysis_user_input_required"
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "failure_analysis_invalid_json"
    assert planner.evidence.repair_plans[-1]["needs_user_input"] is True
    assert planner.evidence.repair_plans[-1]["repair_contract"]["validation_errors"]


def test_analyzer_model_failure_blocks_with_user_input_required(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        ModelError(
            kind=ModelErrorKind.NETWORK_ERROR,
            message="failure analyzer provider unavailable",
            retryable=False,
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "failure_analysis_user_input_required"
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "failure_analysis_unavailable"
    assert "failure analyzer provider unavailable" in planner.evidence.repair_plans[-1]["blocked_reason"]


def test_low_confidence_analysis_blocks_repair_contract(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        analysis_response(
            root_cause="The failure might be in quicksort.py but confidence is too low.",
            failure_category="unit_test_failure",
            affected_files=["quicksort.py"],
            evidence_refs=["call_verify_bad"],
            repair_strategy="guess at a repair",
            next_actions=["Patch quicksort.py."],
            verification_plan=["python quicksort.py"],
            confidence=0.1,
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "failure_analysis_user_input_required"
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "failure_analysis_schema_invalid"
    assert "confidence below repair threshold" in planner.evidence.repair_plans[-1]["blocked_reason"]


def test_unauthorized_affected_files_block_repair_contract(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        analysis_response(
            root_cause="The model tries to edit an unrelated file.",
            failure_category="unit_test_failure",
            affected_files=["other.py"],
            evidence_refs=["call_verify_bad"],
            repair_strategy="patch unrelated file",
            next_actions=["Patch other.py."],
            verification_plan=["python quicksort.py"],
            confidence=0.9,
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "failure_analysis_user_input_required"
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "failure_analysis_schema_invalid"
    assert "unauthorized target" in planner.evidence.repair_plans[-1]["blocked_reason"]


def test_repeated_completion_rejected_escalates_to_failure_analyzer(tmp_path: Path) -> None:
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        assistant("done without evidence"),
        assistant("still done without evidence"),
        analysis_response(
            root_cause="The model repeatedly finalized without required evidence.",
            failure_category="missing_information",
            affected_files=[],
            evidence_refs=["execution_outcome:completion_rejected"],
            repair_strategy="ask for missing evidence decision",
            next_actions=["Ask the user whether to continue without evidence."],
            verification_plan=[],
            confidence=0.3,
            needs_user_input=True,
            blocked_reason="missing required completion evidence",
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=3)

    result = agent.run("change code")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "failure_analysis_user_input_required"
    assert provider.calls[2]["tools"] == []
    assert planner.evidence.failure_analyses[-1]["failure_category"] == "missing_information"
    assert planner.evidence.repair_plans[-1]["blocked_reason"] == "missing required completion evidence"


def test_repeated_failure_fingerprint_budget_blocks_after_second_failed_verification(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create_bad",
            "write_file",
            {
                "path": "quicksort.py",
                "content": "print(1 / 0)\n",
                "mode": "create",
                "reason": "create failing smoke target",
            },
        ),
        tool(
            "call_verify_bad",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
        analysis_response(
            root_cause="quicksort.py raises ZeroDivisionError during smoke verification.",
            failure_category="unit_test_failure",
            affected_files=["quicksort.py"],
            evidence_refs=["call_verify_bad"],
            repair_strategy="patch quicksort.py and rerun verification",
            next_actions=["Rerun python quicksort.py without changing the file."],
            verification_plan=["python quicksort.py"],
            confidence=0.9,
        ),
        tool(
            "call_verify_bad_again",
            "run_verification",
            {
                "changed_files": ["quicksort.py"],
                "task_intent": "verify quicksort script",
                "smoke_commands": [["python", "quicksort.py"]],
            },
        ),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=6)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "repair_budget_exceeded"
    assert planner.state is not None
    assert planner.state.status == TaskStatus.BLOCKED
    assert "repeated_failure" in planner.state.blocked_reasons
    llm_analyses = [item for item in planner.evidence.failure_analyses if item.get("request_id")]
    assert len(llm_analyses) == 1


def test_policy_denial_blocks_without_bypassing_policy(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    policy = DenyMutationPolicyEngine(tmp_path)
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create",
            "write_file",
            {
                "path": "quicksort.py",
                "content": QUICK_SORT,
                "mode": "create",
                "reason": "policy should block this mutation",
            },
        ),
    )
    agent = make_task_agent(
        tmp_path,
        provider=provider,
        planner=planner,
        policy_engine=policy,
        max_turns=5,
    )

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "policy_denied"
    assert not (tmp_path / "quicksort.py").exists()
    assert planner.evidence.failure_analyses == []
    assert planner.evidence.repair_plans == []
    assert any(
        request.operation in {OperationKind.CREATE_FILE, OperationKind.MUTATE_FILE}
        for request in policy.requests
    )


def test_approval_wait_keeps_context_for_resume(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = Planner(tmp_path, session_id="session_1", task_id="task_1")
    context = ContextManager(
        system_prompt="system",
        user_goal="placeholder",
        db_path=tmp_path / "context.sqlite3",
    )
    provider = FakeProvider(
        tool("call_read_1", "read_file", {"path": "README.md"}),
        tool("call_read_2", "read_file", {"path": "README.md", "max_bytes": 20}),
        tool(
            "call_create",
            "write_file",
            {
                "path": "needs-review.txt",
                "content": "approval required\n",
                "mode": "create",
                "reason": "exercise pending approval lifecycle",
            },
        )
    )
    policy = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=PermissionProfile.default_for_workspace(
                tmp_path,
                profile=PermissionProfileName.READ_ONLY,
            ),
        )
    )
    agent = make_task_agent(
        tmp_path,
        provider=provider,
        planner=planner,
        policy_engine=policy,
        context_manager=context,
        max_turns=3,
    )

    result = agent.run("create needs-review.txt")

    assert result.status == AgentLoopStatus.BLOCKED
    assert result.error_code == "approval_required"
    assert planner.state is not None
    assert planner.state.lifecycle_status == RunLifecycleStatus.WAITING_APPROVAL.value
    messages = context.messages()
    assert any(message["role"] == "user" and "create needs-review.txt" in message["content"] for message in messages)
    assert any(message["role"] == "assistant" and message.get("tool_calls") for message in messages)
    assert any(
        message["role"] == "tool" and "approval_required" in str(message.get("content"))
        for message in messages
    )


def make_task_agent(
    tmp_path: Path,
    *,
    provider: FakeProvider,
    planner: Planner,
    policy_engine: Any | None = None,
    context_manager: ContextManager | None = None,
    max_turns: int = 6,
):
    trace = JsonlTraceRecorder.create(tmp_path)
    if planner.trace is None:
        planner.trace = trace
    policy = policy_engine or PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=PermissionProfile.default_for_workspace(
                tmp_path,
                profile=PermissionProfileName.DANGER_FULL_ACCESS,
            ),
        )
    )
    tools = ToolRegistry(tmp_path)
    mutation_manager = WorkspaceMutationManager(tmp_path, trace=trace, planner=planner, policy_engine=policy)
    register_mutation_tools(tools, mutation_manager)
    edit_executor = EditExecutor(tmp_path, mutation_manager=mutation_manager, trace=trace, planner=planner)
    register_edit_tools(tools, edit_executor)
    command_executor = CommandExecutor(tmp_path, trace=trace, planner=planner, policy_engine=policy)
    verification_runner = VerificationRunner(
        tmp_path,
        command_executor=command_executor,
        trace=trace,
        planner=planner,
        policy_engine=policy,
    )
    register_verification_tools(tools, verification_runner)
    return make_agent_session(
        tmp_path,
        provider=provider,
        tools=tools,
        trace=trace,
        max_turns=max_turns,
        planner=planner,
        policy_engine=policy,
        context_manager=context_manager,
    )


def assistant(content: str) -> dict[str, Any]:
    return {"choices": [{"message": {"role": "assistant", "content": content}}]}


def analysis_response(**payload: Any) -> dict[str, Any]:
    payload.setdefault("needs_user_input", False)
    return assistant(json.dumps(payload))


def tool(call_id: str, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    return raw_tool(call_id, name, json.dumps(arguments))


def raw_tool(call_id: str, name: str, raw_arguments: str) -> dict[str, Any]:
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
                            "function": {"name": name, "arguments": raw_arguments},
                        }
                    ],
                }
            }
        ]
    }


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
