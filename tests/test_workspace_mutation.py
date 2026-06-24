import json
from pathlib import Path

import pytest

from singularity.context import ContextManager
from singularity.tools import PermissionLevel, ToolPolicy, ToolRegistry, ToolExecutor, ToolSpec
from singularity.policy import DecisionOutcome, OperationKind
from singularity.tools.models import ToolExecutionBackendKind, ToolResult
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.workspace import (
    CreateFile,
    MutationError,
    WorkspaceMutationManager,
    ReplaceText,
    RollbackManager,
    WorkspacePathResolver,
    WorkspacePolicy,
)
from tests.tool_executor_helpers import default_policy_engine
from singularity.tools.mutation import register_mutation_tools
from tests.test_tool_executor_policy_approval import SequencedPolicyEngine
from singularity.workspace_state import WorkspaceStateManager, WorkspaceHealthStatus


def tool_call(name: str, arguments: dict, *, tool_call_id: str = "call_1") -> dict:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_path_traversal_is_rejected(tmp_path: Path) -> None:
    resolver = WorkspacePathResolver(tmp_path)

    with pytest.raises(MutationError) as exc:
        resolver.resolve("../outside.txt")

    assert exc.value.code == "path_outside_workspace"


def test_symlink_escape_is_rejected(tmp_path: Path) -> None:
    outside = tmp_path.parent / "outside"
    outside.mkdir(exist_ok=True)
    link = tmp_path / "linked"
    try:
        link.symlink_to(outside, target_is_directory=True)
    except OSError:
        pytest.skip("symlink creation is not available in this environment")
    resolver = WorkspacePathResolver(tmp_path)

    with pytest.raises(MutationError) as exc:
        resolver.resolve("linked/file.txt")

    assert exc.value.code == "symlink_escape"


def test_policy_denies_secret_and_git_internal_paths(tmp_path: Path) -> None:
    component = WorkspaceMutationManager(tmp_path)

    env_result = component.apply_operations(
        [CreateFile(path=".env", content="TOKEN=value\n")],
        intent="create secret",
        created_by="test",
    )
    git_result = component.apply_operations(
        [CreateFile(path=".git/config", content="[core]\n")],
        intent="touch git",
        created_by="test",
    )

    assert env_result.ok is False
    assert env_result.error_code == "file_class_denied"
    assert git_result.ok is False
    assert git_result.error_code in {"path_denied", "file_class_denied"}
    assert not (tmp_path / ".env").exists()


def test_policy_denies_singularity_state_dir(tmp_path: Path) -> None:
    component = WorkspaceMutationManager(tmp_path)

    result = component.apply_operations(
        [
            CreateFile(
                path=".singularity/sessions/x/tamper.json",
                content='{"bad": true}\n',
            )
        ],
        intent="tamper with state",
        created_by="test",
    )

    assert result.ok is False
    assert result.error_code == "path_denied"
    assert not (tmp_path / ".singularity" / "sessions" / "x" / "tamper.json").exists()


def test_replace_text_generates_changeset_diff_apply_and_trace(tmp_path: Path) -> None:
    source = tmp_path / "src" / "app.py"
    source.parent.mkdir()
    source.write_text("print('old')\n", encoding="utf-8")
    trace = JsonlTraceRecorder.create(tmp_path)
    component = WorkspaceMutationManager(tmp_path, trace=trace)

    result = component.apply_operations(
        [ReplaceText(path="src/app.py", old_text="old", new_text="new")],
        intent="rename output",
        created_by="test",
        tool_call_id="call_replace",
    )

    assert result.ok is True
    assert source.read_text(encoding="utf-8") == "print('new')\n"
    assert result.changeset_id
    assert result.transaction_id
    assert result.observation["mutation_status"] == "applied"
    assert result.observation["changed_files"] == ["src/app.py"]
    assert result.diffs[0].added_lines == 1
    assert result.diffs[0].removed_lines == 1

    events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    mutation_events = [event for event in events if event["event"] == "mutation"]
    assert mutation_events
    audit = mutation_events[-1]["data"]
    assert audit["transaction_id"] == result.transaction_id
    assert audit["changeset_id"] == result.changeset_id
    assert audit["tool_call_id"] == "call_replace"
    assert audit["path"] == "src/app.py"
    assert audit["operation_type"] == "ReplaceText"
    assert audit["before_sha256"]
    assert audit["after_sha256"]
    assert audit["diff_digest"]
    assert audit["applied"] is True


def test_snapshot_hash_mismatch_rejects_write(tmp_path: Path) -> None:
    source = tmp_path / "src" / "app.py"
    source.parent.mkdir()
    source.write_text("alpha\n", encoding="utf-8")
    component = WorkspaceMutationManager(tmp_path)
    snapshot = component.index.snapshot_file("src/app.py")
    source.write_text("changed by user\n", encoding="utf-8")

    result = component.apply_operations(
        [
            ReplaceText(
                path="src/app.py",
                old_text="alpha",
                new_text="beta",
                expected_sha256=snapshot.sha256,
            )
        ],
        intent="update stale file",
        created_by="test",
    )

    assert result.ok is False
    assert result.error_code in {"snapshot_mismatch", "file_changed"}
    assert source.read_text(encoding="utf-8") == "changed by user\n"


def test_transaction_rolls_back_already_written_files_on_later_failure(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    first = tmp_path / "a.txt"
    second = tmp_path / "b.txt"
    first.write_text("one\n", encoding="utf-8")
    second.write_text("two\n", encoding="utf-8")
    component = WorkspaceMutationManager(tmp_path)
    calls = 0
    original_atomic_write = component.atomic_writer.write_text

    def fail_second_write(path: Path, text: str, *, snapshot):
        nonlocal calls
        calls += 1
        if calls == 2:
            raise MutationError("atomic_write_failed", "simulated write failure")
        return original_atomic_write(path, text, snapshot=snapshot)

    monkeypatch.setattr(component.atomic_writer, "write_text", fail_second_write)

    result = component.apply_operations(
        [
            ReplaceText(path="a.txt", old_text="one", new_text="ONE"),
            ReplaceText(path="b.txt", old_text="two", new_text="TWO"),
        ],
        intent="update two files",
        created_by="test",
    )

    assert result.ok is False
    assert result.error_code == "transaction_failed"
    assert first.read_text(encoding="utf-8") == "one\n"
    assert second.read_text(encoding="utf-8") == "two\n"


def test_rollback_conflict_when_user_changes_file_after_transaction(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    component = WorkspaceMutationManager(tmp_path)
    result = component.apply_operations(
        [ReplaceText(path="app.py", old_text="old", new_text="new")],
        intent="update app",
        created_by="test",
    )
    assert result.ok is True
    source.write_text("user edit\n", encoding="utf-8")

    rollback = RollbackManager(component).rollback(result.transaction_id)

    assert rollback.ok is False
    assert rollback.error_code == "rollback_conflict"
    assert source.read_text(encoding="utf-8") == "user edit\n"


def test_large_diff_is_truncated_and_saved_as_artifact(tmp_path: Path) -> None:
    source = tmp_path / "large.txt"
    source.write_text("".join(f"old {index}\n" for index in range(80)), encoding="utf-8")
    component = WorkspaceMutationManager(tmp_path, diff_context_lines=1, max_inline_diff_lines=20)

    result = component.apply_operations(
        [
            ReplaceText(
                path="large.txt",
                old_text="".join(f"old {index}\n" for index in range(80)),
                new_text="".join(f"new {index}\n" for index in range(80)),
            )
        ],
        intent="large rewrite",
        created_by="test",
    )

    assert result.ok is True
    diff = result.diffs[0]
    assert diff.truncated is True
    assert diff.artifact_path is not None
    assert (tmp_path / diff.artifact_path).exists()
    assert diff.digest


def test_policy_require_review_is_expressed_for_project_config(tmp_path: Path) -> None:
    pyproject = tmp_path / "pyproject.toml"
    pyproject.write_text("[project]\nname = 'x'\n", encoding="utf-8")
    policy = WorkspacePolicy()
    component = WorkspaceMutationManager(tmp_path, policy=policy)

    preview = component.preview_operations(
        [ReplaceText(path="pyproject.toml", old_text="x", new_text="y")],
        intent="change package metadata",
        created_by="test",
    )

    assert preview.ok is False
    assert preview.error_code == "review_required"
    assert preview.policy_decisions[0].decision == "require_review"
    assert pyproject.read_text(encoding="utf-8") == "[project]\nname = 'x'\n"


def test_mutation_observation_can_be_added_to_context_manager(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    component = WorkspaceMutationManager(tmp_path)
    result = component.apply_operations(
        [ReplaceText(path="app.py", old_text="old", new_text="new")],
        intent="update app",
        created_by="test",
    )
    context = ContextManager(system_prompt="system", user_goal="edit file")

    observation = context.add_tool_result(
        tool_call=tool_call("workspace_replace_text", {"path": "app.py"}),
        result=ToolResult.success(content=result.observation).model_dump(mode="json"),
    )

    assert observation.ok is True
    assert observation.raw_result["content"]["mutation_status"] == "applied"
    assert "new\n" not in observation.preview
    assert "app.py" in observation.preview


def test_tool_executor_rejects_write_tool_that_does_not_use_mutation_manager(
    tmp_path: Path,
) -> None:
    from pydantic import BaseModel
    import pytest

    class RawWriteInput(BaseModel):
        path: str
        content: str

    called = False

    def raw_write(args: RawWriteInput) -> dict:
        nonlocal called
        called = True
        (tmp_path / args.path).write_text(args.content, encoding="utf-8")
        return {"status": "wrote"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    with pytest.raises(ValueError, match="mutation manager"):
        registry.register(
            ToolSpec(
                name="raw_write",
                description="Unsafe write.",
                input_model=RawWriteInput,
                handler=raw_write,
                permission_level=PermissionLevel.WRITE,
                risk_tags=("write",),
            )
        )
    assert called is False
    assert not (tmp_path / "unsafe.txt").exists()


def test_registered_mutation_tool_applies_through_tool_executor(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_mutation_tools(registry, WorkspaceMutationManager(tmp_path))
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy(
            allowed_permissions=frozenset({PermissionLevel.READ_ONLY, PermissionLevel.WRITE}),
            denied_risk_tags=frozenset(),
        ),
        trace=None,
        workspace_root=tmp_path,
        policy_engine=default_policy_engine(tmp_path),
    )

    result = component.execute_tool_call(
        tool_call(
            "workspace_replace_text",
            {
                "path": "app.py",
                "old_text": "old",
                "new_text": "new",
                "intent": "update app",
            },
        )
    )

    assert result.ok is True
    assert source.read_text(encoding="utf-8") == "new\n"
    assert result.content["mutation_status"] == "applied"


def test_workspace_create_file_tool_keeps_policy_mutation_and_state_path(tmp_path: Path) -> None:
    trace = JsonlTraceRecorder.create(tmp_path)
    state = WorkspaceStateManager(tmp_path, trace=trace)
    state.begin_session(task_id="task_1", session_id="session_1")
    policy = SequencedPolicyEngine([DecisionOutcome.ALLOW, DecisionOutcome.ALLOW])
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_mutation_tools(
        registry,
        WorkspaceMutationManager(
            tmp_path,
            trace=trace,
            workspace_state_manager=state,
            policy_engine=policy,  # type: ignore[arg-type]
        ),
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy(
            allowed_permissions=frozenset({PermissionLevel.READ_ONLY, PermissionLevel.WRITE}),
            denied_risk_tags=frozenset(),
        ),
        trace=trace,
        workspace_root=tmp_path,
        policy_engine=policy,  # type: ignore[arg-type]
    )

    result = component.execute_tool_call(
        tool_call(
            "workspace_create_file",
            {
                "path": "created.txt",
                "content": "hello\n",
                "intent": "phase 1a regression",
            },
        )
    )

    assert result.ok is True
    assert (tmp_path / "created.txt").read_text(encoding="utf-8") == "hello\n"
    assert result.metadata["backend"] == ToolExecutionBackendKind.DELEGATED_MUTATION_MANAGER.value
    assert any(request.operation == OperationKind.CREATE_FILE for request in policy.requests)
    health = state.get_workspace_health()
    assert health.status == WorkspaceHealthStatus.DIRTY
    assert health.agent_changes == ["created.txt"]
