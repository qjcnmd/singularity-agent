import json
from pathlib import Path

from singularity.tools import ToolPolicy, ToolRegistry, ToolRuntime
from singularity.trace import TraceWriter
from tests.tool_runtime_helpers import make_test_policy_runtime


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
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 100}),
        runtime=runtime,
    )

    assert result["ok"] is True
    assert result["content"]["path"] == "README.md"
    assert result["content"]["content"] == "hello from singularity"
    assert result["truncated"] is False


def test_read_file_rejects_path_escape(tmp_path: Path) -> None:
    outside = tmp_path.parent / "outside.txt"
    outside.write_text("outside", encoding="utf-8")
    registry = ToolRegistry(tmp_path)
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("read_file", {"path": "../outside.txt"}),
        runtime=runtime,
    )

    assert result["ok"] is False
    assert result["error_code"] == "validation_error"
    assert "escapes project root" in result["error"]["message"]


def test_dispatch_returns_error_for_invalid_json(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_raw_tool_call("read_file", "{not json"),
        runtime=runtime,
    )

    assert result["ok"] is False
    assert result["error_code"] == "bad_arguments_json"
    assert "Invalid JSON arguments" in result["error"]["message"]


def test_dispatch_returns_error_for_unknown_tool(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    runtime = ToolRuntime(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = registry.dispatch_for_tests(
        make_tool_call("missing_tool", {"path": "README.md"}),
        runtime=runtime,
    )

    assert result["ok"] is False
    assert result["error_code"] == "tool_not_found"
    assert result["error"]["message"] == "Unknown tool: missing_tool"
