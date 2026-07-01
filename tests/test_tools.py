import json
from pathlib import Path

from singularity.jsonl_trace import JsonlTraceRecorder
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.edit import EDIT_APPLY_TIMEOUT_SECONDS, register_edit_tools
from tests.tool_executor_helpers import make_test_policy_engine


def make_tool_call(name: str, arguments: dict) -> dict:
    return {"function": {"name": name, "arguments": json.dumps(arguments)}}


def make_raw_tool_call(name: str, arguments: str) -> dict:
    return {"function": {"name": name, "arguments": arguments}}


def test_openai_tools_schema_contains_read_only_tools(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)

    schemas = registry.openai_tools()

    names = {tool["function"]["name"] for tool in schemas}
    assert names == {"list_files", "read_file", "search_text"}
    for tool in schemas:
        assert tool["type"] == "function"
        assert "description" in tool["function"]
        assert "parameters" in tool["function"]


def test_read_file_schema_exposes_line_window_arguments(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)

    schemas = registry.openai_tools(strict=True)

    read_file_schema = next(
        tool for tool in schemas if tool["function"]["name"] == "read_file"
    )
    properties = read_file_schema["function"]["parameters"]["properties"]
    assert "line_start" in properties
    assert "line_count" in properties
    assert read_file_schema["function"]["parameters"]["additionalProperties"] is False


def test_read_only_tools_have_stable_runtime_timeout(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)

    for name in ("list_files", "read_file", "search_text"):
        assert registry.get(name).timeout_seconds >= 10.0


def test_edit_apply_budget_covers_review_pipeline(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    register_edit_tools(registry)

    edit_apply = registry.get("edit_apply")

    assert edit_apply is not None
    assert edit_apply.timeout_seconds == EDIT_APPLY_TIMEOUT_SECONDS
    assert edit_apply.timeout_seconds >= 60.0
    assert edit_apply.uses_edit_executor is True
    assert edit_apply.uses_mutation_manager is True


def test_builtin_read_file_uses_thread_handler_not_process_spawn(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("hello\n", encoding="utf-8")
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("read_file", {"path": "README.md"}),
        executor=tool_executor,
    )

    assert result["ok"] is True
    assert result["metadata"]["handler_isolation"] == "thread"


def test_openai_tools_strict_schema_marks_functions_strict(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)

    schemas = registry.openai_tools(strict=True)

    for tool in schemas:
        function = tool["function"]
        assert function["strict"] is True
        assert function["parameters"]["additionalProperties"] is False


def test_read_file_reads_project_file(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("hello from singularity", encoding="utf-8")
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 100}),
        executor=tool_executor,
    )

    assert result["ok"] is True
    assert result["content"]["path"] == "README.md"
    assert result["content"]["content"] == "hello from singularity"
    assert result["truncated"] is False


def test_read_file_reads_requested_line_window(tmp_path: Path) -> None:
    source = tmp_path / "module.py"
    source.write_text(
        "line 1\nline 2\nline 3 target\nline 4 target\nline 5\n",
        encoding="utf-8",
    )
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call(
            "read_file",
            {"path": "module.py", "line_start": 3, "line_count": 2},
        ),
        executor=tool_executor,
    )

    assert result["ok"] is True
    assert result["content"]["content"] == "line 3 target\nline 4 target"
    assert result["content"]["line_start"] == 3
    assert result["content"]["line_end"] == 4
    assert result["content"]["line_count"] == 2
    assert result["content"]["total_lines"] == 5
    assert result["content"]["has_more_lines"] is True
    assert "line 2" not in result["content"]["content"]
    assert "line 5" not in result["content"]["content"]
    assert result["truncated"] is False


def test_read_file_line_window_reports_file_end_without_output_truncation(
    tmp_path: Path,
) -> None:
    source = tmp_path / "module.py"
    source.write_text("line 1\nline 2\nline 3\n", encoding="utf-8")
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call(
            "read_file",
            {"path": "module.py", "line_start": 2, "line_count": 10},
        ),
        executor=tool_executor,
    )

    assert result["ok"] is True
    assert result["content"]["content"] == "line 2\nline 3"
    assert result["content"]["line_start"] == 2
    assert result["content"]["line_end"] == 3
    assert result["content"]["line_count"] == 2
    assert result["content"]["total_lines"] == 3
    assert result["content"]["has_more_lines"] is False
    assert result["content"]["truncated"] is False


def test_read_file_line_window_can_read_past_prefix_limit(tmp_path: Path) -> None:
    source = tmp_path / "module.py"
    source.write_text(
        "".join(f"line {line}\n" for line in range(1, 101)),
        encoding="utf-8",
    )
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call(
            "read_file",
            {
                "path": "module.py",
                "line_start": 80,
                "line_count": 2,
                "max_bytes": 20,
            },
        ),
        executor=tool_executor,
    )

    assert result["ok"] is True
    assert result["content"]["content"] == "line 80\nline 81"
    assert result["content"]["line_start"] == 80
    assert result["content"]["line_end"] == 81
    assert result["content"]["total_lines"] == 100


def test_read_file_rejects_path_escape(tmp_path: Path) -> None:
    outside = tmp_path.parent / "outside.txt"
    outside.write_text("outside", encoding="utf-8")
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("read_file", {"path": "../outside.txt"}),
        executor=tool_executor,
    )

    assert result["ok"] is False
    assert result["error_code"] == "validation_error"
    assert "escapes project root" in result["error"]["message"]


def test_dispatch_returns_error_for_invalid_json(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_raw_tool_call("read_file", "{not json"),
        executor=tool_executor,
    )

    assert result["ok"] is False
    assert result["error_code"] == "bad_arguments_json"
    assert "Invalid JSON arguments" in result["error"]["message"]


def test_dispatch_returns_error_for_unknown_tool(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    tool_executor = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("missing_tool", {"path": "README.md"}),
        executor=tool_executor,
    )

    assert result["ok"] is False
    assert result["error_code"] == "tool_not_found"
    assert result["error"]["message"] == "Unknown tool: missing_tool"
