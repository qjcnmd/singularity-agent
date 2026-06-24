import json
import sqlite3
import sys
from pathlib import Path

import pytest

from singularity.command import (
    CommandPurpose,
    CommandRequest,
    CommandExecutor,
    FilesystemMode,
)
from singularity.context import ContextManager
from singularity.policy import ApprovalMode, PolicyConfig, PolicyEngine, SecurityMode
from singularity.tools.models import ToolResult
from singularity.tools.workspace_state import WorkspaceHealthInput, WorkspaceHealthToolHandlers
from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.workspace import WorkspaceMutationManager, ReplaceText
from singularity.workspace_state import (
    ChangeOwnership,
    WorkspaceStateManager,
    RecoveryStatus,
    WorkspaceHealthStatus,
)


class SimpleTokenCounter:
    def count_text(self, text: str) -> int:
        return len(text.split())

    def count_messages(self, messages: list[dict]) -> int:
        return sum(self.count_text(str(message.get("content") or "")) for message in messages)


def tool_call(name: str, arguments: dict, *, tool_call_id: str = "call_state") -> dict:
    return {
        "id": tool_call_id,
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def test_session_start_creates_persistent_baseline_and_skips_protected_dirs(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    (tmp_path / "src" / "app.py").write_text("print('hi')\n", encoding="utf-8")
    (tmp_path / ".git").mkdir()
    (tmp_path / ".git" / "HEAD").write_text("ref: main\n", encoding="utf-8")
    (tmp_path / "node_modules").mkdir()
    (tmp_path / "node_modules" / "pkg.js").write_text("ignored\n", encoding="utf-8")
    (tmp_path / "work").mkdir()
    (tmp_path / "work" / "pytest-output.txt").write_text("ignored\n", encoding="utf-8")

    component = WorkspaceStateManager(tmp_path)
    baseline = component.begin_session(task_id="task_1", session_id="session_1")

    assert baseline.workspace_root == str(tmp_path.resolve(strict=False))
    assert baseline.session_id == "session_1"
    assert "src/app.py" in baseline.snapshots
    assert ".git/HEAD" not in baseline.snapshots
    assert "node_modules/pkg.js" not in baseline.snapshots
    assert "work/pytest-output.txt" not in baseline.snapshots
    assert (tmp_path / ".singularity" / "sessions" / "session_1" / "journal.jsonl").exists()
    assert (tmp_path / ".singularity" / "workspace_state.sqlite3").exists()


def test_file_snapshot_records_hash_metadata_encoding_line_endings_symlink_and_class(
    tmp_path: Path,
) -> None:
    source = tmp_path / "README.md"
    source.write_text("hello\r\nworld\r\n", encoding="utf-8")
    link = tmp_path / "readme-link.md"
    try:
        link.symlink_to(source)
    except OSError:
        link = None

    component = WorkspaceStateManager(tmp_path)
    component.begin_session(task_id="task_1")
    snapshot = component.snapshot_file("README.md")

    assert snapshot.path == "README.md"
    assert snapshot.canonical_path == str(source.resolve(strict=False))
    assert snapshot.sha256
    assert snapshot.size == source.stat().st_size
    assert snapshot.mtime_ns == source.stat().st_mtime_ns
    assert snapshot.file_type == "file"
    assert snapshot.encoding == "utf-8"
    assert snapshot.line_ending == "crlf"
    assert snapshot.is_binary is False
    assert snapshot.file_class == "DOCUMENTATION"
    assert snapshot.permissions
    assert snapshot.captured_at

    if link is not None:
        link_snapshot = component.snapshot_file("readme-link.md")
        assert link_snapshot.is_symlink is True
        assert link_snapshot.symlink_target


def test_mutation_manager_records_agent_owned_changes_in_state_journal_and_trace(
    tmp_path: Path,
) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    trace = JsonlTraceRecorder.create(tmp_path)
    state = WorkspaceStateManager(tmp_path, trace=trace)
    state.begin_session(task_id="task_1", session_id="session_1")
    mutation = WorkspaceMutationManager(tmp_path, trace=trace, workspace_state_manager=state)

    result = mutation.apply_operations(
        [ReplaceText(path="app.py", old_text="old", new_text="new")],
        intent="update app",
        created_by="test",
        tool_call_id="call_mutation",
    )

    assert result.ok is True
    health = state.get_workspace_health()
    assert health.status == WorkspaceHealthStatus.DIRTY
    assert health.agent_changes == ["app.py"]

    events = [
        json.loads(line)
        for line in (tmp_path / ".singularity" / "sessions" / "session_1" / "journal.jsonl")
        .read_text(encoding="utf-8")
        .splitlines()
    ]
    assert any(event["event_type"] == "file_changed_by_mutation" for event in events)
    assert any(event["ownership"] == ChangeOwnership.AGENT_MUTATION.value for event in events)

    trace_events = [json.loads(line) for line in trace.path.read_text(encoding="utf-8").splitlines()]
    state_events = [event for event in trace_events if event["event"] == "workspace_state"]
    assert any(event["data"]["session_id"] == "session_1" for event in state_events)
    assert any(event["data"]["mutation_id"] for event in state_events)


def test_command_executor_records_side_effect_ownership_by_command_purpose(tmp_path: Path) -> None:
    state = WorkspaceStateManager(tmp_path)
    state.begin_session(task_id="task_1")
    component = CommandExecutor(
        tmp_path,
        workspace_state_manager=state,
        policy_engine=PolicyEngine(
            PolicyConfig(
                workspace_root=tmp_path,
                approval_mode=ApprovalMode.AUTO_SAFE,
                security_mode=SecurityMode.COMPAT,
            )
        ),
    )

    result = component.run(
        CommandRequest(
            argv=[
                sys.executable,
                "-c",
                "from pathlib import Path; Path('generated.txt').write_text('x', encoding='utf-8')",
            ],
            cwd=".",
            purpose=CommandPurpose.PACKAGE_MANAGER,
            filesystem_mode=FilesystemMode.READ_WRITE_WORKSPACE,
            risk_acceptance_reason="test accepts package-manager side effect",
        )
    )

    assert result.changed_files == ["generated.txt"]
    assert result.side_effects[0]["ownership"] == ChangeOwnership.PACKAGE_MANAGER_SIDE_EFFECT.value
    health = state.get_workspace_health()
    assert health.command_side_effects == ["generated.txt"]


def test_external_change_detection_and_snapshot_mismatch_blocks_mutation(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    state = WorkspaceStateManager(tmp_path)
    state.begin_session(task_id="task_1")
    snapshot = state.snapshot_file("app.py")
    source.write_text("user edit\n", encoding="utf-8")

    external = state.record_external_changes()

    assert external.external_changes == ["app.py"]
    assert state.get_workspace_health().status == WorkspaceHealthStatus.CONFLICTED

    mutation = WorkspaceMutationManager(tmp_path, workspace_state_manager=state)
    result = mutation.apply_operations(
        [
            ReplaceText(
                path="app.py",
                old_text="old",
                new_text="new",
                expected_sha256=snapshot.sha256,
            )
        ],
        intent="stale update",
        created_by="test",
    )

    assert result.ok is False
    assert result.error_code in {"snapshot_mismatch", "external_change_detected", "file_changed"}
    assert source.read_text(encoding="utf-8") == "user edit\n"


def test_agent_owned_rollback_skips_external_edits_and_reports_conflict(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    state = WorkspaceStateManager(tmp_path)
    state.begin_session(task_id="task_1")
    mutation = WorkspaceMutationManager(tmp_path, workspace_state_manager=state)
    result = mutation.apply_operations(
        [ReplaceText(path="app.py", old_text="old", new_text="new")],
        intent="update app",
        created_by="test",
    )
    assert result.ok is True
    source.write_text("user edit\n", encoding="utf-8")

    plan = state.prepare_rollback(transaction_id=result.transaction_id)
    rollback = state.apply_rollback(plan)

    assert rollback.ok is False
    assert rollback.error_code == "rollback_conflict"
    assert rollback.conflicts == ["app.py"]
    assert source.read_text(encoding="utf-8") == "user edit\n"


def test_artifact_store_journal_recovery_health_and_context_observation(tmp_path: Path) -> None:
    state = WorkspaceStateManager(tmp_path)
    state.begin_session(task_id="task_1", session_id="session_1")
    artifact = state.artifacts.save(
        kind="full_command_output",
        content="large output",
        linked_command_id="cmd_1",
    )
    state.close_session(status="interrupted")

    restarted = WorkspaceStateManager(tmp_path)
    recovery = restarted.recover_session("session_1")
    health = restarted.get_workspace_health()

    assert artifact.path
    assert (tmp_path / artifact.path).exists()
    assert recovery.status == RecoveryStatus.RECOVERABLE
    assert health.status in {WorkspaceHealthStatus.CLEAN, WorkspaceHealthStatus.DIRTY}

    context = ContextManager(
        system_prompt="system",
        user_goal="inspect state",
        token_counter=SimpleTokenCounter(),
    )
    observation = context.add_tool_result(
        tool_call=tool_call("workspace_health", {}),
        result=ToolResult.success(content=health.to_observation()).model_dump(mode="json"),
    )

    assert observation.ok is True
    assert "workspace_state" in observation.preview


def test_workspace_health_tool_refreshes_external_changes(tmp_path: Path) -> None:
    source = tmp_path / "app.py"
    source.write_text("old\n", encoding="utf-8")
    state = WorkspaceStateManager(tmp_path)
    state.begin_session(task_id="task_1")
    source.write_text("user edit\n", encoding="utf-8")
    handlers = WorkspaceHealthToolHandlers(state)

    result = handlers.workspace_health(WorkspaceHealthInput(refresh_external=True))

    workspace_state = result["workspace_state"]
    assert workspace_state["status"] == WorkspaceHealthStatus.CONFLICTED.value
    assert workspace_state["external_changes"] == ["app.py"]
    assert "journal" not in json.dumps(result, ensure_ascii=False).lower()


def test_workspace_workspace_state_manager_closes_sqlite_store(tmp_path: Path) -> None:
    state = WorkspaceStateManager(tmp_path)
    state.begin_session(task_id="task_1")

    state.close()

    with pytest.raises(sqlite3.ProgrammingError):
        state.store.connection.execute("select 1")
