import json
from pathlib import Path
from typing import Any

from singularity.agent import SingularityAgentRunStatus
from singularity.command import CommandRuntime
from singularity.context import ContextManager
from singularity.edit import EditRuntime
from singularity.planner import PlannerRuntime
from singularity.policy import (
    ApprovalMode,
    DecisionOutcome,
    OperationKind,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicyRuntime,
    RiskLevel,
    SecurityMode,
)
from singularity.task_controller import TaskLifecycleStatus
from singularity.tools import ToolRegistry
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.trace import TraceWriter
from singularity.verification import VerificationRuntime
from singularity.workspace import MutationRuntime
from tests.agent_runtime_helpers import make_agent_session


class FakeProvider:
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


class DenyMutationPolicyRuntime:
    def __init__(self, workspace_root: Path) -> None:
        self.config = PolicyConfig(workspace_root=workspace_root, approval_mode=ApprovalMode.AUTO_SAFE)
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
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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

    assert result.status == SingularityAgentRunStatus.COMPLETED
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
    smoke = next(item for item in latest["results"] if item["kind"] == "runtime_smoke")
    assert smoke["evidence"]["exit_code"] == 0
    assert "ok" in smoke["evidence"]["stdout_excerpt"]
    assert smoke["evidence"]["stderr_excerpt"] == ""


def test_ready_verification_finalizes_without_extra_model_turn(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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

    assert result.status == SingularityAgentRunStatus.COMPLETED
    assert result.turn == 4
    assert "status: completed" in result.final_answer
    assert len(provider.calls) == 4
    assert planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "ready"


def test_malformed_tool_args_record_retryable_outcome(tmp_path: Path) -> None:
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    provider = FakeProvider(
        raw_tool("call_bad", "read_file", "{not json"),
        assistant("still done too early"),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=2)

    result = agent.run("inspect then change code")

    assert result.status == SingularityAgentRunStatus.MAX_TURNS_EXCEEDED
    assert len(provider.calls) == 2
    assert [item["status"] for item in planner.evidence.task_outcomes] == [
        "retryable",
        "replan_required",
    ]
    retryable = next(item for item in planner.evidence.task_outcomes if item["status"] == "retryable")
    assert retryable["error_code"] in {"invalid_json", "bad_arguments_json"}
    assert retryable["next_action"] == "retry"
    assert retryable["retry_allowed"] is True


def test_verification_failure_replans_instead_of_completing(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
        assistant("done despite failing verification"),
    )
    agent = make_task_agent(tmp_path, provider=provider, planner=planner, max_turns=5)

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == SingularityAgentRunStatus.MAX_TURNS_EXCEEDED
    assert planner.state is not None
    assert planner.state.current_phase == "repairing_failures"
    assert planner.evidence.verification_results[-1]["completion_assessment"]["status"] == "failed"
    rejected = planner.evidence.task_outcomes[-1]
    assert rejected["status"] == "replan_required"
    assert rejected["error_code"] == "completion_rejected"
    assert rejected["next_action"] == "continue"
    assert rejected["retry_allowed"] is True


def test_policy_denial_blocks_without_bypassing_policy(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
    policy = DenyMutationPolicyRuntime(tmp_path)
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
        policy_runtime=policy,
        max_turns=5,
    )

    result = agent.run("implement quicksort.py and verify it")

    assert result.status == SingularityAgentRunStatus.BLOCKED
    assert result.error_code == "policy_denied"
    assert not (tmp_path / "quicksort.py").exists()
    assert any(
        request.operation in {OperationKind.CREATE_FILE, OperationKind.MUTATE_FILE}
        for request in policy.requests
    )


def test_approval_wait_keeps_context_for_resume(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("task context", encoding="utf-8")
    planner = PlannerRuntime(tmp_path, session_id="session_1", task_id="task_1")
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
    policy = PolicyRuntime(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_mode=ApprovalMode.REVIEW_ALL,
            security_mode=SecurityMode.COMPAT,
        )
    )
    agent = make_task_agent(
        tmp_path,
        provider=provider,
        planner=planner,
        policy_runtime=policy,
        context_manager=context,
        max_turns=3,
    )

    result = agent.run("create needs-review.txt")

    assert result.status == SingularityAgentRunStatus.BLOCKED
    assert result.error_code == "approval_required"
    assert planner.state is not None
    assert planner.state.lifecycle_status == TaskLifecycleStatus.WAITING_APPROVAL.value
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
    planner: PlannerRuntime,
    policy_runtime: Any | None = None,
    context_manager: ContextManager | None = None,
    max_turns: int = 6,
):
    trace = TraceWriter.create(tmp_path)
    policy = policy_runtime or PolicyRuntime(
        PolicyConfig(
            workspace_root=tmp_path,
            approval_mode=ApprovalMode.AUTO_SAFE,
            security_mode=SecurityMode.COMPAT,
        )
    )
    tools = ToolRegistry(tmp_path)
    mutation_runtime = MutationRuntime(tmp_path, trace=trace, planner=planner, policy_runtime=policy)
    register_mutation_tools(tools, mutation_runtime)
    edit_runtime = EditRuntime(tmp_path, mutation_runtime=mutation_runtime, trace=trace, planner=planner)
    register_edit_tools(tools, edit_runtime)
    command_runtime = CommandRuntime(tmp_path, trace=trace, planner=planner, policy_runtime=policy)
    verification_runtime = VerificationRuntime(
        tmp_path,
        command_runtime=command_runtime,
        trace=trace,
        planner=planner,
        policy_runtime=policy,
    )
    register_verification_tools(tools, verification_runtime)
    return make_agent_session(
        tmp_path,
        provider=provider,
        tools=tools,
        trace=trace,
        max_turns=max_turns,
        planner=planner,
        policy_runtime=policy,
        context_manager=context_manager,
    )


def assistant(content: str) -> dict[str, Any]:
    return {"choices": [{"message": {"role": "assistant", "content": content}}]}


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
