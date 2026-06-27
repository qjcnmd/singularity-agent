from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from typer.testing import CliRunner
from typer.main import get_command

from singularity.agent_loop import AgentLoopStatus
from singularity.cli import app
from singularity.config import ProductionConfig, adaptive_default_max_turns
from singularity.context import ContextManager
from singularity.context.models import ContextItemType, ContextSource
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.model import (
    ModelMessage,
    ModelPurpose,
    ModelRunner,
    ModelTurnResult,
    ModelTurnStatus,
    ModelToolCall,
    ModelToolParseStatus,
    MockModelProvider,
)
from singularity.observability import TraceRecorder
from singularity.observability.artifacts import TraceArtifactStore
from singularity.observability.models import TraceArtifactKind
from singularity.policy import DecisionOutcome
from singularity.tool_protocol.models import (
    ToolCallEnvelope,
    ToolCallPhase,
    ToolProtocolResultEnvelope,
    ToolProtocolTurnStatus,
)
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tools import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolPolicy,
    ToolRegistry,
    ToolExecutor,
    ToolSideEffectKind,
    ToolSpec,
)
from tests.agent_loop_helpers import make_agent_session
from tests.test_tool_executor_policy_approval import (
    EmptyInput,
    SequencedPolicyEngine,
    make_tool_call,
)
from tests.tool_executor_helpers import make_test_policy_engine


def test_cli_help_exposes_production_baseline_options_without_legacy_copy() -> None:
    result = CliRunner().invoke(app, ["main", "--help"], terminal_width=400)

    assert result.exit_code == 0
    output = result.output
    assert "production-oriented local CLI coding agent harness" in output
    assert "minimal" not in output.lower()
    assert "read-only agent loop" not in output.lower()
    command = get_command(app).commands["main"]
    option_names = {
        option
        for parameter in command.params
        for option in (
            *getattr(parameter, "opts", ()),
            *getattr(parameter, "secondary_opts", ()),
        )
    }
    for option in [
        "--max-turns",
        "--profile",
        "--permission-profile",
        "--approval-policy",
        "--network-access",
        "--add-dir",
        "--windows-sandbox",
        "--trace-dir",
        "--context-db",
        "--model",
        "--base-url",
        "--raw-artifacts",
        "--no-raw-artifacts",
        "--resume",
        "--dry-run",
        "--strict",
    ]:
        assert option in option_names
    assert "--approval-mode" not in option_names
    assert "--security-mode" not in option_names


def test_production_config_maps_cli_policy_and_model_overrides(tmp_path: Path) -> None:
    extra = tmp_path / "shared"
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        max_turns=3,
        profile="local-dev",
        permission_profile="read-only",
        approval_policy="never",
        network_access="allowed",
        additional_writable_directories=[extra],
        windows_sandbox="elevated",
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
    assert config.profile == "local-dev"
    assert config.permission_profile.value == "read-only"
    assert config.approval_policy.value == "never"
    assert config.network_access.value == "allowed"
    assert config.additional_writable_directories == (extra.resolve(),)
    assert config.windows_sandbox == "elevated"
    assert config.strict is True
    assert config.dry_run is True
    assert config.trace_dir == tmp_path / "traces"
    assert config.context_db == tmp_path / "context.sqlite3"
    assert config.model == "override-model"
    assert config.base_url == "https://example.test/v1"
    assert config.raw_artifacts is False
    assert config.resume_session == "session_1"
    permission_profile = config.to_permission_profile()
    assert permission_profile.profile.value == "read-only"
    assert permission_profile.approval_policy.value == "never"
    assert permission_profile.network_access.value == "allowed"
    assert permission_profile.additional_writable_directories == (extra.resolve(),)
    model_config = config.to_model_runner_config()
    assert model_config.default_model == "override-model"
    assert model_config.providers["openai_compatible"]["base_url"] == "https://example.test/v1"
    assert model_config.store_raw_responses is False


def test_production_config_merges_cli_env_config_and_defaults(
    tmp_path: Path,
    monkeypatch,
) -> None:
    config_dir = tmp_path / ".singularity"
    config_dir.mkdir()
    (config_dir / "config.toml").write_text(
        """
max_turns = 4
model = "config-model"
base_url = "https://config.example/v1"
raw_artifacts = true

[permissions]
profile = "read-only"
approval_policy = "never"
network_access = "allowed"
additional_writable_directories = ["../shared"]
protected_paths = ["secrets/**"]

[permissions.windows]
implementation = "elevated"

[project_index]
enabled = false
build_on_boot = false
max_files = 123
""".lstrip(),
        encoding="utf-8",
    )
    monkeypatch.delenv("SINGULARITY_BASE_URL", raising=False)
    monkeypatch.setenv("SINGULARITY_MAX_TURNS", "6")
    monkeypatch.setenv("SINGULARITY_MODEL", "env-model")
    monkeypatch.setenv("SINGULARITY_PROJECT_INDEX_ENABLED", "true")

    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        permission_profile="danger-full-access",
        cli_overrides={"permission_profile"},
    )
    effective = config.effective_config()

    assert config.max_turns == 6
    assert config.permission_profile.value == "danger-full-access"
    assert config.approval_policy.value == "never"
    assert config.network_access.value == "allowed"
    assert config.additional_writable_directories == ((tmp_path / "../shared").resolve(),)
    assert config.windows_sandbox == "elevated"
    assert config.protected_paths == ("secrets/**",)
    assert any(
        rule.pattern == "secrets/**"
        for rule in config.to_permission_profile().protected_paths
    )
    assert config.model == "env-model"
    assert config.base_url == "https://config.example/v1"
    assert config.raw_artifacts is True
    assert config.project_index_enabled is True
    assert config.project_index_build_on_boot is False
    assert config.project_index_max_files == 123
    assert effective["sources"]["max_turns"] == "env:SINGULARITY_MAX_TURNS"
    assert effective["sources"]["permission_profile"] == "cli"
    assert effective["sources"]["approval_policy"] == "config:.singularity/config.toml"
    assert effective["sources"]["base_url"] == "config:.singularity/config.toml"
    assert effective["sources"]["dry_run"] == "default"
    assert "api_key" not in json.dumps(effective).lower()


def test_production_config_reports_custom_config_file_source(tmp_path: Path) -> None:
    config_file = tmp_path / "component.toml"
    config_file.write_text("max_turns = 9\n", encoding="utf-8")

    config = ProductionConfig.from_cli(project_root=tmp_path, config_file=config_file)
    effective = config.effective_config()

    assert config.max_turns == 9
    assert effective["config_file"] == "component.toml"
    assert effective["sources"]["max_turns"] == "config:component.toml"


def test_production_config_loads_project_env_without_leaking_api_key(monkeypatch, tmp_path: Path) -> None:
    monkeypatch.delenv("SINGULARITY_API_KEY", raising=False)
    monkeypatch.delenv("SINGULARITY_BASE_URL", raising=False)
    monkeypatch.delenv("SINGULARITY_MODEL", raising=False)
    (tmp_path / ".env").write_text(
        "\n".join(
            [
                "SINGULARITY_API_KEY=sk-local-test-secret",
                "SINGULARITY_BASE_URL=https://example.invalid/v1",
                "SINGULARITY_MODEL=env-file-model",
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    config = ProductionConfig.from_cli(project_root=tmp_path)
    effective = config.effective_config()

    assert config.model == "env-file-model"
    assert config.base_url == "https://example.invalid/v1"
    assert effective["sources"]["model"] == "env:SINGULARITY_MODEL"
    assert effective["sources"]["base_url"] == "env:SINGULARITY_BASE_URL"
    assert effective["sources"]["env_file"] == "project:.env"
    dumped = json.dumps(effective)
    assert "sk-local-test-secret" not in dumped
    assert "api_key" not in dumped.lower()


def test_production_config_loads_env_from_explicit_root_for_fixture_workspace(
    monkeypatch,
    tmp_path: Path,
) -> None:
    monkeypatch.delenv("SINGULARITY_API_KEY", raising=False)
    monkeypatch.delenv("SINGULARITY_BASE_URL", raising=False)
    monkeypatch.delenv("SINGULARITY_MODEL", raising=False)
    repo_root = tmp_path / "repo"
    workspace = tmp_path / "eval-workspace"
    repo_root.mkdir()
    workspace.mkdir()
    (repo_root / ".env").write_text(
        "\n".join(
            [
                "SINGULARITY_API_KEY=sk-local-test-secret",
                "SINGULARITY_BASE_URL=https://example.invalid/v1",
                "SINGULARITY_MODEL=env-root-model",
            ]
        )
        + "\n",
        encoding="utf-8",
    )

    config = ProductionConfig.from_cli(project_root=workspace, env_root=repo_root)
    effective = config.effective_config()

    assert config.model == "env-root-model"
    assert config.base_url == "https://example.invalid/v1"
    assert effective["sources"]["env_file"].replace("\\", "/").endswith("/repo/.env")
    dumped = json.dumps(effective)
    assert "sk-local-test-secret" not in dumped
    assert "api_key" not in dumped.lower()


def test_adaptive_default_turn_budget_scales_long_tasks(tmp_path: Path) -> None:
    assert adaptive_default_max_turns("inspect README") == 8
    assert (
        adaptive_default_max_turns(
            "Implement the integration fix, run tests, update the report, and commit the result."
        )
        == 12
    )
    assert (
        adaptive_default_max_turns(
            "根据清单按阶段完成实现、测试、报告、提交、push、合并，并在每个阶段验证结果。"
            "This is an end-to-end roadmap task with benchmark, architecture, integration, and report work."
        )
        == 16
    )

    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        default_max_turns=adaptive_default_max_turns(
            "根据清单按阶段完成实现、测试、报告、提交、push、合并，并在每个阶段验证结果。"
        ),
    )

    assert config.max_turns == 16
    assert config.effective_config()["sources"]["max_turns"] == "default:adaptive"


def test_tool_policy_is_not_policy_engine_permission_decider(tmp_path: Path) -> None:
    calls: list[str] = []
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="write_via_policy_engine",
            description="write",
            input_model=EmptyInput,
            handler=lambda _args: calls.append("called") or {"ok": True},
            permission_level=PermissionLevel.WRITE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER,
            uses_mutation_manager=True,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=SequencedPolicyEngine([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
    )

    result = component.execute_tool_call(make_tool_call("write_via_policy_engine"))

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
            execution_backend=ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER,
            uses_mutation_manager=True,
            side_effects=ToolSideEffectKind.MUTATE_WORKSPACE,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=SequencedPolicyEngine([DecisionOutcome.ALLOW]),  # type: ignore[arg-type]
        dry_run=True,
    )

    result = component.execute_tool_call(make_tool_call("mutate_for_real"))

    assert result.ok is False
    assert result.error_code == "dry_run_blocked"
    assert calls == []


def test_protocol_default_state_store_lives_in_trace_run_dir(tmp_path: Path) -> None:
    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "trace-root")
    component = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=trace,
    )

    assert component.state_store.db_path == trace.store.run_dir / "tool_protocol.sqlite3"


def test_protocol_default_state_store_for_legacy_trace_uses_run_directory(tmp_path: Path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)
    component = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=trace,
    )

    assert component.state_store.db_path == trace.path.parent / trace.run_id / "tool_protocol.sqlite3"


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
    component = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=None,
        state_store=store,
    )

    recovered = component.recover_pending(run_id="run_1")

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
    assert item.source_component == ContextSource.TOOL_PROTOCOL


def test_workspace_state_uses_dedicated_context_item_not_tool_result() -> None:
    context = ContextManager(system_prompt="system", user_goal="goal")
    item = context.add_workspace_state({"status": "clean", "secret": "sk-test-value"})

    assert item.item_type == ContextItemType.WORKSPACE_STATE
    assert item.source_component == ContextSource.WORKSPACE_STATE
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


def test_min_agent_uses_injected_protocol_and_tool_executor(tmp_path: Path) -> None:
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
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    tool_protocol = CountingProtocolEngine(registry=registry, trace=None)
    agent = make_agent_session(
        tmp_path,
        provider=provider,
        tools=registry,
        trace=TraceRecorder.create(tmp_path),
        console=NullConsole(),
        max_turns=2,
        tool_executor=tool_executor,
        tool_protocol=tool_protocol,
    )

    result = agent.run("inspect")

    assert result.status == AgentLoopStatus.MAX_TURNS_EXCEEDED
    assert tool_protocol.calls == 1
    assert tool_protocol.tool_executor_ids == [id(tool_executor)]


def test_read_only_tools_still_execute_through_tool_protocol(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("through production path", encoding="utf-8")
    request_context = ContextManager(system_prompt="system", user_goal="inspect")
    component = ModelRunner.with_mock_provider(
        MockModelProvider(text=""),
        tool_registry=ToolRegistry(tmp_path),
    )
    request = component.build_request_from_context(
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
    tool_protocol = ToolProtocolEngine(
        registry=ToolRegistry(tmp_path),
        trace=None,
        state_store=ToolProtocolStateStore(tmp_path / "run" / "tool_protocol.sqlite3"),
    )
    tool_executor = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    turn = tool_protocol.process_model_turn(
        request=request,
        result=result,
        turn=1,
        context=request_context,
        tool_executor=tool_executor,
    )

    assert turn.executed_count == 1
    assert "through production path" in request_context.messages()[-1]["content"]


def test_readme_documents_v010_production_architecture() -> None:
    readme = Path("README.md").read_text(encoding="utf-8")

    assert "# Singularity v0.1.0" in readme
    assert "## 当前身份" in readme
    assert "本地优先的 CLI coding agent harness" in readme
    assert "docs/architecture/modules/" in readme
    assert (
        "CLI\n"
        "-> KernelBootstrap.boot()\n"
        "-> AgentGraphBuilder.build()\n"
        "-> AgentKernel.run_task()\n"
        "-> AgentLoop.run()\n"
        "-> RunController.start()\n"
        "-> Planner.step()\n"
        "-> ModelRunner.build_request_from_context()\n"
        "-> ModelTurnRequestBuilder.build_request()\n"
        "-> PromptAssemblyPipeline.build_for_model_turn()\n"
        "-> ContextManager.build_bundle()\n"
        "-> ModelRunner.run_turn()"
    ) in readme
    assert (
        "ToolProtocolEngine.process_model_turn()\n"
        "-> ToolExecutor.execute_request()\n"
        "-> PolicyEngine.enforce() / ApprovalGate"
    ) in readme
    assert "PolicyEngine（策略引擎）" in readme
    assert "ApprovalGate（审批闸门）" in readme
    assert "EvaluationRunner（评估运行器）" in readme
    assert "singularity-agent eval run docs/evaluation/capability-regression-tasks.json --json" in readme
    assert "<trace-run-dir>/context.sqlite3" in readme
    assert "<trace-run-dir>/tool_protocol.sqlite3" in readme
    assert "--strict" in readme
    assert "--dry-run" in readme
    assert "agent_completed" in readme
    assert "evaluation_passed" in readme
    assert "miscompletion_count" in readme
    assert "旧阶段报告" in readme
    assert "docs/architecture/execution" + "-map.md" not in readme
    assert "docs/evaluation" + "-harness.md" not in readme


class CountingProtocolEngine(ToolProtocolEngine):
    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.calls = 0
        self.tool_executor_ids: list[int] = []

    def process_model_turn(self, **kwargs: Any) -> Any:
        self.calls += 1
        self.tool_executor_ids.append(id(kwargs["tool_executor"]))
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
