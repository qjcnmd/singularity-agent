from pathlib import Path

from singularity.model import (
    ModelToolRenderer,
    ModelToolParseStatus,
    ToolCallNormalizer,
)
from singularity.tools import ToolRegistry
from singularity.tools.read_only import register_read_only_tools


def test_tool_renderer_filters_hashes_and_normalizes_tool_calls(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    renderer = ModelToolRenderer(registry)

    rendered = renderer.render(allowed_tool_names=["read_file"])

    assert [tool.name for tool in rendered] == ["read_file"]
    assert renderer.schema_hash(rendered) == renderer.schema_hash(rendered)

    normalizer = ToolCallNormalizer(registry)
    valid = normalizer.normalize(
        {
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": {"path": "README.md"}},
        },
        allowed_tool_names=["read_file"],
    )
    assert valid.parse_status == ModelToolParseStatus.VALID
    assert valid.raw_arguments == '{"path": "README.md"}'

    invalid = normalizer.normalize(
        {
            "id": "call_2",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{bad"},
        },
        allowed_tool_names=["read_file"],
    )
    assert invalid.parse_status == ModelToolParseStatus.INVALID_JSON

    unknown = normalizer.normalize(
        {
            "id": "call_3",
            "type": "function",
            "function": {"name": "missing", "arguments": "{}"},
        },
        allowed_tool_names=["read_file"],
    )
    assert unknown.parse_status == ModelToolParseStatus.UNKNOWN_TOOL


def test_empty_allowed_tool_list_exposes_no_tools(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path)
    renderer = ModelToolRenderer(registry)

    assert renderer.render(allowed_tool_names=[]) == []

    normalizer = ToolCallNormalizer(registry)
    denied = normalizer.normalize(
        {
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": {"path": "README.md"}},
        },
        allowed_tool_names=[],
    )
    assert denied.parse_status == ModelToolParseStatus.UNKNOWN_TOOL


def test_tool_renderer_orders_schemas_by_name_for_stable_prefix(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_read_only_tools(registry)
    renderer = ModelToolRenderer(registry)

    rendered = renderer.render(allowed_tool_names=["search_text", "read_file"])

    assert [tool.name for tool in rendered] == ["read_file", "search_text"]
