from pathlib import Path

from pydantic import BaseModel, ConfigDict

from singularity.model import (
    ModelToolParseStatus,
    ModelToolRenderer,
    ToolCallNormalizer,
)
from singularity.tools import ToolRegistry
from singularity.tools.models import PermissionLevel, ToolSpec
from singularity.tools.read_only import register_read_only_tools


class ListInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    expected_files: list[str]
    content: str


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


def test_tool_call_normalizer_coerces_json_string_for_list_fields_only(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="list_input",
            version="test",
            description="list input",
            input_model=ListInput,
            handler=lambda _args: {},
            permission_level=PermissionLevel.WRITE,
            uses_mutation_manager=True,
        )
    )

    normalized = ToolCallNormalizer(registry).normalize(
        {
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "list_input",
                "arguments": {
                    "expected_files": '["close_elements.py"]',
                    "content": '{"still":"a string"}',
                },
            },
        },
        allowed_tool_names=["list_input"],
    )

    assert normalized.parse_status == ModelToolParseStatus.VALID
    assert normalized.arguments == {
        "expected_files": ["close_elements.py"],
        "content": '{"still":"a string"}',
    }


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


def test_disabled_tool_is_not_rendered_or_normalized(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="disabled_tool",
            description="disabled",
            input_model=ListInput,
            handler=lambda _args: {},
            enabled=False,
        )
    )

    renderer = ModelToolRenderer(registry)

    assert renderer.render() == []
    assert registry.to_openai_tools() == []

    denied = ToolCallNormalizer(registry).normalize(
        {
            "id": "call_disabled",
            "type": "function",
            "function": {"name": "disabled_tool", "arguments": {"expected_files": [], "content": ""}},
        },
        allowed_tool_names=["disabled_tool"],
    )

    assert denied.parse_status == ModelToolParseStatus.UNKNOWN_TOOL


def test_tool_renderer_orders_schemas_by_name_for_stable_prefix(tmp_path: Path) -> None:
    registry = ToolRegistry(tmp_path, include_default_tools=False)
    register_read_only_tools(registry)
    renderer = ModelToolRenderer(registry)

    rendered = renderer.render(allowed_tool_names=["search_text", "read_file"])

    assert [tool.name for tool in rendered] == ["read_file", "search_text"]
