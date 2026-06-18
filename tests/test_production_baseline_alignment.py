from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict
from typer.testing import CliRunner

from miniharness.cli import app
from miniharness.config import ProductionRuntimeConfig
from miniharness.context import ContextManager
from miniharness.context.models import ContextItemType, ContextRuntime
from miniharness.model import (
    ModelMessage,
    ModelPurpose,
    ModelRuntime,
    ModelTurnResult,
    ModelTurnStatus,
    ModelToolCall,
    ModelToolParseStatus,
    MockModelProvider,
)
from miniharness.observability import TraceRuntime
from miniharness.observability.artifacts import TraceArtifactStore
from miniharness.observability.models import TraceArtifactKind
from miniharness.policy import ApprovalMode, DecisionOutcome
from miniharness.tool_protocol.models import (
    ToolCallEnvelope,
    ToolCallPhase,
    ToolProtocolResultEnvelope,
    ToolProtocolTurnStatus,
)
from miniharness.tool_protocol.runtime import ToolCallingProtocolRuntime
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolPolicy,
    ToolRegistry,
    ToolRuntime,
    ToolSideEffectKind,
    ToolSpec,
)
from tests.agent_runtime_helpers import make_agent_session
from tests.test_tool_runtime_policy_approval import (
    EmptyInput,
    SequencedPolicyRuntime,
    make_tool_call,
)
from tests.tool_runtime_helpers import make_test_policy_runtime


def test_cli_help_exposes_production_baseline_options_without_legacy_copy() -> None:
    result = CliRunner().invoke(app, ["main", "--help"], terminal_width=220)

    assert result.exit_code == 0
    output = result.output
    assert "production-grade local CLI coding agent harness" in output
    assert "minimal" not in output.lower()
    assert "read-only agent loop" not in output.lower()
    for option in [
        "--max-turns",
        "--approval-mode",
        "--trace-dir",
        "--context-db",
        "--model",
        "--base-url",
        "--raw-artifacts",
        "--no-raw-artif",
        "--resume",
        "--dry-run",
        "--strict",
    ]:
        assert option in output


def test_production_runtime_config_maps_cli_policy_and_model_overrides(tmp_path: Path) -> None:
    config = ProductionRuntimeConfig.from_cli(
        project_root=tmp_path,
        max_turns=3,
        approval_mode="read_only",
        strict=True,
        dry_run=True,
        trace_dir=tmp_path / "traces",
        context_db=tmp_path / "context.sqlite3",
        model="override-model",
        base_url="https://example.test/v1",
        raw_artifacts=False,
        resume_session="session_1",
    )

    assert config.max_turns == 3
    assert config.approval_mode == ApprovalMode.READ_ONLY
    assert config.strict is True
    assert config.dry_run is True
    assert config.trace_dir == tmp_path / "traces"
    assert config.context_db == tmp_path / "context.sqlite3"
    assert config.model == "override-model"
    assert config.base_url == "https://example.test/v1"
    assert config.raw_artifacts is False
    assert config.resume_session == "session_1"
    assert config.to_policy_config().approval_mode == ApprovalMode.READ_ONLY
    model_config = config.to_model_runtime_config()
    assert model_config.default_model == "override-model"
    assert model_config.providers["openai_compatible"]["base_url"] == "https://example.test/v1"
    assert model_config.store_raw_responses is False


def test_tool_policy_is_not_runtime_permission_decider(tmp_path: Path) -> None:
    calls: list[str] = []
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_via_policy_runtime",
            description="write",
            input_model=EmptyInput,
            handler=lambda _args: calls.append("called") or {"ok": True},
            permission_level=PermissionLevel.WRITE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
            uses_mutation_runtime=True,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=SequencedPolicyRuntime([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
    )

    result = runtime.execute_tool_call(make_tool_call("write_via_policy_runtime"))

    assert result.ok is True
    assert calls == ["called"]
    assert "policy_decision_id" in result.metadata


def test_dry_run_blocks_side_effect_tools_before_handler(tmp_path: Path) -> None:
    calls: list[str] = []
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="mutate_for_real",
            description="mutation",
            input_model=EmptyInput,
            handler=lambda _args: calls.append("called") or {"ok": True},
            permission_level=PermissionLevel.WRITE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_RUNTIME,
            uses_mutation_runtime=True,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
        )
    )
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=SequencedPolicyRuntime([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
        dry_run=True,
    )

    result = runtime.execute_tool_call(make_tool_call("mutate_for_real"))

    assert result.ok is False
    assert result.error_code == "dry_run_blocked"
    assert calls == []


def test_protocol_default_state_store_lives_in_trace_run_dir(tmp_path: Path) -> None:
    trace = TraceRuntime.create(tmp_path, trace_dir=tmp_path / "trace-root")
    runtime = ToolCallingProtocolRuntime(
        registry=ToolRegistry(tmp_path),
        trace=trace,
    )

    assert runtime.state_store.db_path == trace.store.run_dir / "tool_protocol.sqlite3"


def test_protocol_replay_classifies_read_only_side_effect_and_conflict(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "run" / "tool_protocol.sqlite3")
    batch_call = _protocol_call("call_1", raw_arguments='{"path":"README.md"}')
    batch = {
        "batch_id": "batch_1",
        "run_id": "run_1",
        "session_id": "session_1",
        "task_id": "task_1",
        "phase_id": "phase_1",
        "model_request_id": "req_1",
        "model_response_id": "resp_1",
        "assistant_message": {"role": "assistant", "tool_calls": []},
        "tool_calls": [batch_call.to_dict()],
    }
    store.save_batch(batch)
    record = store.upsert_record(batch_call, batch_id="batch_1", phase=ToolCallPhase.SUCCEEDED)
    store.bind_result(
        record.record_id,
        result=ToolProtocolResultEnvelope(
            tool_call_id="call_1",
            tool_name="read_file",
            ok=True,
            status="ok",
            content_preview="preview",
            content_digest="digest",
            redacted=True,
        ),
    )

    readonly = store.check_replay(batch_call, side_effects=ToolSideEffectKind.READ_WORKSPACE, idempotent=True)
    side_effect = store.check_replay(
        batch_call,
        side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
        idempotent=True,
    )
    conflict = store.check_replay(
        _protocol_call("call_1", raw_arguments='{"path":"other.md"}'),
        side_effects=ToolSideEffectKind.READ_WORKSPACE,
        idempotent=True,
    )

    assert readonly.status == "read_only_replay"
    assert readonly.allowed is True
    assert side_effect.status == "side_effect_replay"
    assert side_effect.allowed is False
    assert conflict.status == "conflicting_replay"
    assert conflict.allowed is False


def test_protocol_recovery_reports_pending_approval(tmp_path: Path) -> None:
    store = ToolProtocolStateStore(tmp_path / "run" / "tool_protocol.sqlite3")
    call = _protocol_call("call_review", raw_arguments='{"path":"README.md"}')
    store.save_batch(
        {
            "batch_id": "batch_review",
            "run_id": "run_1",
            "session_id": "session_1",
            "task_id": "task_1",
            "phase_id": "phase_1",
            "model_request_id": "req_1",
            "model_response_id": "resp_1",
            "assistant_message": {"role": "assistant", "tool_calls": []},
            "tool_calls": [call.to_dict()],
        }
    )
    store.upsert_record(call, batch_id="batch_review", phase=ToolCallPhase.WAITING_APPROVAL)
    runtime = ToolCallingProtocolRuntime(
        registry=ToolRegistry(tmp_path),
        trace=None,
        state_store=store,
    )

    recovered = runtime.recover_pending(run_id="run_1")

    assert recovered.status == ToolProtocolTurnStatus.PENDING_APPROVAL
    assert recovered.pending_approval_count == 1
    assert recovered.next_action == "resume_pending_approval"
    assert "pending approval: call_review" in recovered.recovery_report["warnings"]


def test_context_protocol_result_is_default_structured_entry_without_raw_payload() -> None:
    context = ContextManager(system_prompt="system", user_goal="goal")
    observation = context.add_tool_protocol_result(
        ToolProtocolResultEnvelope(
            tool_call_id="call_read",
            tool_name="read_file",
            ok=True,
            status="ok",
            content_preview="secret=sk-test-value",
            content_digest="digest",
            redacted=True,
            raw_result_ref="artifact_digest",
            metadata={"raw_result": "must not render"},
        )
    )

    message_payload = json.loads(context.messages()[-1]["content"])
    assert observation.tool_name == "read_file"
    assert message_payload["tool_call_id"] == "call_read"
    assert message_payload["result_ref"] == "artifact_digest"
    assert "raw_result" not in message_payload
    assert "sk-test-value" not in context.messages()[-1]["content"]
    item = context.store.load_item(observation.id)
    assert item is not None
    assert item.item_type == ContextItemType.TOOL_OBSERVATION
    assert item.source_runtime == ContextRuntime.TOOL_PROTOCOL


def test_workspace_state_uses_dedicated_context_item_not_tool_result() -> None:
    context = ContextManager(system_prompt="system", user_goal="goal")
    item = context.add_workspace_state({"status": "clean", "secret": "sk-test-value"})

    assert item.item_type == ContextItemType.WORKSPACE_STATE
    assert item.source_runtime == ContextRuntime.WORKSPACE_STATE
    assert all(message["role"] != "tool" for message in context.messages())
    stored = context.store.load_item(item.item_id)
    assert stored is not None
    assert "sk-test-value" not in json.dumps(stored.content, ensure_ascii=False)


def test_sensitive_trace_artifacts_are_redacted_even_when_raw_artifacts_enabled(tmp_path: Path) -> None:
    store = TraceArtifactStore(tmp_path, run_id="run_1", session_id="session_1")

    artifact = store.write_text_artifact(
        kind=TraceArtifactKind.MODEL_MESSAGE,
        text='{"message":"OPENAI_API_KEY=sk-secret-value"}',
        sensitive=True,
    )

    assert artifact.redacted is True
    assert "sk-secret-value" not in artifact.path.read_text(encoding="utf-8")


def test_min_agent_uses_injected_protocol_and_tool_runtime(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("hello", encoding="utf-8")
    provider = SequencedChatProvider(
        {
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": None,
                        "tool_calls": [
                            {
                                "id": "call_readme",
                                "type": "function",
                                "function": {
                                    "name": "read_file",
                                    "arguments": '{"path":"README.md"}',
                                },
                            }
                        ],
                    }
                }
            ]
        },
        {"choices": [{"message": {"role": "assistant", "content": "done"}}]},
    )
    registry = ToolRegistry(tmp_path)
    tool_runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )
    protocol_runtime = CountingProtocolRuntime(registry=registry, trace=None)
    agent = make_agent_session(
        tmp_path,
        provider=provider,
        tools=registry,
        trace=TraceRuntime.create(tmp_path),
        console=NullConsole(),
        max_turns=2,
        tool_runtime=tool_runtime,
        protocol_runtime=protocol_runtime,
    )

    answer = agent.run("inspect")

    assert "Planner blocked finalization" in answer
    assert protocol_runtime.calls == 1
    assert protocol_runtime.tool_runtime_ids == [id(tool_runtime)]


def test_read_only_tools_still_execute_through_protocol_runtime(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("through production path", encoding="utf-8")
    request_context = ContextManager(system_prompt="system", user_goal="inspect")
    runtime = ModelRuntime.with_mock_provider(
        MockModelProvider(text=""),
        tool_registry=ToolRegistry(tmp_path),
    )
    request = runtime.build_request_from_context(
        request_context,
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        action_id="action_1",
        purpose=ModelPurpose.PLAN_NEXT_ACTION,
        allowed_tool_names=["read_file"],
    )
    result = ModelTurnResult(
        request_id=request.request_id,
        response_id="resp_1",
        status=ModelTurnStatus.SUCCESS,
        assistant_message=ModelMessage.assistant_text(""),
        tool_calls=[
            ModelToolCall(
                tool_call_id="call_read",
                tool_name="read_file",
                arguments={"path": "README.md"},
                raw_arguments='{"path":"README.md"}',
                parse_status=ModelToolParseStatus.VALID,
            )
        ],
    )
    protocol_runtime = ToolCallingProtocolRuntime(
        registry=ToolRegistry(tmp_path),
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "run" / "tool_protocol.sqlite3"),
    )
    tool_runtime = ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    turn = protocol_runtime.process_model_turn(
        request=request,
        result=result,
        turn=1,
        context=request_context,
        tool_runtime=tool_runtime,
    )

    assert turn.executed_count == 1
    assert "through production path" in request_context.messages()[-1]["content"]


def test_readme_documents_v010_production_architecture() -> None:
    readme = Path("README.md").read_text(encoding="utf-8")

    assert "# MiniHarness v0.1.0" in readme
    assert "production-grade local CLI coding agent harness" in readme
    assert "CLI\n-> MiniAgent\n-> PlannerRuntime\n-> ContextRuntime\n-> ModelRuntime" in readme
    assert "ToolCallingProtocolRuntime\n-> ToolRuntime\n-> PolicyRuntime / ApprovalGate" in readme
    assert "list_files" in readme
    assert "read_file" in readme
    assert "search_text" in readme
    assert "does not implement a Git Runtime" in readme
    assert "approval modes" in readme.lower()
    assert "<trace-run-dir>/context.sqlite3" in readme
    assert "<trace-run-dir>/tool_protocol.sqlite3" in readme
    assert "--strict" in readme
    assert "--dry-run" in readme
    assert "--resume" in readme
    assert "raw tool args" in readme
    assert "raw tool results" in readme
    assert "real sandbox isolation" in readme


class CountingProtocolRuntime(ToolCallingProtocolRuntime):
    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.calls = 0
        self.tool_runtime_ids: list[int] = []

    def process_model_turn(self, **kwargs: Any) -> Any:
        self.calls += 1
        self.tool_runtime_ids.append(id(kwargs["tool_runtime"]))
        return super().process_model_turn(**kwargs)


class NullConsole:
    def print(self, *_args: Any, **_kwargs: Any) -> None:
        return None


class SequencedChatProvider:
    def __init__(self, *responses: dict[str, Any]) -> None:
        self.responses = list(responses)

    def chat(
        self,
        *,
        messages: list[dict[str, Any]],
        tools: list[dict[str, Any]],
    ) -> dict[str, Any]:
        _ = messages, tools
        return self.responses.pop(0)


def _protocol_call(tool_call_id: str, *, raw_arguments: str) -> ToolCallEnvelope:
    arguments = json.loads(raw_arguments)
    digest = ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="resp_1",
        tool_call_id=tool_call_id,
        tool_name="read_file",
        raw_arguments=raw_arguments,
        parsed_arguments=arguments,
        normalized_arguments=arguments,
    ).argument_digest
    return ToolCallEnvelope(
        protocol_version="1.0",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="phase_1",
        model_request_id="req_1",
        model_response_id="resp_1",
        assistant_message_id="resp_1",
        tool_call_id=tool_call_id,
        tool_name="read_file",
        raw_arguments=raw_arguments,
        parsed_arguments=arguments,
        normalized_arguments=arguments,
        argument_digest=digest,
    )
