import json
from pathlib import Path

from miniharness.tools import ToolRegistry


def make_tool_call(name: str, arguments: dict) -> dict:
    return {"function": {"name": name, "arguments": json.dumps(arguments)}}


def test_openai_tools_schema_contains_read_only_tools(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)

    schemas = registry.openai_tools()

    names = {tool["function"]["name"] for tool in schemas}
    assert names == {"list_files", "read_file", "search_text"}
    for tool in schemas:
        assert tool["type"] == "function"
        assert "description" in tool["function"]
        assert "parameters" in tool["function"]


def test_read_file_reads_project_file(tmp_path: Path) -> None:
    readme = tmp_path / "README.md"
    readme.write_text("hello from miniharness", encoding="utf-8")
    registry = ToolRegistry(tmp_path)

    result = registry.dispatch(
        make_tool_call("read_file", {"path": "README.md", "max_bytes": 100})
    )

    assert result["ok"] is True
    assert result["path"] == "README.md"
    assert result["content"] == "hello from miniharness"
    assert result["truncated"] is False


def test_read_file_rejects_path_escape(tmp_path: Path) -> None:
    outside = tmp_path.parent / "outside.txt"
    outside.write_text("outside", encoding="utf-8")
    registry = ToolRegistry(tmp_path)

    result = registry.dispatch(
        make_tool_call("read_file", {"path": "../outside.txt"})
    )

    assert result["ok"] is False
    assert "escapes project root" in result["error"]
