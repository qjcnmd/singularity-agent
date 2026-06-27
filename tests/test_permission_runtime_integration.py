from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest
from pydantic import BaseModel, ConfigDict

from singularity.command import CommandExecutor, CommandRequest
from singularity.config import ProductionConfig
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.kernel.graph import AgentGraphBuilder
from singularity.kernel.models import RunIdentity
from singularity.observability import TraceRecorder
from singularity.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PermissionProfile,
    PermissionProfileName,
    PolicyConfig,
    PolicyDecision,
    PolicyEngine,
    PolicyRequest,
)
from singularity.sandbox import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxManager,
    SandboxProfileName,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
    default_sandbox_profile,
)
from singularity.tools import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutor,
    ToolPolicy,
    ToolRegistry,
    ToolSpec,
)
from singularity.workspace import CreateFile, WorkspaceMutationManager


class _EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


class _ReviewPolicyEngine:
    def __init__(self, workspace: Path) -> None:
        self.config = PolicyConfig(
            workspace_root=workspace,
            permission_profile=PermissionProfile.default_for_workspace(
                workspace, profile=PermissionProfileName.READ_ONLY
            ),
            audit_log_path=workspace / "policy-audit.jsonl",
        )

    def enforce(self, request: PolicyRequest) -> PolicyDecision:
        return PolicyDecision(
            request_id=request.request_id,
            outcome=DecisionOutcome.REQUIRE_REVIEW,
            reason="review required by test",
        )


class _NoEarlyApprovalGate:
    def __getattr__(self, name: str) -> Any:
        if name in {
            "authorize",
            "consume_matching_grant",
            "consume_grant",
            "resolve",
            "register_grant",
        }:
            raise AssertionError(f"ToolExecutor consumed delegated approval via {name}")
        raise AttributeError(name)


class _AuthorizingGate:
    def __init__(self) -> None:
        self.requests: list[PolicyRequest] = []

    def authorize(
        self, request: PolicyRequest, _decision: PolicyDecision
    ) -> SimpleNamespace:
        self.requests.append(request)
        return SimpleNamespace(grant_id=f"grant_{len(self.requests)}")


def _tool_call(name: str) -> dict[str, Any]:
    return {
        "id": "call_delegated",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps({})},
    }


def test_kernel_graph_shares_one_permission_profile_across_five_boundaries(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("SINGULARITY_API_KEY", "test")
    monkeypatch.setenv("SINGULARITY_BASE_URL", "http://localhost/v1")
    monkeypatch.setenv("SINGULARITY_MODEL", "test-model")
    config = ProductionConfig.from_cli(
        project_root=tmp_path,
        dry_run=True,
        permission_profile="workspace-write",
        approval_policy="never",
    )
    trace = TraceRecorder.create(tmp_path, trace_dir=tmp_path / "traces")
    graph = AgentGraphBuilder().build(
        project_root=tmp_path,
        config=config,
        trace=trace,
        identity=RunIdentity.new(
            run_id=trace.run_id,
            session_id=trace.session_id,
            task_id=trace.run_id,
        ),
        user_goal="verify permission wiring",
    )

    profile = graph.policy_engine.config.permission_profile
    assert profile is not None
    assert graph.approval_gate.config.permission_profile is profile
    assert graph.command_executor.permission_profile is profile
    assert graph.mutation_manager.permission_profile is profile
    assert graph.sandbox_manager.permission_profile is profile


@pytest.mark.parametrize(
    ("backend_kind", "permission_level", "capability", "operation"),
    [
        (
            ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR,
            PermissionLevel.SHELL,
            Capability.EXECUTE_COMMAND,
            OperationKind.EXECUTE_COMMAND,
        ),
        (
            ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER,
            PermissionLevel.WRITE,
            Capability.MUTATE_WORKSPACE,
            OperationKind.MUTATE_FILE,
        ),
    ],
)
def test_tool_executor_does_not_consume_approval_for_delegated_boundaries(
    tmp_path: Path,
    backend_kind: ToolExecutionBackendKind,
    permission_level: PermissionLevel,
    capability: Capability,
    operation: OperationKind,
) -> None:
    calls: list[str] = []
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="delegated_action",
            description="delegated test action",
            input_model=_EmptyInput,
            handler=lambda _args: calls.append("handler") or {"ok": True},
            permission_level=permission_level,
            capabilities=(capability,),
            operation=operation,
            execution_backend=backend_kind,
            uses_command_executor=(
                backend_kind
                == ToolExecutionBackendKind.DELEGATED_COMMAND_EXECUTOR
            ),
            uses_mutation_manager=(
                backend_kind
                == ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER
            ),
        )
    )
    executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.coding_agent(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=_ReviewPolicyEngine(tmp_path),  # type: ignore[arg-type]
        approval_gate=_NoEarlyApprovalGate(),  # type: ignore[arg-type]
    )

    result = executor.execute_tool_call(_tool_call("delegated_action"))

    assert result.ok is True
    assert calls == ["handler"]


def test_direct_command_executor_uses_approval_gate(tmp_path: Path) -> None:
    profile = PermissionProfile.default_for_workspace(
        tmp_path, profile=PermissionProfileName.READ_ONLY
    )
    policy_engine = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=profile,
            audit_log_path=tmp_path / "policy-audit.jsonl",
        )
    )
    gate = _AuthorizingGate()
    executor = CommandExecutor(
        tmp_path,
        policy_engine=policy_engine,
        approval_gate=gate,  # type: ignore[arg-type]
        sandbox_manager=SandboxManager(
            tmp_path, backends=[], permission_profile=profile
        ),
    )

    executor.run(CommandRequest(argv=[sys.executable, "-c", "print('ok')"], cwd="."))

    assert len(gate.requests) == 1
    assert gate.requests[0].component.value == "command"


def test_direct_mutation_manager_uses_approval_gate(tmp_path: Path) -> None:
    profile = PermissionProfile.default_for_workspace(
        tmp_path, profile=PermissionProfileName.READ_ONLY
    )
    policy_engine = PolicyEngine(
        PolicyConfig(
            workspace_root=tmp_path,
            permission_profile=profile,
            audit_log_path=tmp_path / "policy-audit.jsonl",
        )
    )
    gate = _AuthorizingGate()
    manager = WorkspaceMutationManager(
        tmp_path,
        policy_engine=policy_engine,
        approval_gate=gate,  # type: ignore[arg-type]
    )

    result = manager.apply_operations(
        [CreateFile(path="approved.txt", content="approved\n")],
        intent="test direct approval",
        created_by="test",
    )

    assert result.ok is True
    assert (tmp_path / "approved.txt").read_text(encoding="utf-8") == "approved\n"
    assert len(gate.requests) == 1
    assert gate.requests[0].component.value == "mutation"


def test_additional_writable_directory_mutation_is_applied_without_full_access(
    tmp_path: Path,
) -> None:
    extra = tmp_path.parent / f"{tmp_path.name}-extra"
    extra.mkdir()
    profile = PermissionProfile.default_for_workspace(
        tmp_path,
        profile=PermissionProfileName.WORKSPACE_WRITE,
        additional_writable_directories=(extra,),
    )
    manager = WorkspaceMutationManager(
        tmp_path,
        policy_engine=PolicyEngine(
            PolicyConfig(
                workspace_root=tmp_path,
                permission_profile=profile,
                audit_log_path=tmp_path / "policy-audit.jsonl",
            )
        ),
    )
    target = extra / "artifact.txt"

    result = manager.apply_operations(
        [CreateFile(path=str(target), content="artifact\n")],
        intent="write authorized extra directory",
        created_by="test",
    )

    assert result.ok is True
    assert target.read_text(encoding="utf-8") == "artifact\n"


def _capabilities() -> SandboxCapabilities:
    return SandboxCapabilities(
        filesystem_isolation=True,
        copy_on_write=True,
        readonly_mount=True,
        network_isolation=True,
        env_isolation=True,
        process_tree_kill=True,
        timeout=True,
        output_limit=True,
        memory_limit=True,
        process_limit=True,
        artifact_capture=True,
        change_detection=True,
    )


@dataclass
class _CountingBackend:
    root: Path
    available: bool
    prepare_calls: int = 0
    run_calls: int = 0

    def name(self) -> str:
        return "counting_native"

    def capabilities(self) -> SandboxCapabilities:
        return _capabilities()

    def is_available(self) -> bool:
        return self.available

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        self.prepare_calls += 1
        return PreparedSandbox(
            sandbox_id=request.sandbox_id,
            backend_name=self.name(),
            sandbox_root=self.root / "sandbox",
            workspace_copy_root=self.root / "sandbox" / "workspace",
            execution_cwd=self.root / "sandbox" / "workspace",
            env={},
            request=request,
            created_at=datetime.now(UTC).isoformat(),
            trace_id="trace",
        )

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        self.run_calls += 1
        now = datetime.now(UTC).isoformat()
        return SandboxResult(
            sandbox_id=prepared.sandbox_id,
            backend_name=self.name(),
            status=SandboxStatus.SUCCESS,
            exit_code=0,
            stdout="",
            stderr="",
            started_at=now,
            ended_at=now,
            duration_ms=0,
        )

    def cleanup(self, _prepared: PreparedSandbox) -> None:
        return None


def _sandbox_request(tmp_path: Path, command: list[str]) -> SandboxRequest:
    return SandboxRequest(
        sandbox_id="permission-runtime",
        session_id="session",
        task_id="task",
        action_id="action",
        command=command,
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=default_sandbox_profile(
            SandboxProfileName.ISOLATED_VERIFICATION, workspace_root=tmp_path
        ),
    )


def test_sandbox_protected_path_preflight_starts_no_backend_process(
    tmp_path: Path,
) -> None:
    backend = _CountingBackend(tmp_path, available=True)
    manager = SandboxManager(tmp_path, backends=[backend])

    result = manager.run(_sandbox_request(tmp_path, ["python", ".env"]))

    assert result.status == SandboxStatus.POLICY_BLOCKED
    assert result.metadata["error_code"] == "protected_path_denied"
    assert backend.prepare_calls == 0
    assert backend.run_calls == 0


def test_sandbox_backend_unavailable_starts_no_backend_process(tmp_path: Path) -> None:
    backend = _CountingBackend(tmp_path, available=False)
    manager = SandboxManager(tmp_path, backends=[backend])

    result = manager.run(
        _sandbox_request(tmp_path, ["python", "-c", "print('ok')"])
    )

    assert result.status == SandboxStatus.BACKEND_UNAVAILABLE
    assert result.metadata["error_code"] == "backend_unavailable"
    assert backend.prepare_calls == 0
    assert backend.run_calls == 0
