import json
from pathlib import Path
from typing import Any

from pydantic import BaseModel, ConfigDict

from singularity.tools import (
    ToolCachePolicy,
    ToolPolicy,
    ToolRegistry,
    ToolExecutor,
    ToolSpec,
)
from singularity.policy import ResourceRef
from singularity.jsonl_trace import JsonlTraceRecorder
from tests.tool_executor_helpers import make_test_policy_engine


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


def make_tool_call(
    name: str,
    arguments: dict[str, Any] | None = None,
    *,
    tool_call_id: str | None = None,
) -> dict[str, Any]:
    return {
        "id": tool_call_id or f"call_{name}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments or {})},
    }


def test_read_file_cache_invalidates_when_file_changes(tmp_path: Path) -> None:
    path = tmp_path / "README.md"
    path.write_text("first", encoding="utf-8")
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md"}, tool_call_id="call_read_first")
    )
    path.write_text("second", encoding="utf-8")
    second = component.execute_tool_call(
        make_tool_call("read_file", {"path": "README.md"}, tool_call_id="call_read_second")
    )

    assert first.content["content"] == "first"
    assert second.content["content"] == "second"
    assert second.metadata["cache_hit"] is False


def test_sensitive_result_is_not_cached(tmp_path: Path) -> None:
    calls: list[int] = []

    def handler(_args: EmptyInput) -> dict[str, str]:
        calls.append(1)
        return {"token": "secret-token"}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="secretish",
            description="secretish",
            input_model=EmptyInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
            sensitivity="secret",
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("secretish", tool_call_id="call_secret_1"))
    component.execute_tool_call(make_tool_call("secretish", tool_call_id="call_secret_2"))

    assert len(calls) == 2


def test_idempotent_false_is_not_cached(tmp_path: Path) -> None:
    calls: list[int] = []

    def handler(_args: EmptyInput) -> dict[str, int]:
        calls.append(1)
        return {"count": len(calls)}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="non_idempotent",
            description="non idempotent",
            input_model=EmptyInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
            idempotent=False,
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("non_idempotent", tool_call_id="call_non_idem_1"))
    second = component.execute_tool_call(make_tool_call("non_idempotent", tool_call_id="call_non_idem_2"))

    assert second.content["count"] == 2
    assert second.metadata["cache_hit"] is False


def test_bounded_lru_evicts_old_entries(tmp_path: Path) -> None:
    calls: list[str] = []

    class ValueInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        value: str

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="bounded",
            description="bounded",
            input_model=ValueInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True, max_entries=2),
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    for index, value in enumerate(["a", "b", "c", "a"], start=1):
        component.execute_tool_call(
            make_tool_call("bounded", {"value": value}, tool_call_id=f"call_bounded_{index}")
        )

    assert calls == ["a", "b", "c", "a"]


def test_cache_can_be_invalidated_by_path(tmp_path: Path) -> None:
    path = tmp_path / "a.txt"
    path.write_text("first", encoding="utf-8")
    component = ToolExecutor(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )
    component.execute_tool_call(
        make_tool_call("read_file", {"path": "a.txt"}, tool_call_id="call_cache_path_1")
    )
    path.write_text("second", encoding="utf-8")

    component.invalidate_paths(["a.txt"])
    result = component.execute_tool_call(
        make_tool_call("read_file", {"path": "a.txt"}, tool_call_id="call_cache_path_2")
    )

    assert result.content["content"] == "second"
    assert result.metadata["cache_hit"] is False


def test_file_invalidation_evicts_parent_directory_cache_entry(tmp_path: Path) -> None:
    calls: list[int] = []

    class DirInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        path: str

    def handler(args: DirInput) -> dict[str, int]:
        calls.append(1)
        return {"count": len(calls)}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="scan_dir",
            description="scan dir",
            input_model=DirInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
            resource_resolver=lambda args, _root: [
                ResourceRef("directory", args["path"], workspace_relative=True)
            ],
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    component.execute_tool_call(make_tool_call("scan_dir", {"path": "src"}, tool_call_id="call_scan_1"))
    component.invalidate_paths(["src/app.py"])
    result = component.execute_tool_call(
        make_tool_call("scan_dir", {"path": "src"}, tool_call_id="call_scan_2")
    )

    assert result.content["count"] == 2
    assert result.metadata["cache_hit"] is False


def test_cacheable_tool_call_id_conflict_is_rejected(tmp_path: Path) -> None:
    calls: list[str] = []

    class ValueInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        value: str

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="cached_echo",
            description="cached echo",
            input_model=ValueInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(make_tool_call("cached_echo", {"value": "x"}))
    second = component.execute_tool_call(make_tool_call("cached_echo", {"value": "y"}))

    assert first.ok is True
    assert second.ok is False
    assert second.error_code == "conflicting_replay"
    assert calls == ["x"]


def test_cache_hit_still_records_tool_call_id_for_conflict_detection(tmp_path: Path) -> None:
    calls: list[str] = []

    class ValueInput(BaseModel):
        model_config = ConfigDict(extra="forbid")
        value: str

    def handler(args: ValueInput) -> dict[str, str]:
        calls.append(args.value)
        return {"value": args.value}

    registry = ToolRegistry(tmp_path, include_default_tools=False)
    registry.register(
        ToolSpec(
            name="cached_echo",
            description="cached echo",
            input_model=ValueInput,
            handler=handler,
            cache_policy=ToolCachePolicy(cacheable=True),
        )
    )
    component = ToolExecutor(
        registry=registry,
        policy=ToolPolicy.read_only(),
        trace=JsonlTraceRecorder.create(tmp_path),
        workspace_root=tmp_path,
        policy_engine=make_test_policy_engine(tmp_path),
    )

    first = component.execute_tool_call(
        make_tool_call("cached_echo", {"value": "x"}, tool_call_id="call_first")
    )
    cached = component.execute_tool_call(
        make_tool_call("cached_echo", {"value": "x"}, tool_call_id="call_cached")
    )
    conflict = component.execute_tool_call(
        make_tool_call("cached_echo", {"value": "y"}, tool_call_id="call_cached")
    )

    assert first.ok is True
    assert cached.ok is True
    assert cached.metadata["cache_hit"] is True
    assert conflict.ok is False
    assert conflict.error_code == "conflicting_replay"
    assert calls == ["x"]

